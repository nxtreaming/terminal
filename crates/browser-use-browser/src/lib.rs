//! Rust-owned browser control plane for browser-use terminal.
//!
//! The LLM-facing split is intentional:
//! - `browser` controls connection/lifecycle/debug state.
//! - `browser_script` runs fresh Python for page interaction through this
//!   Rust-held CDP connection.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tempfile::TempDir;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

const BU_API: &str = "https://api.browser-use.com/api/v3";
const LOG_LIMIT: usize = 250;
const SCRIPT_MAX_OUTPUT_CHARS: usize = 120_000;

#[derive(Debug)]
pub struct BrowserCommandOutput {
    pub content: Value,
    pub events: Vec<Value>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct BrowserScriptOutput {
    pub ok: bool,
    pub text: String,
    pub error: Option<String>,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub outputs: Vec<Value>,
    #[serde(default)]
    pub artifacts: Vec<Value>,
    #[serde(default)]
    pub images: Vec<Value>,
    #[serde(default)]
    pub browser_events: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BrowserMode {
    None,
    Local,
    Managed,
    RemoteCdp,
    RemoteCloud,
}

impl BrowserMode {
    fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Local => "local",
            Self::Managed => "managed",
            Self::RemoteCdp => "remote-cdp",
            Self::RemoteCloud => "remote-cloud",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BrowserOwner {
    None,
    External,
    Rust,
}

impl BrowserOwner {
    fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::External => "external",
            Self::Rust => "rust",
        }
    }
}

#[derive(Debug, Clone)]
struct Endpoint {
    kind: String,
    http_url: Option<String>,
    ws_url: String,
    candidate_id: Option<String>,
}

struct CdpConnection {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

#[derive(Debug, Clone)]
struct ManagedLaunch {
    executable: String,
    profile: ManagedProfile,
    headless: bool,
    extra_args: Vec<String>,
}

#[derive(Debug, Clone)]
enum ManagedProfile {
    Temp,
    Path(PathBuf),
}

struct ManagedBrowser {
    child: Child,
    _profile_dir: Option<TempDir>,
    launch: ManagedLaunch,
}

struct BrowserSession {
    mode: BrowserMode,
    owner: BrowserOwner,
    endpoint: Option<Endpoint>,
    connection: Option<CdpConnection>,
    current_target_id: Option<String>,
    current_session_id: Option<String>,
    connection_generation: u64,
    managed: Option<ManagedBrowser>,
    remote_browser_id: Option<String>,
    live_url: Option<String>,
    browser_name: Option<String>,
    profile: Option<String>,
    last_error: Option<String>,
    logs: VecDeque<String>,
}

impl Default for BrowserSession {
    fn default() -> Self {
        Self {
            mode: BrowserMode::None,
            owner: BrowserOwner::None,
            endpoint: None,
            connection: None,
            current_target_id: None,
            current_session_id: None,
            connection_generation: 0,
            managed: None,
            remote_browser_id: None,
            live_url: None,
            browser_name: None,
            profile: None,
            last_error: None,
            logs: VecDeque::new(),
        }
    }
}

static SESSIONS: OnceLock<Mutex<HashMap<String, BrowserSession>>> = OnceLock::new();

pub fn run_browser_command(
    session_id: &str,
    cwd: impl AsRef<Path>,
    artifact_dir: impl AsRef<Path>,
    raw_cmd: &str,
) -> Result<BrowserCommandOutput> {
    let mut argv = shell_words(raw_cmd)?;
    if argv.first().is_some_and(|arg| arg == "browser") {
        argv.remove(0);
    }
    if argv.is_empty() {
        argv.push("help".to_string());
    }

    let mut sessions = sessions()
        .lock()
        .expect("browser session registry poisoned");
    let session = sessions.entry(session_id.to_string()).or_default();
    session.log(format!("browser {}", argv.join(" ")));
    let content = dispatch_browser_command(session, cwd.as_ref(), artifact_dir.as_ref(), &argv)?;
    Ok(BrowserCommandOutput {
        events: session.browser_events(),
        content,
    })
}

pub fn run_browser_script(
    session_id: &str,
    cwd: impl AsRef<Path>,
    artifact_dir: impl AsRef<Path>,
    code: &str,
    timeout_seconds: u64,
) -> Result<BrowserScriptOutput> {
    fs::create_dir_all(artifact_dir.as_ref())
        .with_context(|| format!("create artifact dir {}", artifact_dir.as_ref().display()))?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("bind browser_script bridge")?;
    let bridge_addr = listener.local_addr()?;
    listener
        .set_nonblocking(true)
        .context("set browser_script bridge nonblocking")?;
    let stop = Arc::new(AtomicBool::new(false));
    let bridge_stop = stop.clone();
    let bridge_session_id = session_id.to_string();
    let bridge = thread::spawn(move || run_bridge(listener, bridge_session_id, bridge_stop));

    let prelude = browser_script_prelude(
        bridge_addr.port(),
        cwd.as_ref(),
        artifact_dir.as_ref(),
        code,
    )?;
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(prelude)
        .current_dir(cwd.as_ref())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn browser_script python3")?;

    let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(1));
    let mut timed_out = false;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let output = child
        .wait_with_output()
        .context("wait for browser_script python3")?;
    stop.store(true, Ordering::SeqCst);
    let _ = bridge.join();

    if timed_out {
        return Ok(BrowserScriptOutput {
            ok: false,
            text: String::new(),
            error: Some(format!(
                "browser_script timed out after {timeout_seconds} seconds"
            )),
            ..Default::default()
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let marker = "__BROWSER_SCRIPT_RESULT__";
    let result_line = stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(marker))
        .map(str::trim);
    let Some(result_line) = result_line else {
        return Ok(BrowserScriptOutput {
            ok: false,
            text: truncate_text(&stdout, SCRIPT_MAX_OUTPUT_CHARS),
            error: Some(if stderr.trim().is_empty() {
                "browser_script did not emit a result".to_string()
            } else {
                stderr
            }),
            ..Default::default()
        });
    };
    let mut response: BrowserScriptOutput =
        serde_json::from_str(result_line).context("parse browser_script result")?;
    if !stderr.trim().is_empty() && response.error.is_none() && !response.ok {
        response.error = Some(stderr);
    }
    Ok(response)
}

pub fn cleanup_session(session_id: &str) -> usize {
    let mut sessions = sessions()
        .lock()
        .expect("browser session registry poisoned");
    if let Some(mut session) = sessions.remove(session_id) {
        session.stop_owned_managed();
        if session.owner == BrowserOwner::Rust && session.mode == BrowserMode::RemoteCloud {
            let _ = session.stop_owned_remote();
        }
        1
    } else {
        0
    }
}

fn sessions() -> &'static Mutex<HashMap<String, BrowserSession>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn dispatch_browser_command(
    session: &mut BrowserSession,
    cwd: &Path,
    artifact_dir: &Path,
    argv: &[String],
) -> Result<Value> {
    match argv.first().map(String::as_str).unwrap_or("help") {
        "help" | "--help" | "-h" => Ok(Value::String(browser_help().to_string())),
        "status" => Ok(session.status_json()),
        "doctor" => {
            let doctor = session.doctor(cwd)?;
            if has_flag(argv, "--json") {
                Ok(doctor)
            } else {
                Ok(Value::String(render_doctor(&doctor)))
            }
        }
        "connect" => dispatch_connect(session, argv),
        "local" => dispatch_local(session, argv, artifact_dir),
        "remote" => dispatch_remote(session, argv),
        "recover" => dispatch_recover(session, argv),
        "runtime" => dispatch_runtime(session, argv),
        other => bail!("unknown browser command: {other}. Run `browser help`."),
    }
}

fn dispatch_connect(session: &mut BrowserSession, argv: &[String]) -> Result<Value> {
    match argv.get(1).map(String::as_str) {
        Some("local") => {
            let candidate_id = option_value(argv, "--candidate");
            session.connect_local(candidate_id)
        }
        Some("managed") => {
            let headless = if has_flag(argv, "--headed") {
                false
            } else {
                !has_flag(argv, "--headful")
            };
            let profile = match option_value(argv, "--profile").as_deref() {
                None | Some("temp") => ManagedProfile::Temp,
                Some(path) => ManagedProfile::Path(PathBuf::from(path)),
            };
            let extra_args = option_values(argv, "--arg");
            session.connect_managed(headless, profile, extra_args)
        }
        Some("remote-cdp") => {
            if let Some(url) = option_value(argv, "--url") {
                session.connect_remote_http(url)
            } else if let Some(ws) = option_value(argv, "--ws") {
                session.connect_remote_ws(ws)
            } else {
                bail!("connect remote-cdp requires --url <http-url> or --ws <ws-url>");
            }
        }
        Some(other) => bail!("unknown browser connect mode: {other}"),
        None => bail!("browser connect requires local, managed, or remote-cdp"),
    }
}

fn dispatch_local(
    _session: &mut BrowserSession,
    argv: &[String],
    _artifact_dir: &Path,
) -> Result<Value> {
    match argv.get(1).map(String::as_str) {
        Some("list") => Ok(json!({ "candidates": local_candidates() })),
        Some("setup") => {
            let url = "chrome://inspect/#remote-debugging";
            let opened = open::that(url).is_ok();
            Ok(json!({
                "status": "needs-user-action",
                "opened": opened,
                "url": url,
                "instructions": [
                    "In the browser that opens, enable 'Allow remote debugging for this browser instance'.",
                    "If Chrome shows an additional permission prompt, click Allow.",
                    "Then run `browser connect local` again."
                ],
                "next_step": "browser connect local"
            }))
        }
        Some("profiles") => dispatch_local_profiles(argv),
        Some(other) => bail!("unknown browser local command: {other}"),
        None => bail!("browser local requires list, setup, or profiles"),
    }
}

fn dispatch_local_profiles(argv: &[String]) -> Result<Value> {
    if argv.get(2).map(String::as_str) == Some("inspect") {
        let profile = argv
            .get(3)
            .map(String::as_str)
            .ok_or_else(|| anyhow!("local profiles inspect requires <profile-name>"))?;
        return inspect_local_profile(profile, has_flag(argv, "--domains-only"));
    }
    list_local_profiles()
}

fn dispatch_remote(session: &mut BrowserSession, argv: &[String]) -> Result<Value> {
    match argv.get(1).map(String::as_str) {
        Some("start") => session.start_remote_cloud(argv),
        Some("stop") => session.stop_owned_remote(),
        Some("status") => Ok(session.status_json()),
        Some("live-url") => Ok(json!({ "live_url": session.live_url })),
        Some("profiles") => list_cloud_profiles(),
        Some(other) => bail!("unknown browser remote command: {other}"),
        None => bail!("browser remote requires start, stop, status, live-url, or profiles"),
    }
}

fn dispatch_recover(session: &mut BrowserSession, argv: &[String]) -> Result<Value> {
    match argv.get(1).map(String::as_str) {
        Some("reconnect-websocket") => session.reconnect_websocket(),
        Some("reattach-same-target") => session.reattach_same_target(),
        Some("restart-runtime") => session.restart_runtime(),
        Some("restart-owned-browser") => session.restart_owned_browser(),
        Some("stop-owned-remote") => session.stop_owned_remote(),
        Some(other) => bail!("unknown browser recover command: {other}"),
        None => bail!("browser recover requires a recovery action"),
    }
}

fn dispatch_runtime(session: &mut BrowserSession, argv: &[String]) -> Result<Value> {
    match argv.get(1).map(String::as_str) {
        Some("logs") => Ok(Value::String(
            session.logs.iter().cloned().collect::<Vec<_>>().join("\n"),
        )),
        Some("ownership") => Ok(session.ownership_json()),
        Some("cleanup-stale") => Ok(json!({
            "status": "ok",
            "cleaned": 0,
            "note": "No stale runtime files were removed. Rust browser state is in-process for this session.",
        })),
        Some(other) => bail!("unknown browser runtime command: {other}"),
        None => bail!("browser runtime requires logs, ownership, or cleanup-stale"),
    }
}

impl BrowserSession {
    fn log(&mut self, message: impl Into<String>) {
        let message = message.into();
        if self.logs.len() >= LOG_LIMIT {
            self.logs.pop_front();
        }
        self.logs
            .push_back(format!("[{}] {message}", unix_time_ms()));
    }

    fn browser_events(&self) -> Vec<Value> {
        let mut events = Vec::new();
        match self.mode {
            BrowserMode::None => {}
            _ => {
                events.push(json!({
                    "type": if self.connection.is_some() { "browser.connected" } else { "browser.disconnected" },
                    "payload": self.browser_event_payload(),
                }));
                if self.live_url.is_some() {
                    events.push(json!({
                        "type": "browser.live_url",
                        "payload": { "url": self.live_url },
                    }));
                }
            }
        }
        events
    }

    fn browser_event_payload(&self) -> Value {
        json!({
            "backend": self.mode.as_str(),
            "status": if self.connection.is_some() { "connected" } else { "disconnected" },
            "target_id": self.current_target_id,
            "session_id": self.current_session_id,
            "generation": self.connection_generation,
            "live_url": self.live_url,
        })
    }

    fn status_json(&self) -> Value {
        let connected = self.connection.is_some();
        let page = json!({
            "target_id": self.current_target_id,
            "session_id": self.current_session_id,
        });
        json!({
            "mode": self.mode.as_str(),
            "connection": if connected { "connected" } else if self.endpoint.is_some() { "disconnected" } else { "not-configured" },
            "reason": self.last_error,
            "next_step": self.next_step(),
            "owner": self.owner.as_str(),
            "browser": self.browser_name,
            "profile": self.profile,
            "endpoint": self.endpoint.as_ref().map(|endpoint| json!({
                "kind": endpoint.kind,
                "http_url": endpoint.http_url,
                "ws_url": redact_ws_url(&endpoint.ws_url),
                "candidate_id": endpoint.candidate_id,
            })),
            "page": page,
            "safety": {
                "can_restart_browser": self.owner == BrowserOwner::Rust && self.mode == BrowserMode::Managed,
                "can_close_browser": self.owner == BrowserOwner::Rust && self.mode == BrowserMode::Managed,
                "can_stop_remote": self.owner == BrowserOwner::Rust && self.mode == BrowserMode::RemoteCloud && self.remote_browser_id.is_some(),
            },
            "connection_generation": self.connection_generation,
            "remote_browser_id": self.remote_browser_id,
            "live_url": self.live_url,
        })
    }

    fn ownership_json(&self) -> Value {
        json!({
            "owner": self.owner.as_str(),
            "mode": self.mode.as_str(),
            "endpoint": self.endpoint.as_ref().map(|endpoint| json!({
                "kind": endpoint.kind,
                "http_url": endpoint.http_url,
                "ws_url": redact_ws_url(&endpoint.ws_url),
                "candidate_id": endpoint.candidate_id,
            })),
            "managed_pid": self.managed.as_ref().map(|managed| managed.child.id()),
            "remote_browser_id": self.remote_browser_id,
            "target_id": self.current_target_id,
            "session_id": self.current_session_id,
            "connection_generation": self.connection_generation,
            "safe_actions": {
                "restart_runtime": self.endpoint.is_some(),
                "restart_owned_browser": self.owner == BrowserOwner::Rust && self.mode == BrowserMode::Managed,
                "stop_owned_remote": self.owner == BrowserOwner::Rust && self.mode == BrowserMode::RemoteCloud && self.remote_browser_id.is_some(),
            }
        })
    }

    fn next_step(&self) -> Option<&'static str> {
        if self.endpoint.is_none() {
            Some("browser connect local")
        } else if self.connection.is_none() {
            Some("browser recover reconnect-websocket")
        } else if self.current_target_id.is_some() && self.current_session_id.is_none() {
            Some("browser recover reattach-same-target")
        } else {
            None
        }
    }

    fn connect_local(&mut self, candidate_id: Option<String>) -> Result<Value> {
        let candidates = local_candidates();
        if candidates.is_empty() {
            self.last_error =
                Some("No local remote-debugging browser candidates found".to_string());
            return Ok(json!({
                "status": "blocked",
                "reason": "No running Chromium-family browser with DevToolsActivePort was found.",
                "next_step": "browser local setup",
            }));
        }
        let candidate = if let Some(candidate_id) = candidate_id {
            candidates
                .into_iter()
                .find(|candidate| candidate.id == candidate_id)
                .ok_or_else(|| anyhow!("unknown local candidate id: {candidate_id}"))?
        } else if candidates.len() == 1 {
            candidates.into_iter().next().expect("one candidate")
        } else {
            return Ok(json!({
                "status": "needs-user-action",
                "reason": "Multiple local browser candidates are available. Ask the user which browser/profile to attach.",
                "candidates": candidates,
                "next_step": "browser connect local --candidate <id>",
            }));
        };
        self.stop_owned_managed();
        let endpoint = Endpoint {
            kind: "devtools-active-port".to_string(),
            http_url: candidate.http_url.clone(),
            ws_url: candidate.ws_url.clone(),
            candidate_id: Some(candidate.id.clone()),
        };
        self.connect_endpoint(endpoint, BrowserMode::Local, BrowserOwner::External)?;
        self.browser_name = Some(candidate.browser_name.clone());
        self.profile = Some(candidate.profile_path.display().to_string());
        Ok(json!({
            "status": "connected",
            "candidate": candidate,
            "browser": self.status_json(),
        }))
    }

    fn connect_remote_http(&mut self, http_url: String) -> Result<Value> {
        let ws_url = resolve_ws_from_http(&http_url)?;
        self.stop_owned_managed();
        self.connect_endpoint(
            Endpoint {
                kind: "cdp-url".to_string(),
                http_url: Some(http_url),
                ws_url,
                candidate_id: None,
            },
            BrowserMode::RemoteCdp,
            BrowserOwner::External,
        )?;
        Ok(json!({ "status": "connected", "browser": self.status_json() }))
    }

    fn connect_remote_ws(&mut self, ws_url: String) -> Result<Value> {
        self.stop_owned_managed();
        self.connect_endpoint(
            Endpoint {
                kind: "cdp-ws".to_string(),
                http_url: None,
                ws_url,
                candidate_id: None,
            },
            BrowserMode::RemoteCdp,
            BrowserOwner::External,
        )?;
        Ok(json!({ "status": "connected", "browser": self.status_json() }))
    }

    fn connect_managed(
        &mut self,
        headless: bool,
        profile: ManagedProfile,
        extra_args: Vec<String>,
    ) -> Result<Value> {
        self.stop_owned_managed();
        let mut launch_errors = Vec::new();
        let mut launched = None;
        for executable in chromium_candidate_paths(headless) {
            let launch = ManagedLaunch {
                executable,
                profile: profile.clone(),
                headless,
                extra_args: extra_args.clone(),
            };
            match launch_managed_browser(launch.clone()) {
                Ok((managed, http_url)) => {
                    launched = Some((launch, managed, http_url));
                    break;
                }
                Err(error) => {
                    launch_errors.push(format!("{}: {error:#}", launch.executable));
                }
            }
        }
        let Some((launch, managed, http_url)) = launched else {
            if launch_errors.is_empty() {
                bail!(
                    "No Chromium executable found. Set CHROME_PATH or install Playwright Chromium."
                );
            }
            bail!(
                "No Chromium executable successfully exposed DevTools:\n{}",
                launch_errors.join("\n")
            );
        };
        let ws_url = resolve_ws_from_http(&http_url)?;
        self.managed = Some(managed);
        self.connect_endpoint(
            Endpoint {
                kind: "cdp-url".to_string(),
                http_url: Some(http_url),
                ws_url,
                candidate_id: None,
            },
            BrowserMode::Managed,
            BrowserOwner::Rust,
        )?;
        self.browser_name = Some("Managed Chromium".to_string());
        self.profile = Some(match &launch.profile {
            ManagedProfile::Temp => "temp".to_string(),
            ManagedProfile::Path(path) => path.display().to_string(),
        });
        Ok(json!({ "status": "connected", "browser": self.status_json() }))
    }

    fn start_remote_cloud(&mut self, argv: &[String]) -> Result<Value> {
        let mut body = serde_json::Map::new();
        if let Some(profile_id) = option_value(argv, "--profile-id") {
            body.insert("profileId".to_string(), Value::String(profile_id));
        }
        if let Some(profile_name) = option_value(argv, "--profile-name") {
            if body.contains_key("profileId") {
                bail!("pass --profile-id or --profile-name, not both");
            }
            let profile_id = resolve_cloud_profile_name(&profile_name)?;
            body.insert("profileId".to_string(), Value::String(profile_id));
        }
        if let Some(timeout) = option_value(argv, "--timeout") {
            let timeout: i64 = timeout
                .parse()
                .with_context(|| format!("invalid --timeout value: {timeout}"))?;
            body.insert("timeout".to_string(), Value::Number(timeout.into()));
        }
        if let Some(country) = option_value(argv, "--proxy-country") {
            if country.eq_ignore_ascii_case("none") {
                body.insert("proxyCountryCode".to_string(), Value::Null);
            } else {
                body.insert("proxyCountryCode".to_string(), Value::String(country));
            }
        }
        let browser = browser_use_api("/browsers", "POST", Some(Value::Object(body)))?;
        let id = browser
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Browser Use API response missing browser id"))?
            .to_string();
        let cdp_url = browser
            .get("cdpUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Browser Use API response missing cdpUrl"))?
            .to_string();
        let ws_url = match resolve_ws_from_http(&cdp_url) {
            Ok(ws_url) => ws_url,
            Err(error) => {
                let _ = stop_cloud_browser(&id);
                return Err(error);
            }
        };
        self.stop_owned_managed();
        self.connect_endpoint(
            Endpoint {
                kind: "browser-use-cloud".to_string(),
                http_url: Some(cdp_url),
                ws_url,
                candidate_id: None,
            },
            BrowserMode::RemoteCloud,
            BrowserOwner::Rust,
        )?;
        self.remote_browser_id = Some(id);
        self.live_url = browser
            .get("liveUrl")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        self.browser_name = Some("Browser Use cloud".to_string());
        Ok(json!({
            "status": "connected",
            "remote_browser": browser,
            "browser": self.status_json(),
            "live_url": self.live_url,
        }))
    }

    fn stop_owned_remote(&mut self) -> Result<Value> {
        if !(self.owner == BrowserOwner::Rust && self.mode == BrowserMode::RemoteCloud) {
            return Ok(json!({
                "stopped": false,
                "reason": "current browser is not a Rust-owned Browser Use cloud browser",
            }));
        }
        let Some(id) = self.remote_browser_id.clone() else {
            return Ok(json!({ "stopped": false, "reason": "missing remote browser id" }));
        };
        stop_cloud_browser(&id)?;
        self.connection = None;
        self.endpoint = None;
        self.current_session_id = None;
        self.current_target_id = None;
        self.remote_browser_id = None;
        self.live_url = None;
        self.mode = BrowserMode::None;
        self.owner = BrowserOwner::None;
        self.connection_generation += 1;
        Ok(json!({ "stopped": true, "browser_id": id }))
    }

    fn connect_endpoint(
        &mut self,
        endpoint: Endpoint,
        mode: BrowserMode,
        owner: BrowserOwner,
    ) -> Result<()> {
        let ws_url = endpoint.ws_url.clone();
        let connection = CdpConnection::connect(&ws_url)?;
        self.endpoint = Some(endpoint);
        self.connection = Some(connection);
        self.mode = mode;
        self.owner = owner;
        self.connection_generation += 1;
        self.last_error = None;
        self.attach_first_page()?;
        Ok(())
    }

    fn reconnect_websocket(&mut self) -> Result<Value> {
        let Some(endpoint) = self.endpoint.clone() else {
            bail!("no browser endpoint is configured");
        };
        self.connection = Some(CdpConnection::connect(&endpoint.ws_url)?);
        self.connection_generation += 1;
        if self.current_target_id.is_some() {
            let _ = self.reattach_same_target();
        } else {
            let _ = self.attach_first_page();
        }
        Ok(json!({
            "status": "reconnected",
            "browser": self.status_json(),
        }))
    }

    fn reattach_same_target(&mut self) -> Result<Value> {
        let target_id = self
            .current_target_id
            .clone()
            .ok_or_else(|| anyhow!("no previous target_id to reattach"))?;
        let targets = self.targets()?;
        if !targets.iter().any(|target| target["targetId"] == target_id) {
            return Ok(json!({
                "status": "target-gone",
                "target_id": target_id,
                "available_targets": targets,
                "next_step": "Use browser_script list_tabs()/switch_tab(...) or browser_script new_tab(...).",
            }));
        }
        let session_id = self.attach_target(&target_id)?;
        self.current_target_id = Some(target_id.clone());
        self.current_session_id = Some(session_id.clone());
        self.connection_generation += 1;
        Ok(json!({
            "status": "reattached",
            "target_id": target_id,
            "session_id": session_id,
            "browser": self.status_json(),
        }))
    }

    fn restart_runtime(&mut self) -> Result<Value> {
        self.connection = None;
        self.current_session_id = None;
        self.connection_generation += 1;
        self.reconnect_websocket()
    }

    fn restart_owned_browser(&mut self) -> Result<Value> {
        if !(self.owner == BrowserOwner::Rust && self.mode == BrowserMode::Managed) {
            return Ok(json!({
                "restarted": false,
                "reason": "restart-owned-browser only works for Rust-owned managed browsers",
            }));
        }
        let launch = self
            .managed
            .as_ref()
            .map(|managed| managed.launch.clone())
            .ok_or_else(|| anyhow!("missing managed launch config"))?;
        self.stop_owned_managed();
        self.connect_managed(launch.headless, launch.profile, launch.extra_args)?;
        Ok(json!({ "restarted": true, "browser": self.status_json() }))
    }

    fn stop_owned_managed(&mut self) {
        if let Some(mut managed) = self.managed.take() {
            let _ = managed.child.kill();
            let _ = managed.child.wait();
        }
        if self.mode == BrowserMode::Managed {
            self.connection = None;
            self.endpoint = None;
            self.current_target_id = None;
            self.current_session_id = None;
            self.mode = BrowserMode::None;
            self.owner = BrowserOwner::None;
            self.connection_generation += 1;
        }
    }

    fn doctor(&mut self, cwd: &Path) -> Result<Value> {
        let candidates = local_candidates();
        let mut checks = Vec::new();
        checks.push(json!({
            "name": "runtime state",
            "ok": true,
            "detail": "Rust browser runtime is available in-process",
        }));
        checks.push(json!({
            "name": "local browser candidates",
            "ok": !candidates.is_empty(),
            "count": candidates.len(),
            "next_step": if candidates.is_empty() { "browser local setup" } else { "browser connect local" },
        }));
        checks.push(json!({
            "name": "profile-use installed",
            "ok": which("profile-use").is_some(),
            "next_step": if which("profile-use").is_none() { "curl -fsSL https://browser-use.com/profile.sh | sh" } else { "" },
        }));
        checks.push(json!({
            "name": "Browser Use API key",
            "ok": std::env::var("BROWSER_USE_API_KEY").is_ok_and(|value| !value.trim().is_empty()),
            "detail": "Only required for Browser Use cloud browsers and cloud profiles",
        }));
        if self.endpoint.is_some() {
            let cdp_ok = self.cdp("Browser.getVersion", None, json!({})).is_ok();
            checks.push(json!({
                "name": "CDP websocket",
                "ok": cdp_ok,
                "next_step": if cdp_ok { "" } else { "browser recover reconnect-websocket" },
            }));
            let target_id = self.current_target_id.clone();
            let target_ok = match target_id.as_deref() {
                Some(target_id) => self.target_exists(target_id).unwrap_or(false),
                None => false,
            };
            checks.push(json!({
                "name": "current target",
                "ok": target_ok,
                "target_id": self.current_target_id,
                "next_step": if target_ok { "" } else { "browser recover reattach-same-target" },
            }));
        }
        checks.push(json!({
            "name": "cwd",
            "ok": cwd.exists(),
            "path": cwd.display().to_string(),
        }));
        Ok(json!({
            "status": if checks.iter().all(|check| check.get("ok").and_then(Value::as_bool).unwrap_or(false)) { "ok" } else { "needs-action" },
            "checks": checks,
            "browser": self.status_json(),
        }))
    }

    fn cdp(&mut self, method: &str, session_id: Option<&str>, params: Value) -> Result<Value> {
        let Some(connection) = self.connection.as_mut() else {
            bail!(
                "browser is not connected. Run `browser status --json` or `browser connect ...`."
            );
        };
        match connection.call(method, session_id, params) {
            Ok(value) => Ok(value),
            Err(error) => {
                let message = format!("{error:#}");
                self.last_error = Some(message.clone());
                self.connection = None;
                bail!(message);
            }
        }
    }

    fn attach_first_page(&mut self) -> Result<()> {
        let targets = self.targets()?;
        let target_id = targets
            .iter()
            .find(|target| is_real_page_target(target))
            .and_then(|target| target.get("targetId").and_then(Value::as_str))
            .map(ToOwned::to_owned);
        let target_id = match target_id {
            Some(target_id) => target_id,
            None => self
                .cdp("Target.createTarget", None, json!({ "url": "about:blank" }))?
                .get("targetId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Target.createTarget response missing targetId"))?
                .to_string(),
        };
        let session_id = self.attach_target(&target_id)?;
        self.current_target_id = Some(target_id);
        self.current_session_id = Some(session_id);
        let _ = self.cdp_current("Runtime.enable", json!({}));
        let _ = self.cdp_current("Page.enable", json!({}));
        Ok(())
    }

    fn switch_target(&mut self, target_id: &str) -> Result<Value> {
        let targets = self.targets()?;
        if !targets.iter().any(|target| target["targetId"] == target_id) {
            bail!("target not found: {target_id}");
        }
        let session_id = self.attach_target(target_id)?;
        self.current_target_id = Some(target_id.to_string());
        self.current_session_id = Some(session_id.clone());
        self.connection_generation += 1;
        Ok(json!({
            "target_id": target_id,
            "session_id": session_id,
            "page": self.current_page_probe_mut().unwrap_or(Value::Null),
        }))
    }

    fn cdp_current(&mut self, method: &str, params: Value) -> Result<Value> {
        let session_id = self.current_session_id.clone().ok_or_else(|| {
            anyhow!("no current browser session; run `browser recover reattach-same-target`")
        })?;
        self.cdp(method, Some(&session_id), params)
    }

    fn targets(&mut self) -> Result<Vec<Value>> {
        let result = self.cdp("Target.getTargets", None, json!({}))?;
        Ok(result
            .get("targetInfos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn target_exists(&mut self, target_id: &str) -> Result<bool> {
        Ok(self
            .targets()?
            .iter()
            .any(|target| target["targetId"] == target_id))
    }

    fn attach_target(&mut self, target_id: &str) -> Result<String> {
        let result = self.cdp(
            "Target.attachToTarget",
            None,
            json!({ "targetId": target_id, "flatten": true }),
        )?;
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("Target.attachToTarget response missing sessionId"))
    }

    fn current_page_probe_mut(&mut self) -> Result<Value> {
        let title = self
            .cdp_current(
                "Runtime.evaluate",
                json!({ "expression": "document.title", "returnByValue": true }),
            )
            .ok()
            .and_then(|value| value.pointer("/result/value").cloned());
        let url = self
            .cdp_current(
                "Runtime.evaluate",
                json!({ "expression": "location.href", "returnByValue": true }),
            )
            .ok()
            .and_then(|value| value.pointer("/result/value").cloned());
        Ok(json!({
            "target_id": self.current_target_id,
            "session_id": self.current_session_id,
            "title": title,
            "url": url,
        }))
    }
}

impl CdpConnection {
    fn connect(ws_url: &str) -> Result<Self> {
        let (mut socket, _) =
            connect(ws_url).with_context(|| format!("connect CDP websocket {ws_url}"))?;
        set_cdp_socket_timeouts(&mut socket);
        Ok(Self { socket, next_id: 1 })
    }

    fn call(&mut self, method: &str, session_id: Option<&str>, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(session_id) = session_id {
            message["sessionId"] = Value::String(session_id.to_string());
        }
        self.socket
            .send(Message::Text(serde_json::to_string(&message)?))
            .with_context(|| format!("send CDP {method}"))?;
        loop {
            match self
                .socket
                .read()
                .with_context(|| format!("read CDP {method}"))?
            {
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(&text)?;
                    if value.get("id").and_then(Value::as_u64) == Some(id) {
                        if let Some(error) = value.get("error") {
                            bail!("CDP {method} failed: {error}");
                        }
                        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
                Message::Close(frame) => bail!("CDP websocket closed: {frame:?}"),
                Message::Ping(bytes) => {
                    let _ = self.socket.send(Message::Pong(bytes));
                }
                _ => {}
            }
        }
    }
}

fn set_cdp_socket_timeouts(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) {
    if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(20)));
    }
}

#[derive(Debug, Clone, Serialize)]
struct LocalCandidate {
    id: String,
    browser_name: String,
    profile_path: PathBuf,
    http_url: Option<String>,
    ws_url: String,
    source: String,
    connectable: bool,
}

fn local_candidates() -> Vec<LocalCandidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (browser_name, root) in known_profile_roots() {
        let active = root.join("DevToolsActivePort");
        let Ok(raw) = fs::read_to_string(&active) else {
            continue;
        };
        let mut lines = raw.lines();
        let Some(port) = lines.next().map(str::trim).filter(|line| !line.is_empty()) else {
            continue;
        };
        let Some(path) = lines.next().map(str::trim).filter(|line| !line.is_empty()) else {
            continue;
        };
        let ws_url = format!("ws://127.0.0.1:{port}{path}");
        if !seen.insert(ws_url.clone()) {
            continue;
        }
        let id = format!("local-{}", candidates.len() + 1);
        let http_url = Some(format!("http://127.0.0.1:{port}"));
        let connectable = tcp_port_open("127.0.0.1", port.parse().unwrap_or(0));
        candidates.push(LocalCandidate {
            id,
            browser_name: browser_name.to_string(),
            profile_path: root,
            http_url,
            ws_url,
            source: active.display().to_string(),
            connectable,
        });
    }
    for port in [9222_u16, 9223] {
        let http_url = format!("http://127.0.0.1:{port}");
        let Ok(ws_url) = resolve_ws_from_http(&http_url) else {
            continue;
        };
        if !seen.insert(ws_url.clone()) {
            continue;
        }
        candidates.push(LocalCandidate {
            id: format!("local-{}", candidates.len() + 1),
            browser_name: format!("CDP port {port}"),
            profile_path: PathBuf::new(),
            http_url: Some(http_url),
            ws_url,
            source: "port-probe".to_string(),
            connectable: true,
        });
    }
    candidates
}

fn known_profile_roots() -> Vec<(&'static str, PathBuf)> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    vec![
        (
            "Google Chrome",
            home.join("Library/Application Support/Google/Chrome"),
        ),
        (
            "Chrome Canary",
            home.join("Library/Application Support/Google/Chrome Canary"),
        ),
        ("Comet", home.join("Library/Application Support/Comet")),
        (
            "Arc",
            home.join("Library/Application Support/Arc/User Data"),
        ),
        (
            "Dia",
            home.join("Library/Application Support/Dia/User Data"),
        ),
        (
            "Microsoft Edge",
            home.join("Library/Application Support/Microsoft Edge"),
        ),
        (
            "Microsoft Edge Beta",
            home.join("Library/Application Support/Microsoft Edge Beta"),
        ),
        (
            "Microsoft Edge Dev",
            home.join("Library/Application Support/Microsoft Edge Dev"),
        ),
        (
            "Microsoft Edge Canary",
            home.join("Library/Application Support/Microsoft Edge Canary"),
        ),
        (
            "Brave",
            home.join("Library/Application Support/BraveSoftware/Brave-Browser"),
        ),
        ("Google Chrome", home.join(".config/google-chrome")),
        ("Chromium", home.join(".config/chromium")),
        ("Chromium", home.join(".config/chromium-browser")),
        ("Microsoft Edge", home.join(".config/microsoft-edge")),
        (
            "Microsoft Edge Beta",
            home.join(".config/microsoft-edge-beta"),
        ),
        (
            "Microsoft Edge Dev",
            home.join(".config/microsoft-edge-dev"),
        ),
        (
            "Chromium",
            home.join(".var/app/org.chromium.Chromium/config/chromium"),
        ),
        (
            "Google Chrome",
            home.join(".var/app/com.google.Chrome/config/google-chrome"),
        ),
        (
            "Brave",
            home.join(".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser"),
        ),
        (
            "Microsoft Edge",
            home.join(".var/app/com.microsoft.Edge/config/microsoft-edge"),
        ),
        (
            "Google Chrome",
            home.join("AppData/Local/Google/Chrome/User Data"),
        ),
        (
            "Chrome Canary",
            home.join("AppData/Local/Google/Chrome SxS/User Data"),
        ),
        ("Chromium", home.join("AppData/Local/Chromium/User Data")),
        (
            "Microsoft Edge",
            home.join("AppData/Local/Microsoft/Edge/User Data"),
        ),
        (
            "Microsoft Edge Beta",
            home.join("AppData/Local/Microsoft/Edge Beta/User Data"),
        ),
        (
            "Microsoft Edge Dev",
            home.join("AppData/Local/Microsoft/Edge Dev/User Data"),
        ),
        (
            "Microsoft Edge Canary",
            home.join("AppData/Local/Microsoft/Edge SxS/User Data"),
        ),
        (
            "Brave",
            home.join("AppData/Local/BraveSoftware/Brave-Browser/User Data"),
        ),
    ]
}

fn resolve_ws_from_http(http_url: &str) -> Result<String> {
    let url = format!("{}/json/version", http_url.trim_end_matches('/'));
    let value: Value = Client::new()
        .get(&url)
        .timeout(Duration::from_secs(15))
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url} returned error"))?
        .json()
        .with_context(|| format!("parse {url}"))?;
    value
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("{url} missing webSocketDebuggerUrl"))
}

fn launch_managed_browser(launch: ManagedLaunch) -> Result<(ManagedBrowser, String)> {
    let port = free_port()?;
    let (profile_path, temp_dir) = match &launch.profile {
        ManagedProfile::Temp => {
            let temp = tempfile::Builder::new()
                .prefix("but-managed-browser.")
                .tempdir()
                .context("create managed browser temp profile")?;
            (temp.path().to_path_buf(), Some(temp))
        }
        ManagedProfile::Path(path) => {
            fs::create_dir_all(path)
                .with_context(|| format!("create managed browser profile {}", path.display()))?;
            (path.clone(), None)
        }
    };
    let mut args = vec![
        "--remote-debugging-address=127.0.0.1".to_string(),
        format!("--remote-debugging-port={port}"),
        format!("--user-data-dir={}", profile_path.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
    ];
    if launch.headless {
        args.push("--headless=new".to_string());
        args.push("--window-size=1280,720".to_string());
    } else {
        args.extend([
            "--new-window".to_string(),
            "--window-size=1512,900".to_string(),
        ]);
    }
    args.extend(launch.extra_args.clone());
    args.push("about:blank".to_string());
    let mut child = Command::new(&launch.executable)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("launch managed browser {}", launch.executable))?;
    let http_url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_error = None;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            bail!("managed browser exited before DevTools became available");
        }
        match resolve_ws_from_http(&http_url) {
            Ok(_) => {
                return Ok((
                    ManagedBrowser {
                        child,
                        _profile_dir: temp_dir,
                        launch,
                    },
                    http_url,
                ));
            }
            Err(error) => {
                last_error = Some(format!("{error:#}"));
                thread::sleep(Duration::from_millis(250));
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    bail!(
        "managed browser DevTools did not become available: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    );
}

fn chromium_candidate_paths(headless: bool) -> Vec<String> {
    let mut paths = Vec::new();
    if let Ok(path) = std::env::var("CHROME_PATH") {
        if !path.trim().is_empty() {
            paths.push(path);
        }
    }
    let mut candidates = vec![
        PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
        PathBuf::from("/opt/homebrew/Caskroom/chromium/latest/chrome-mac/Chromium.app/Contents/MacOS/Chromium"),
        PathBuf::from("/usr/bin/chromium"),
        PathBuf::from("/usr/bin/chromium-browser"),
        PathBuf::from("/usr/bin/google-chrome"),
        PathBuf::from("/usr/bin/google-chrome-stable"),
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
    ];
    if !headless {
        candidates.push(PathBuf::from(
            "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        ));
    }
    for candidate in candidates {
        if candidate.exists() {
            paths.push(candidate.display().to_string());
        }
    }
    for name in [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
    ] {
        if let Some(path) = which(name) {
            paths.push(path.display().to_string());
        }
    }
    for candidate in playwright_chromium_candidates() {
        if candidate.exists() {
            paths.push(candidate.display().to_string());
        }
    }
    dedupe_strings(paths)
}

fn dedupe_strings(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn playwright_chromium_candidates() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut matches = Vec::new();
    for root in [
        home.join("Library/Caches/ms-playwright"),
        home.join(".cache/ms-playwright"),
    ] {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("chromium-"))
            {
                continue;
            }
            let mac = path.join(
                "chrome-mac/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            );
            let mac_arm = path.join("chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing");
            let linux = path.join("chrome-linux/chrome");
            for candidate in [mac, mac_arm, linux] {
                if candidate.exists() {
                    matches.push(candidate);
                }
            }
        }
    }
    matches.sort();
    matches.reverse();
    matches
}

fn free_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn list_local_profiles() -> Result<Value> {
    let Some(profile_use) = which("profile-use") else {
        return Ok(json!({
            "status": "unavailable",
            "error": "profile-use not installed",
            "next_step": "curl -fsSL https://browser-use.com/profile.sh | sh",
        }));
    };
    let output = Command::new(profile_use)
        .args(["list", "--json"])
        .output()
        .context("run profile-use list --json")?;
    if !output.status.success() {
        return Ok(json!({
            "status": "failed",
            "stderr": String::from_utf8_lossy(&output.stderr),
        }));
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| json!({ "raw": String::from_utf8_lossy(&output.stdout).to_string() }));
    Ok(json!({ "status": "ok", "profiles": value }))
}

fn inspect_local_profile(profile: &str, domains_only: bool) -> Result<Value> {
    let Some(profile_use) = which("profile-use") else {
        return Ok(json!({
            "status": "unavailable",
            "error": "profile-use not installed",
            "next_step": "curl -fsSL https://browser-use.com/profile.sh | sh",
        }));
    };
    let attempts = [
        vec!["inspect", "--profile", profile, "--json"],
        vec!["inspect", profile, "--json"],
    ];
    for args in attempts {
        let output = Command::new(&profile_use).args(args).output();
        let Ok(output) = output else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let mut value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(
            |_| json!({ "raw": String::from_utf8_lossy(&output.stdout).to_string() }),
        );
        if domains_only {
            value = domain_summary(value);
        }
        return Ok(
            json!({ "status": "ok", "profile": profile, "domains_only": domains_only, "summary": value }),
        );
    }
    Ok(json!({
        "status": "failed",
        "profile": profile,
        "error": "profile-use inspect failed for this profile",
    }))
}

fn domain_summary(value: Value) -> Value {
    let mut domains = HashMap::<String, usize>::new();
    collect_domains(&value, &mut domains);
    let mut rows = domains
        .into_iter()
        .map(|(domain, count)| json!({ "domain": domain, "count": count }))
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        a.get("domain")
            .and_then(Value::as_str)
            .cmp(&b.get("domain").and_then(Value::as_str))
    });
    Value::Array(rows)
}

fn collect_domains(value: &Value, domains: &mut HashMap<String, usize>) {
    match value {
        Value::Object(map) => {
            if let Some(domain) = map
                .get("domain")
                .or_else(|| map.get("Domain"))
                .and_then(Value::as_str)
            {
                *domains
                    .entry(domain.trim_start_matches('.').to_string())
                    .or_default() += 1;
            }
            for value in map.values() {
                collect_domains(value, domains);
            }
        }
        Value::Array(items) => {
            for value in items {
                collect_domains(value, domains);
            }
        }
        _ => {}
    }
}

fn list_cloud_profiles() -> Result<Value> {
    let first = browser_use_api("/profiles?pageSize=100&pageNumber=1", "GET", None)?;
    let items = first
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| first.as_array().cloned())
        .unwrap_or_default();
    let mut profiles = Vec::new();
    for profile in items {
        let Some(id) = profile.get("id").and_then(Value::as_str) else {
            continue;
        };
        let detail = browser_use_api(&format!("/profiles/{id}"), "GET", None).unwrap_or(profile);
        profiles.push(json!({
            "id": detail.get("id"),
            "name": detail.get("name"),
            "userId": detail.get("userId"),
            "cookieDomains": detail.get("cookieDomains").cloned().unwrap_or(Value::Array(Vec::new())),
            "lastUsedAt": detail.get("lastUsedAt"),
        }));
    }
    Ok(json!({ "status": "ok", "profiles": profiles }))
}

fn resolve_cloud_profile_name(profile_name: &str) -> Result<String> {
    let profiles = list_cloud_profiles()?;
    let matches = profiles
        .get("profiles")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|profile| profile.get("name").and_then(Value::as_str) == Some(profile_name))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [profile] => profile
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("cloud profile {profile_name:?} missing id")),
        [] => {
            bail!("no cloud profile named {profile_name:?}; run `browser remote profiles --json`")
        }
        _ => bail!("multiple cloud profiles named {profile_name:?}; pass --profile-id <uuid>"),
    }
}

fn browser_use_api(path: &str, method: &str, body: Option<Value>) -> Result<Value> {
    let key = std::env::var("BROWSER_USE_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("BROWSER_USE_API_KEY missing"))?;
    let client = Client::new();
    let url = format!("{BU_API}{path}");
    let request = match method {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "PATCH" => client.patch(&url),
        other => bail!("unsupported Browser Use API method: {other}"),
    }
    .header("X-Browser-Use-API-Key", key)
    .header("Content-Type", "application/json")
    .timeout(Duration::from_secs(60));
    let request = if let Some(body) = body {
        request.json(&body)
    } else {
        request
    };
    let response = request
        .send()
        .with_context(|| format!("{method} {url}"))?
        .error_for_status()
        .with_context(|| format!("{method} {url} returned error"))?;
    Ok(response.json().unwrap_or_else(|_| json!({})))
}

fn stop_cloud_browser(browser_id: &str) -> Result<Value> {
    browser_use_api(
        &format!("/browsers/{browser_id}"),
        "PATCH",
        Some(json!({ "action": "stop" })),
    )
}

fn run_bridge(listener: TcpListener, session_id: String, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = handle_bridge_stream(stream, &session_id);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

fn handle_bridge_stream(mut stream: TcpStream, session_id: &str) -> Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let request: Value = serde_json::from_str(&line)?;
    let response = match bridge_request(session_id, &request) {
        Ok(value) => json!({ "ok": true, "result": value }),
        Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
    };
    writeln!(stream, "{}", serde_json::to_string(&response)?)?;
    Ok(())
}

fn bridge_request(session_id: &str, request: &Value) -> Result<Value> {
    let kind = request.get("kind").and_then(Value::as_str).unwrap_or("");
    let mut sessions = sessions()
        .lock()
        .expect("browser session registry poisoned");
    let session = sessions
        .get_mut(session_id)
        .ok_or_else(|| anyhow!("browser is not connected; run `browser connect ...` first"))?;
    match kind {
        "cdp" => {
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("bridge cdp request missing method"))?;
            let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
            let session_id = request.get("session_id").and_then(Value::as_str);
            let use_browser_session = session_id.is_none() && !method.starts_with("Target.");
            let current_session = session.current_session_id.clone();
            let session_id = if use_browser_session {
                current_session.as_deref()
            } else {
                session_id
            };
            session.cdp(method, session_id, params)
        }
        "status" => Ok(session.status_json()),
        "switch_tab" => {
            let target_id = request
                .get("target_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("switch_tab requires target_id"))?;
            session.switch_target(target_id)
        }
        "ensure_real_tab" => {
            let targets = session.targets()?;
            let Some(target_id) = targets
                .iter()
                .find(|target| is_real_page_target(target))
                .and_then(|target| target.get("targetId").and_then(Value::as_str))
                .map(ToOwned::to_owned)
            else {
                session.attach_first_page()?;
                return Ok(session.status_json());
            };
            session.switch_target(&target_id)
        }
        other => bail!("unknown browser_script bridge request: {other}"),
    }
}

fn browser_script_prelude(
    bridge_port: u16,
    cwd: &Path,
    artifact_dir: &Path,
    user_code: &str,
) -> Result<String> {
    let encoded_code = general_purpose::STANDARD.encode(user_code.as_bytes());
    Ok(format!(
        r#"
import base64, contextlib, io, json, os, pathlib, shutil, socket, sys, time, traceback, urllib.request

BRIDGE_PORT = {bridge_port}
CWD = pathlib.Path({cwd:?})
ARTIFACT_DIR = pathlib.Path({artifact_dir:?})
ARTIFACT_DIR.mkdir(parents=True, exist_ok=True)
__outputs = []
__artifacts = []
__images = []
__final_answer = None

def _jsonable(value):
    try:
        json.dumps(value)
        return value
    except TypeError:
        return repr(value)

def _bridge(payload):
    with socket.create_connection(("127.0.0.1", BRIDGE_PORT), timeout=30) as sock:
        sock.sendall((json.dumps(payload) + "\n").encode())
        chunks = []
        while True:
            data = sock.recv(65536)
            if not data:
                break
            chunks.append(data)
    response = json.loads(b"".join(chunks).decode())
    if not response.get("ok"):
        raise RuntimeError(response.get("error") or "browser bridge failed")
    return response.get("result")

def cdp(method, session_id=None, **params):
    return _bridge({{"kind": "cdp", "method": method, "session_id": session_id, "params": params}})

def cdp_batch(calls):
    out = []
    for call in calls:
        if isinstance(call, dict):
            call = dict(call)
            method = call.pop("method")
            session_id = call.pop("session_id", None)
            out.append(cdp(method, session_id=session_id, **call))
        else:
            method, params = call
            out.append(cdp(method, **params))
    return out

def js(expression, returnByValue=True):
    result = cdp("Runtime.evaluate", expression=expression, returnByValue=returnByValue, awaitPromise=True)
    if "exceptionDetails" in result:
        raise RuntimeError(json.dumps(result["exceptionDetails"], default=str))
    value = result.get("result", {{}})
    return value.get("value", value)

def page_info():
    return {{
        "title": js("document.title"),
        "url": js("location.href"),
        "readyState": js("document.readyState"),
        "target": _bridge({{"kind": "status"}}).get("page"),
    }}

def current_tab():
    return _bridge({{"kind": "status"}}).get("page")

def list_tabs():
    return cdp("Target.getTargets").get("targetInfos", [])

def switch_tab(target_id):
    return _bridge({{"kind": "switch_tab", "target_id": target_id}})

def ensure_real_tab():
    return _bridge({{"kind": "ensure_real_tab"}})

def new_tab(url="about:blank"):
    target_id = cdp("Target.createTarget", url=url).get("targetId")
    return switch_tab(target_id)

def goto_url(url):
    result = cdp("Page.navigate", url=url)
    wait_for_load(timeout=15)
    return result

def wait_for_load(timeout=10):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if js("document.readyState") in ("interactive", "complete"):
                return True
        except Exception:
            pass
        time.sleep(0.1)
    return False

def wait_for_element(selector, timeout=10):
    deadline = time.time() + timeout
    expr = "!!document.querySelector(%s)" % json.dumps(selector)
    while time.time() < deadline:
        if js(expr):
            return True
        time.sleep(0.1)
    return False

def wait_for_network_idle(timeout=10):
    time.sleep(min(timeout, 1))
    return True

def _write_b64_artifact(label, data_b64, suffix=".png", mime_type="image/png"):
    safe = "".join(ch if ch.isalnum() or ch in "-_" else "_" for ch in str(label or "screenshot")).strip("_") or "screenshot"
    path = ARTIFACT_DIR / f"{{int(time.time()*1000)}}_{{safe}}{{suffix}}"
    path.write_bytes(base64.b64decode(data_b64))
    meta = {{"path": str(path), "mime_type": mime_type, "detail": "auto", "label": label}}
    __images.append(meta)
    __artifacts.append({{"path": str(path), "kind": "image", "mime_type": mime_type}})
    return str(path)

def capture_screenshot(label="screenshot", full=False, attach=True, **kwargs):
    try:
        target_id = (current_tab() or {{}}).get("target_id")
        if target_id:
            cdp("Target.activateTarget", session_id=None, targetId=target_id)
        cdp("Page.bringToFront")
        version = cdp("Browser.getVersion", session_id=None)
        if "Headless" in (version.get("userAgent") or ""):
            cdp("Emulation.setDeviceMetricsOverride", width=1280, height=720, deviceScaleFactor=1, mobile=False)
            time.sleep(0.2)
    except Exception:
        pass
    params = {{"format": kwargs.pop("format", "png")}}
    if full:
        params["captureBeyondViewport"] = True
    params.update(kwargs)
    result = cdp("Page.captureScreenshot", **params)
    if attach:
        return _write_b64_artifact(label, result["data"], ".png", "image/png")
    return result

def screenshot(label="screenshot", full=False):
    return capture_screenshot(label=label, full=full, attach=True)

def screenshot_clip(label, x, y, width, height):
    return capture_screenshot(label=label, clip={{"x": x, "y": y, "width": width, "height": height, "scale": 1}}, attach=True)

def click_at_xy(x, y):
    cdp("Input.dispatchMouseEvent", type="mousePressed", x=x, y=y, button="left", clickCount=1)
    cdp("Input.dispatchMouseEvent", type="mouseReleased", x=x, y=y, button="left", clickCount=1)
    return True

def type_text(text):
    return cdp("Input.insertText", text=text)

def press_key(key):
    cdp("Input.dispatchKeyEvent", type="keyDown", key=key)
    cdp("Input.dispatchKeyEvent", type="keyUp", key=key)
    return True

def scroll(x=0, y=600):
    return js(f"window.scrollBy({{int(x)}}, {{int(y)}}); [window.scrollX, window.scrollY]", returnByValue=True)

def fill_input(selector, text, clear=True):
    js("""(() => {{
      const el = document.querySelector(%s);
      if (!el) throw new Error('selector not found: %s');
      if (%s) el.value = '';
      el.focus();
      el.value = %s;
      el.dispatchEvent(new Event('input', {{bubbles:true}}));
      el.dispatchEvent(new Event('change', {{bubbles:true}}));
      return true;
    }})()""" % (json.dumps(selector), selector.replace("'", "\\'"), "true" if clear else "false", json.dumps(text)))
    return True

def upload_file(*args, **kwargs):
    raise NotImplementedError("upload_file helper is reserved for the file chooser implementation; use raw CDP if needed.")

def drain_events():
    return []

def http_get(url, **kwargs):
    with urllib.request.urlopen(url, timeout=kwargs.get("timeout", 30)) as response:
        return response.read()

def copy_artifact(path, kind="file"):
    src = pathlib.Path(path).expanduser()
    dest = ARTIFACT_DIR / src.name
    if src.resolve() != dest.resolve():
        shutil.copy2(src, dest)
    meta = {{"path": str(dest), "kind": kind}}
    __artifacts.append(meta)
    return str(dest)

def emit_image(path, label=None):
    path = pathlib.Path(path).expanduser()
    meta = {{"path": str(path), "mime_type": "image/png", "detail": "auto", "label": label}}
    __images.append(meta)
    return meta

def set_final_answer(data, artifact_name=None, audit=None):
    global __final_answer
    artifact = None
    if artifact_name:
        path = ARTIFACT_DIR / artifact_name
        path.write_text(json.dumps(data, indent=2, ensure_ascii=False), encoding="utf-8")
        artifact = {{"path": str(path), "kind": "json", "mime_type": "application/json"}}
        __artifacts.append(artifact)
    __final_answer = {{"result": data, "artifact": artifact, "audit": audit}}
    return __final_answer

def audit_artifact(data=None, **requirements):
    checks = {{}}
    if data is not None:
        checks["has_data"] = data is not None and data != [] and data != {{}}
        if isinstance(data, list):
            checks["record_count"] = len(data)
    checks.update({{f"requirement_{{k}}": bool(v) for k, v in requirements.items()}})
    return {{"generated_by": "audit_artifact", "checks": checks, "ready_for_done": all(checks.values()) if checks else True}}

def agent_workspace():
    path = CWD / ".browser-use" / "agent-workspace"
    path.mkdir(parents=True, exist_ok=True)
    return str(path)

def load_agent_helpers():
    helper = pathlib.Path(agent_workspace()) / "agent_helpers.py"
    if helper.exists():
        exec(helper.read_text(encoding="utf-8"), globals())
    return helper.exists()

def _run_user_code():
    code = base64.b64decode({encoded_code:?}).decode()
    exec(compile(code, "<browser_script>", "exec"), globals())

stdout = io.StringIO()
stderr = io.StringIO()
ok = True
error = None
try:
    with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
        load_agent_helpers()
        _run_user_code()
except Exception:
    ok = False
    error = traceback.format_exc()

text = stdout.getvalue()
if stderr.getvalue():
    text += ("\n" if text else "") + stderr.getvalue()

result = {{
    "ok": ok,
    "text": text[-{SCRIPT_MAX_OUTPUT_CHARS}:],
    "error": error,
    "data": {{"final_answer": __final_answer}} if __final_answer is not None else {{}},
    "outputs": __outputs,
    "artifacts": __artifacts,
    "images": __images,
    "browser_events": [],
}}
print("__BROWSER_SCRIPT_RESULT__" + json.dumps(result, default=_jsonable))
"#
    ))
}

fn is_real_page_target(target: &Value) -> bool {
    if target.get("type").and_then(Value::as_str) != Some("page") {
        return false;
    }
    let url = target.get("url").and_then(Value::as_str).unwrap_or("");
    !matches!(url, "" | "about:blank")
        || target
            .get("title")
            .and_then(Value::as_str)
            .is_some_and(|title| !title.trim().is_empty())
}

fn browser_help() -> &'static str {
    include_str!("../../../prompts/browser-tool-description.md").trim()
}

fn render_doctor(value: &Value) -> String {
    let mut lines = vec![format!(
        "browser doctor: {}",
        value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    )];
    for check in value
        .get("checks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let ok = if check.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            "ok"
        } else {
            "needs action"
        };
        let name = check.get("name").and_then(Value::as_str).unwrap_or("check");
        lines.push(format!("- {name}: {ok}"));
        if let Some(next) = check.get("next_step").and_then(Value::as_str) {
            if !next.is_empty() {
                lines.push(format!("  next: {next}"));
            }
        }
    }
    lines.join("\n")
}

fn shell_words(input: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut quote = None;
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            (None, c) => current.push(c),
        }
    }
    if quote.is_some() {
        bail!("unterminated quote in browser command");
    }
    if !current.is_empty() {
        words.push(current);
    }
    Ok(words)
}

fn option_value(argv: &[String], name: &str) -> Option<String> {
    argv.windows(2)
        .find_map(|pair| (pair[0] == name).then(|| pair[1].clone()))
}

fn option_values(argv: &[String], name: &str) -> Vec<String> {
    argv.windows(2)
        .filter_map(|pair| (pair[0] == name).then(|| pair[1].clone()))
        .collect()
}

fn has_flag(argv: &[String], name: &str) -> bool {
    argv.iter().any(|arg| arg == name)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|path| path.exists())
}

fn tcp_port_open(host: &str, port: u16) -> bool {
    if port == 0 {
        return false;
    }
    TcpStream::connect_timeout(
        &format!("{host}:{port}").parse().expect("valid socket addr"),
        Duration::from_millis(150),
    )
    .is_ok()
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn redact_ws_url(url: &str) -> String {
    if let Some((prefix, _)) = url.split_once('?') {
        format!("{prefix}?...")
    } else {
        url.to_string()
    }
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        text.to_string()
    } else {
        let keep_from = text.len().saturating_sub(max_chars);
        format!("[truncated]\n{}", &text[keep_from..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words_accepts_browser_prefix_and_quotes() {
        assert_eq!(
            shell_words("browser remote start --profile-name 'Work Profile'").unwrap(),
            vec![
                "browser",
                "remote",
                "start",
                "--profile-name",
                "Work Profile"
            ]
        );
    }

    #[test]
    fn status_shape_contains_llm_recovery_fields() {
        let session = BrowserSession::default();
        let status = session.status_json();
        assert_eq!(status["mode"], "none");
        assert_eq!(status["connection"], "not-configured");
        assert_eq!(status["next_step"], "browser connect local");
        assert!(status.get("safety").is_some());
        assert!(status.get("connection_generation").is_some());
    }

    #[test]
    fn browser_help_is_cli_like() {
        let help = browser_help();
        assert!(help.contains("browser status --json"));
        assert!(help.contains("browser connect local"));
        assert!(help.contains("browser_script"));
        assert!(help
            .to_ascii_lowercase()
            .contains("remote start means start and connect"));
    }

    #[test]
    fn doctor_is_read_only_and_points_to_explicit_next_steps() {
        let temp = tempfile::tempdir().unwrap();
        let output =
            run_browser_command("doctor-empty", temp.path(), temp.path(), "browser doctor")
                .unwrap();
        let text = output.content.as_str().unwrap();
        assert!(text.contains("browser doctor"));
        assert!(text.contains("next: browser connect local"));
    }

    #[test]
    fn recovery_without_connection_fails_without_side_effects() {
        let temp = tempfile::tempdir().unwrap();
        let error = run_browser_command(
            "recover-empty",
            temp.path(),
            temp.path(),
            "browser recover reconnect-websocket",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("no browser endpoint is configured"));
    }

    #[test]
    fn browser_script_runs_fresh_python_without_browser_when_no_cdp_used() {
        let temp = tempfile::tempdir().unwrap();
        let output = run_browser_script(
            "script-no-cdp",
            temp.path(),
            temp.path().join("artifacts"),
            "print('hello')\nset_final_answer({'ok': True}, artifact_name='answer.json')",
            10,
        )
        .unwrap();
        assert!(output.ok, "{:?}", output.error);
        assert!(output.text.contains("hello"));
        assert_eq!(output.data["final_answer"]["result"]["ok"], true);
        assert!(output
            .artifacts
            .iter()
            .any(|artifact| artifact["path"].as_str().unwrap().ends_with("answer.json")));
    }

    #[test]
    #[ignore = "launches a real local Chromium-family browser for end-to-end smoke verification"]
    fn managed_browser_smoke_navigates_and_captures_screenshot() {
        if chromium_candidate_paths(true).is_empty() {
            eprintln!("skipping managed browser smoke: no Chromium-family browser found");
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let artifacts = temp.path().join("artifacts");
        let session_id = "managed-smoke";

        let connect = run_browser_command(
            session_id,
            temp.path(),
            &artifacts,
            "browser connect managed --headless",
        )
        .unwrap();
        assert_eq!(connect.content["status"], "connected");

        let script = run_browser_script(
            session_id,
            temp.path(),
            &artifacts,
            r##"
goto_url("data:text/html,<title>Browser Smoke</title><h1 id='ok'>Browser Smoke</h1>")
wait_for_element("#ok")
info = page_info()
print(info)
screenshot("managed_smoke")
set_final_answer(info, artifact_name="managed-smoke.json")
"##,
            30,
        )
        .unwrap();
        assert!(script.ok, "{:?}\n{}", script.error, script.text);
        assert_eq!(
            script.data["final_answer"]["result"]["title"],
            "Browser Smoke"
        );
        assert!(
            !script.images.is_empty(),
            "expected screenshot image artifact"
        );

        cleanup_session(session_id);
    }
}
