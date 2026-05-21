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
const BROWSER_SCRIPT_HELPERS: &str = include_str!("browser_script_helpers.py");

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

#[derive(Debug, Clone, Serialize)]
struct LocalBrowserInstall {
    browser_name: String,
    browser_path: PathBuf,
    user_data_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
struct LocalBrowserProfile {
    id: String,
    browser_name: String,
    browser_path: PathBuf,
    user_data_dir: PathBuf,
    profile_dir: String,
    profile_name: String,
    profile_path: PathBuf,
    display_name: String,
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
    last_error_kind: Option<String>,
    last_target_id: Option<String>,
    last_session_id: Option<String>,
    last_emitted_browser_payload: Option<Value>,
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
            last_error_kind: None,
            last_target_id: None,
            last_session_id: None,
            last_emitted_browser_payload: None,
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
    let bridge_errors = Arc::new(Mutex::new(Vec::new()));
    let bridge_stop = stop.clone();
    let bridge_error_sink = bridge_errors.clone();
    let bridge_session_id = session_id.to_string();
    let bridge = thread::spawn(move || {
        run_bridge(listener, bridge_session_id, bridge_stop, bridge_error_sink)
    });

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
    let bridge_errors = bridge_errors
        .lock()
        .expect("browser_script bridge error registry poisoned")
        .clone();

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
    if !bridge_errors.is_empty() {
        response.browser_events.push(json!({
            "type": "browser.bridge_errors",
            "payload": { "errors": bridge_errors },
        }));
        if !response.ok {
            let details = response
                .browser_events
                .last()
                .and_then(|event| event.pointer("/payload/errors"))
                .map(ToString::to_string)
                .unwrap_or_default();
            response.error = Some(match response.error.take() {
                Some(error) => format!("{error}\n\nRust bridge errors: {details}"),
                None => format!("Rust bridge errors: {details}"),
            });
        }
    }
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
            let profile_ref = option_value(argv, "--profile");
            let (opened, profile, open_error) = if let Some(profile_ref) = profile_ref {
                let profiles = detect_local_profiles();
                let selected = resolve_local_profile(&profiles, &profile_ref)?;
                match open_local_profile_url(&selected, url) {
                    Ok(()) => (true, Some(selected), None),
                    Err(error) => (false, Some(selected), Some(format!("{error:#}"))),
                }
            } else {
                (open::that(url).is_ok(), None, None)
            };
            Ok(json!({
                "status": "needs-user-action",
                "opened": opened,
                "url": url,
                "profile": profile,
                "open_error": open_error,
                "instructions": [
                    "In the browser/profile that opens, enable 'Allow remote debugging for this browser instance' if Chrome reports it is blocked.",
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

    fn browser_events(&mut self) -> Vec<Value> {
        let mut events = Vec::new();
        if self.mode == BrowserMode::None {
            self.last_emitted_browser_payload = None;
            return events;
        }
        let payload = self.browser_event_payload();
        if self.last_emitted_browser_payload.as_ref() == Some(&payload) {
            return events;
        }
        let event_type = self.browser_event_type(&payload);
        self.last_emitted_browser_payload = Some(payload.clone());
        events.push(json!({
            "type": event_type,
            "payload": payload,
        }));
        if self.live_url.is_some() {
            events.push(json!({
                "type": "browser.live_url",
                "payload": { "url": self.live_url },
            }));
        }
        events
    }

    fn browser_event_type(&self, payload: &Value) -> &'static str {
        let status = payload.get("status").and_then(Value::as_str);
        if status != Some("connected") {
            return "browser.disconnected";
        }
        let Some(previous) = self.last_emitted_browser_payload.as_ref() else {
            return "browser.connected";
        };
        if previous.get("status").and_then(Value::as_str) != Some("connected") {
            return "browser.reconnected";
        }
        if previous.get("target_id") != payload.get("target_id") {
            return "browser.target_changed";
        }
        if previous.get("session_id") != payload.get("session_id")
            || previous.get("generation") != payload.get("generation")
        {
            return "browser.reconnected";
        }
        "browser.connected"
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
            "last_target_id": self.last_target_id,
            "last_session_id": self.last_session_id,
        });
        json!({
            "mode": self.mode.as_str(),
            "connection": if connected { "connected" } else if self.endpoint.is_some() { "disconnected" } else { "not-configured" },
            "reason": self.last_error,
            "loss_reason": self.last_error_kind,
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
        } else if matches!(
            self.last_error_kind.as_deref(),
            Some("browser-closed" | "stale-port")
        ) && self.mode == BrowserMode::Local
        {
            Some("Open Chrome with the selected profile, then run browser connect local")
        } else if self.last_error_kind.as_deref() == Some("permission-blocked")
            && self.mode == BrowserMode::Local
        {
            Some("browser local setup")
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
            self.last_error_kind = Some("browser-not-running".to_string());
            return Ok(json!({
                "status": "blocked",
                "state": "browser-not-running",
                "reason": "No running Chromium-family browser is exposing a reachable local CDP endpoint.",
                "next_step": "browser local setup",
            }));
        }
        let reachable = candidates
            .iter()
            .filter(|candidate| candidate.connectable)
            .cloned()
            .collect::<Vec<_>>();
        if reachable.is_empty() {
            self.last_error =
                Some("Only stale local browser debug candidates were found".to_string());
            self.last_error_kind = Some("stale-port".to_string());
            return Ok(json!({
                "status": "blocked",
                "state": "stale-port",
                "reason": "Found stale DevToolsActivePort files, but no local Chrome CDP port is reachable. Chrome was likely closed or the debug server stopped.",
                "candidates": candidates,
                "next_step": "Open Chrome with the selected profile, then run browser connect local",
            }));
        }
        let candidate = if let Some(candidate_id) = candidate_id {
            let Some(candidate) = candidates
                .into_iter()
                .find(|candidate| candidate.id == candidate_id)
            else {
                bail!("unknown local candidate id: {candidate_id}");
            };
            if !candidate.connectable {
                self.last_error = candidate.reason.clone();
                self.last_error_kind = Some(candidate.state.clone());
                return Ok(json!({
                    "status": "blocked",
                    "state": candidate.state,
                    "reason": candidate.reason,
                    "candidate": candidate,
                    "next_step": "Open Chrome with this profile, then run browser connect local",
                }));
            }
            candidate
        } else if reachable.len() == 1 {
            reachable
                .into_iter()
                .next()
                .expect("one reachable candidate")
        } else {
            return Ok(json!({
                "status": "needs-user-action",
                "reason": "Multiple reachable local browser candidates are available. Ask the user which browser/profile to attach.",
                "candidates": reachable,
                "ignored_candidates": candidates.into_iter().filter(|candidate| !candidate.connectable).collect::<Vec<_>>(),
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
        if let Err(error) =
            self.connect_endpoint(endpoint, BrowserMode::Local, BrowserOwner::External)
        {
            let message = format!("{error:#}");
            let kind = classify_browser_error(&message);
            self.last_error = Some(message.clone());
            self.last_error_kind = Some(kind.to_string());
            return Ok(json!({
                "status": "blocked",
                "state": kind,
                "reason": local_connect_error_reason(kind, &message),
                "candidate": candidate,
                "raw_error": message,
                "next_step": local_connect_next_step(kind),
            }));
        }
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
        self.last_error = None;
        self.last_error_kind = None;
        self.last_target_id = None;
        self.last_session_id = None;
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
        self.last_error_kind = None;
        self.last_target_id = None;
        self.last_session_id = None;
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
            self.last_error = None;
            self.last_error_kind = None;
            self.last_target_id = None;
            self.last_session_id = None;
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
            "ok": candidates.iter().any(|candidate| candidate.connectable),
            "count": candidates.len(),
            "connectable_count": candidates.iter().filter(|candidate| candidate.connectable).count(),
            "stale_count": candidates.iter().filter(|candidate| candidate.stale).count(),
            "state": if candidates.iter().any(|candidate| candidate.connectable) {
                "reachable"
            } else if candidates.iter().any(|candidate| candidate.stale) {
                "stale-port"
            } else {
                "browser-not-running"
            },
            "detail": if candidates.iter().any(|candidate| candidate.connectable) {
                "At least one local browser CDP endpoint is reachable."
            } else if candidates.iter().any(|candidate| candidate.stale) {
                "DevToolsActivePort files exist, but their ports are not reachable. Chrome was likely closed or restarted."
            } else {
                "No local browser CDP endpoint is reachable."
            },
            "next_step": if candidates.iter().any(|candidate| candidate.connectable) {
                "browser connect local"
            } else if candidates.iter().any(|candidate| candidate.stale) {
                "Open Chrome with the selected profile, then run browser connect local"
            } else {
                "browser local setup"
            },
        }));
        let profiles = detect_local_profiles();
        checks.push(json!({
            "name": "local browser profiles",
            "ok": !profiles.is_empty(),
            "count": profiles.len(),
            "detail": "Rust filesystem profile discovery; no external CLI required",
            "next_step": if profiles.is_empty() { "Use `browser local profiles --json` to see scan details." } else { "browser local profiles --json" },
        }));
        checks.push(json!({
            "name": "Browser Use API key",
            "ok": std::env::var("BROWSER_USE_API_KEY").is_ok_and(|value| !value.trim().is_empty()),
            "detail": "Only required for Browser Use cloud browsers and cloud profiles",
        }));
        if let Some(endpoint) = self.endpoint.as_ref() {
            let endpoint_probe = probe_endpoint(endpoint);
            let cdp_ok = endpoint_probe.ok;
            checks.push(json!({
                "name": "CDP websocket",
                "ok": cdp_ok,
                "state": endpoint_probe.state,
                "detail": endpoint_probe.detail,
                "next_step": if cdp_ok {
                    ""
                } else if self.mode == BrowserMode::Local {
                    endpoint_probe.next_step
                } else {
                    "browser recover reconnect-websocket"
                },
            }));
            let target_ok =
                cdp_ok && self.current_target_id.is_some() && self.current_session_id.is_some();
            checks.push(json!({
                "name": "current target",
                "ok": target_ok,
                "target_id": self.current_target_id,
                "last_target_id": self.last_target_id,
                "next_step": if target_ok { "" } else if cdp_ok { "browser recover reattach-same-target" } else { "Recover the browser connection before reattaching a target." },
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
                self.last_error_kind = Some(classify_browser_error(&message).to_string());
                self.connection = None;
                self.last_target_id = self.current_target_id.take();
                self.last_session_id = self.current_session_id.take();
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

    fn cdp_storage_cookies(&mut self) -> Result<Vec<Value>> {
        Ok(self
            .call("Storage.getCookies", None, json!({}))?
            .get("cookies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }
}

fn set_cdp_socket_timeouts(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) {
    if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(20)));
    }
}

fn classify_browser_error(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("403 forbidden") || lower.contains("http error: 403") {
        "permission-blocked"
    } else if lower.contains("target")
        && (lower.contains("not found")
            || lower.contains("target-gone")
            || lower.contains("no target with given id"))
    {
        "target-gone"
    } else if lower.contains("connection refused")
        || lower.contains("couldn't connect to server")
        || lower.contains("unable to connect")
        || lower.contains("operation timed out")
        || lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("websocket closed")
        || lower.contains("already closed")
    {
        "browser-closed"
    } else {
        "websocket-dropped"
    }
}

fn local_connect_error_reason(kind: &str, raw_error: &str) -> String {
    match kind {
        "permission-blocked" => {
            "Chrome is running, but it rejected CDP control. Remote debugging permission is likely blocked for this browser instance.".to_string()
        }
        "browser-closed" => {
            "Chrome is not currently exposing the selected local CDP endpoint. It may have been closed, restarted, or stopped its debug server.".to_string()
        }
        "target-gone" => "The previous browser tab target is gone.".to_string(),
        _ => format!("Local browser CDP connection failed: {raw_error}"),
    }
}

fn local_connect_next_step(kind: &str) -> &'static str {
    match kind {
        "permission-blocked" => "browser local setup",
        "browser-closed" => "Open Chrome with the selected profile, then run browser connect local",
        "target-gone" => "Use browser_script list_tabs()/switch_tab(...) or open a new tab",
        _ => "browser doctor --json",
    }
}

struct EndpointProbe {
    ok: bool,
    state: &'static str,
    detail: String,
    next_step: &'static str,
}

fn probe_endpoint(endpoint: &Endpoint) -> EndpointProbe {
    let Some(http_url) = endpoint.http_url.as_deref() else {
        return EndpointProbe {
            ok: true,
            state: "unknown",
            detail:
                "No DevTools HTTP endpoint is available to probe without touching the websocket."
                    .to_string(),
            next_step: "browser recover reconnect-websocket",
        };
    };
    let url = format!("{}/json/version", http_url.trim_end_matches('/'));
    let response = Client::new()
        .get(&url)
        .timeout(Duration::from_secs(2))
        .send();
    match response {
        Ok(response) if response.status().is_success() => EndpointProbe {
            ok: true,
            state: "reachable",
            detail: format!("{url} is reachable."),
            next_step: "",
        },
        Ok(response) if response.status().as_u16() == 403 => EndpointProbe {
            ok: false,
            state: "permission-blocked",
            detail: "The browser is reachable, but Chrome rejected DevTools access with 403."
                .to_string(),
            next_step: "browser local setup",
        },
        Ok(response) => EndpointProbe {
            ok: false,
            state: "endpoint-error",
            detail: format!("{url} returned HTTP {}.", response.status()),
            next_step: "browser recover reconnect-websocket",
        },
        Err(error) => EndpointProbe {
            ok: false,
            state: if endpoint.kind == "devtools-active-port" {
                "browser-closed"
            } else {
                "websocket-dropped"
            },
            detail: format!("{url} is not reachable: {error:#}"),
            next_step: if endpoint.kind == "devtools-active-port" {
                "Open Chrome with the selected profile, then run browser connect local"
            } else {
                "browser recover reconnect-websocket"
            },
        },
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
    state: String,
    stale: bool,
    reason: Option<String>,
    next_step: Option<String>,
}

fn local_candidates() -> Vec<LocalCandidate> {
    local_candidates_from_roots(known_profile_roots(), &[9222_u16, 9223])
}

fn local_candidates_from_roots(
    roots: Vec<(&'static str, PathBuf)>,
    probe_ports: &[u16],
) -> Vec<LocalCandidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (browser_name, root) in roots {
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
        let state = if connectable {
            "reachable"
        } else {
            "stale-port"
        };
        candidates.push(LocalCandidate {
            id,
            browser_name: browser_name.to_string(),
            profile_path: root,
            http_url,
            ws_url,
            source: active.display().to_string(),
            connectable,
            state: state.to_string(),
            stale: !connectable,
            reason: (!connectable).then(|| {
                "DevToolsActivePort exists, but the recorded CDP port is not reachable. Chrome was likely closed or the debug server stopped.".to_string()
            }),
            next_step: Some(if connectable {
                "browser connect local --candidate <id>".to_string()
            } else {
                "Open Chrome with this profile, then run browser connect local".to_string()
            }),
        });
    }
    for port in probe_ports {
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
            state: "reachable".to_string(),
            stale: false,
            reason: None,
            next_step: Some("browser connect local --candidate <id>".to_string()),
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
    Ok(json!({
        "status": "ok",
        "source": "rust-local-filesystem",
        "profiles": detect_local_profiles(),
    }))
}

fn inspect_local_profile(profile: &str, domains_only: bool) -> Result<Value> {
    let profiles = detect_local_profiles();
    let selected = match resolve_local_profile(&profiles, profile) {
        Ok(profile) => profile,
        Err(error) => {
            return Ok(json!({
                "status": "failed",
                "profile_ref": profile,
                "error": format!("{error:#}"),
                "available_profiles": profiles,
            }));
        }
    };
    match inspect_local_profile_cookies(&selected) {
        Ok(summary) => Ok(json!({
            "status": "ok",
            "source": "rust-local-cdp",
            "profile": selected,
            "domains_only": domains_only,
            "raw_cookie_values_returned": false,
            "cookie_summary": summary,
        })),
        Err(error) => Ok(json!({
            "status": "failed",
            "source": "rust-local-cdp",
            "profile": selected,
            "raw_cookie_values_returned": false,
            "error": format!("{error:#}"),
        })),
    }
}

fn detect_local_profiles() -> Vec<LocalBrowserProfile> {
    detect_profiles_from_installs(known_local_browser_installs())
}

fn detect_profiles_from_installs(installs: Vec<LocalBrowserInstall>) -> Vec<LocalBrowserProfile> {
    let mut profiles = Vec::new();
    let mut seen = HashSet::new();
    for install in installs {
        if !install.user_data_dir.exists() {
            continue;
        }
        let profile_names = load_profile_names_from_local_state(&install.user_data_dir);
        let Ok(entries) = fs::read_dir(&install.user_data_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let profile_dir = entry.file_name().to_string_lossy().to_string();
            let profile_path = entry.path();
            if !is_valid_local_profile_dir(&profile_path) {
                continue;
            }
            if !seen.insert((install.user_data_dir.clone(), profile_dir.clone())) {
                continue;
            }
            let profile_name = profile_names
                .get(&profile_dir)
                .filter(|name| !name.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| profile_dir.clone());
            profiles.push(LocalBrowserProfile {
                id: format!("{}:{profile_dir}", browser_slug(&install.browser_name)),
                browser_name: install.browser_name.clone(),
                browser_path: install.browser_path.clone(),
                user_data_dir: install.user_data_dir.clone(),
                profile_dir,
                profile_name: profile_name.clone(),
                profile_path,
                display_name: format!("{} - {profile_name}", install.browser_name),
            });
        }
    }
    profiles.sort_by(|a, b| {
        a.browser_name
            .cmp(&b.browser_name)
            .then_with(|| {
                profile_dir_sort_key(&a.profile_dir).cmp(&profile_dir_sort_key(&b.profile_dir))
            })
            .then_with(|| natural_cmp(&a.profile_name, &b.profile_name))
    });
    profiles
}

fn known_local_browser_installs() -> Vec<LocalBrowserInstall> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let program_files = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:/Program Files"));
    let program_files_x86 = std::env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:/Program Files (x86)"));
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData/Local"));
    let candidates = vec![
        (
            "Google Chrome",
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            home.join("Library/Application Support/Google/Chrome"),
        ),
        (
            "Chrome Canary",
            PathBuf::from(
                "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
            ),
            home.join("Library/Application Support/Google/Chrome Canary"),
        ),
        (
            "Brave",
            PathBuf::from("/Applications/Brave Browser.app/Contents/MacOS/Brave Browser"),
            home.join("Library/Application Support/BraveSoftware/Brave-Browser"),
        ),
        (
            "Microsoft Edge",
            PathBuf::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
            home.join("Library/Application Support/Microsoft Edge"),
        ),
        (
            "Chromium",
            PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
            home.join("Library/Application Support/Chromium"),
        ),
        (
            "Arc",
            PathBuf::from("/Applications/Arc.app/Contents/MacOS/Arc"),
            home.join("Library/Application Support/Arc/User Data"),
        ),
        (
            "Dia",
            PathBuf::from("/Applications/Dia.app/Contents/MacOS/Dia"),
            home.join("Library/Application Support/Dia"),
        ),
        (
            "Comet",
            PathBuf::from("/Applications/Comet.app/Contents/MacOS/Comet"),
            home.join("Library/Application Support/Comet"),
        ),
        (
            "Helium",
            PathBuf::from("/Applications/Helium.app/Contents/MacOS/Helium"),
            home.join("Library/Application Support/Helium"),
        ),
        (
            "Sidekick",
            PathBuf::from("/Applications/Sidekick.app/Contents/MacOS/Sidekick"),
            home.join("Library/Application Support/Sidekick"),
        ),
        (
            "Thorium",
            PathBuf::from("/Applications/Thorium.app/Contents/MacOS/Thorium"),
            home.join("Library/Application Support/Thorium"),
        ),
        (
            "SigmaOS",
            PathBuf::from("/Applications/SigmaOS.app/Contents/MacOS/SigmaOS"),
            home.join("Library/Application Support/SigmaOS/User Data"),
        ),
        (
            "Wavebox",
            PathBuf::from("/Applications/Wavebox.app/Contents/MacOS/Wavebox"),
            home.join("Library/Application Support/WaveboxApp"),
        ),
        (
            "Ghost Browser",
            PathBuf::from("/Applications/Ghost Browser.app/Contents/MacOS/Ghost Browser"),
            home.join("Library/Application Support/Ghost Browser"),
        ),
        (
            "Blisk",
            PathBuf::from("/Applications/Blisk.app/Contents/MacOS/Blisk"),
            home.join("Library/Application Support/Blisk"),
        ),
        (
            "Opera",
            PathBuf::from("/Applications/Opera.app/Contents/MacOS/Opera"),
            home.join("Library/Application Support/com.operasoftware.Opera"),
        ),
        (
            "Vivaldi",
            PathBuf::from("/Applications/Vivaldi.app/Contents/MacOS/Vivaldi"),
            home.join("Library/Application Support/Vivaldi"),
        ),
        (
            "Yandex",
            PathBuf::from("/Applications/Yandex.app/Contents/MacOS/Yandex"),
            home.join("Library/Application Support/Yandex/YandexBrowser"),
        ),
        (
            "Iridium",
            PathBuf::from("/Applications/Iridium.app/Contents/MacOS/Iridium"),
            home.join("Library/Application Support/Iridium"),
        ),
        (
            "Google Chrome",
            PathBuf::from("/usr/bin/google-chrome"),
            home.join(".config/google-chrome"),
        ),
        (
            "Google Chrome",
            PathBuf::from("/usr/bin/google-chrome-stable"),
            home.join(".config/google-chrome"),
        ),
        (
            "Brave",
            PathBuf::from("/usr/bin/brave-browser"),
            home.join(".config/BraveSoftware/Brave-Browser"),
        ),
        (
            "Brave",
            PathBuf::from("/usr/bin/brave"),
            home.join(".config/BraveSoftware/Brave-Browser"),
        ),
        (
            "Brave",
            PathBuf::from("/snap/bin/brave"),
            home.join(".config/BraveSoftware/Brave-Browser"),
        ),
        (
            "Microsoft Edge",
            PathBuf::from("/usr/bin/microsoft-edge"),
            home.join(".config/microsoft-edge"),
        ),
        (
            "Microsoft Edge",
            PathBuf::from("/usr/bin/microsoft-edge-stable"),
            home.join(".config/microsoft-edge"),
        ),
        (
            "Chromium",
            PathBuf::from("/usr/bin/chromium"),
            home.join(".config/chromium"),
        ),
        (
            "Chromium",
            PathBuf::from("/usr/bin/chromium-browser"),
            home.join(".config/chromium"),
        ),
        (
            "Chromium",
            PathBuf::from("/snap/bin/chromium"),
            home.join(".config/chromium"),
        ),
        (
            "Opera",
            PathBuf::from("/usr/bin/opera"),
            home.join(".config/opera"),
        ),
        (
            "Opera",
            PathBuf::from("/snap/bin/opera"),
            home.join(".config/opera"),
        ),
        (
            "Vivaldi",
            PathBuf::from("/usr/bin/vivaldi"),
            home.join(".config/vivaldi"),
        ),
        (
            "Vivaldi",
            PathBuf::from("/usr/bin/vivaldi-stable"),
            home.join(".config/vivaldi"),
        ),
        (
            "Vivaldi",
            PathBuf::from("/snap/bin/vivaldi"),
            home.join(".config/vivaldi"),
        ),
        (
            "Yandex",
            PathBuf::from("/usr/bin/yandex-browser"),
            home.join(".config/yandex-browser"),
        ),
        (
            "Yandex",
            PathBuf::from("/usr/bin/yandex-browser-stable"),
            home.join(".config/yandex-browser"),
        ),
        (
            "Iridium",
            PathBuf::from("/usr/bin/iridium-browser"),
            home.join(".config/iridium"),
        ),
        (
            "Ungoogled Chromium",
            PathBuf::from("/usr/bin/ungoogled-chromium"),
            home.join(".config/chromium"),
        ),
        (
            "Thorium",
            PathBuf::from("/usr/bin/thorium-browser"),
            home.join(".config/thorium"),
        ),
        (
            "Sidekick",
            home.join(".local/share/sidekick/sidekick"),
            home.join(".config/Sidekick"),
        ),
        (
            "Wavebox",
            PathBuf::from("/usr/bin/wavebox"),
            home.join(".config/Wavebox"),
        ),
        (
            "Google Chrome",
            program_files.join("Google/Chrome/Application/chrome.exe"),
            local_app_data.join("Google/Chrome/User Data"),
        ),
        (
            "Google Chrome",
            program_files_x86.join("Google/Chrome/Application/chrome.exe"),
            local_app_data.join("Google/Chrome/User Data"),
        ),
        (
            "Google Chrome",
            local_app_data.join("Google/Chrome/Application/chrome.exe"),
            local_app_data.join("Google/Chrome/User Data"),
        ),
        (
            "Brave",
            program_files.join("BraveSoftware/Brave-Browser/Application/brave.exe"),
            local_app_data.join("BraveSoftware/Brave-Browser/User Data"),
        ),
        (
            "Brave",
            local_app_data.join("BraveSoftware/Brave-Browser/Application/brave.exe"),
            local_app_data.join("BraveSoftware/Brave-Browser/User Data"),
        ),
        (
            "Microsoft Edge",
            program_files.join("Microsoft/Edge/Application/msedge.exe"),
            local_app_data.join("Microsoft/Edge/User Data"),
        ),
        (
            "Microsoft Edge",
            program_files_x86.join("Microsoft/Edge/Application/msedge.exe"),
            local_app_data.join("Microsoft/Edge/User Data"),
        ),
        (
            "Chromium",
            local_app_data.join("Chromium/Application/chrome.exe"),
            local_app_data.join("Chromium/User Data"),
        ),
        (
            "Opera",
            local_app_data.join("Programs/Opera/opera.exe"),
            home.join("AppData/Roaming/Opera Software/Opera Stable"),
        ),
        (
            "Opera",
            program_files.join("Opera/opera.exe"),
            home.join("AppData/Roaming/Opera Software/Opera Stable"),
        ),
        (
            "Vivaldi",
            local_app_data.join("Vivaldi/Application/vivaldi.exe"),
            local_app_data.join("Vivaldi/User Data"),
        ),
        (
            "Vivaldi",
            program_files.join("Vivaldi/Application/vivaldi.exe"),
            local_app_data.join("Vivaldi/User Data"),
        ),
        (
            "Yandex",
            local_app_data.join("Yandex/YandexBrowser/Application/browser.exe"),
            local_app_data.join("Yandex/YandexBrowser/User Data"),
        ),
        (
            "Iridium",
            local_app_data.join("Iridium/Application/iridium.exe"),
            local_app_data.join("Iridium/User Data"),
        ),
        (
            "Sidekick",
            local_app_data.join("Sidekick/Application/sidekick.exe"),
            local_app_data.join("Sidekick/User Data"),
        ),
        (
            "Thorium",
            local_app_data.join("Thorium/Application/thorium.exe"),
            local_app_data.join("Thorium/User Data"),
        ),
        (
            "Wavebox",
            local_app_data.join("WaveboxApp/Application/wavebox.exe"),
            local_app_data.join("WaveboxApp/User Data"),
        ),
        (
            "Blisk",
            local_app_data.join("Blisk/Application/blisk.exe"),
            local_app_data.join("Blisk/User Data"),
        ),
    ];
    let mut installs: Vec<LocalBrowserInstall> = Vec::new();
    let mut seen: HashMap<(String, PathBuf), usize> = HashMap::new();
    for (browser_name, browser_path, user_data_dir) in candidates {
        if !browser_path.exists() && !user_data_dir.exists() {
            continue;
        }
        let key = (browser_name.to_string(), user_data_dir.clone());
        let candidate = LocalBrowserInstall {
            browser_name: browser_name.to_string(),
            browser_path,
            user_data_dir,
        };
        if let Some(index) = seen.get(&key).copied() {
            if !installs[index].browser_path.exists() && candidate.browser_path.exists() {
                installs[index] = candidate;
            }
        } else {
            seen.insert(key, installs.len());
            installs.push(candidate);
        }
    }
    installs
}

fn load_profile_names_from_local_state(user_data_dir: &Path) -> HashMap<String, String> {
    let Ok(raw) = fs::read_to_string(user_data_dir.join("Local State")) else {
        return HashMap::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return HashMap::new();
    };
    value
        .pointer("/profile/info_cache")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(profile_dir, info)| {
            info.get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .map(|name| (profile_dir.clone(), name.to_string()))
        })
        .collect()
}

fn is_valid_local_profile_dir(path: &Path) -> bool {
    ["Preferences", "Cookies", "History", "Network/Cookies"]
        .iter()
        .any(|relative| path.join(relative).exists())
}

fn browser_slug(name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn profile_dir_sort_key(profile_dir: &str) -> (u8, String) {
    if profile_dir == "Default" {
        (0, String::new())
    } else {
        (1, profile_dir.to_string())
    }
}

fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let mut ia = 0;
    let mut ib = 0;
    while ia < a_bytes.len() && ib < b_bytes.len() {
        if a_bytes[ia].is_ascii_digit() && b_bytes[ib].is_ascii_digit() {
            let (na, next_a) = parse_ascii_number(a_bytes, ia);
            let (nb, next_b) = parse_ascii_number(b_bytes, ib);
            match na.cmp(&nb) {
                std::cmp::Ordering::Equal => {
                    ia = next_a;
                    ib = next_b;
                }
                other => return other,
            }
        } else {
            match a_bytes[ia].cmp(&b_bytes[ib]) {
                std::cmp::Ordering::Equal => {
                    ia += 1;
                    ib += 1;
                }
                other => return other,
            }
        }
    }
    a_bytes.len().cmp(&b_bytes.len())
}

fn parse_ascii_number(bytes: &[u8], mut index: usize) -> (u64, usize) {
    let mut number = 0_u64;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        number = number
            .saturating_mul(10)
            .saturating_add((bytes[index] - b'0') as u64);
        index += 1;
    }
    (number, index)
}

fn resolve_local_profile(
    profiles: &[LocalBrowserProfile],
    profile_ref: &str,
) -> Result<LocalBrowserProfile> {
    if let Some(profile) = profiles.iter().find(|profile| profile.id == profile_ref) {
        return Ok(profile.clone());
    }
    let matches = profiles
        .iter()
        .filter(|profile| {
            profile.profile_name == profile_ref
                || profile.profile_dir == profile_ref
                || profile.display_name == profile_ref
        })
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [profile] => Ok(profile.clone()),
        [] => {
            bail!("no local profile matched {profile_ref:?}; run `browser local profiles --json`")
        }
        _ => bail!("multiple local profiles matched {profile_ref:?}; pass the exact profile id"),
    }
}

fn open_local_profile_url(profile: &LocalBrowserProfile, url: &str) -> Result<()> {
    let mut command = Command::new(&profile.browser_path);
    command
        .arg(format!("--profile-directory={}", profile.profile_dir))
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .spawn()
        .with_context(|| format!("open {} with {}", url, profile.display_name))?;
    Ok(())
}

fn inspect_local_profile_cookies(profile: &LocalBrowserProfile) -> Result<Value> {
    let temp = tempfile::Builder::new()
        .prefix("but-profile-inspect.")
        .tempdir()
        .context("create temp profile inspection dir")?;
    copy_local_state_for_profile(&profile.user_data_dir, temp.path())?;
    copy_profile_dir_for_inspection(&profile.profile_path, &temp.path().join("Default"))?;
    let launch = ManagedLaunch {
        executable: profile.browser_path.display().to_string(),
        profile: ManagedProfile::Path(temp.path().to_path_buf()),
        headless: true,
        extra_args: vec!["--no-startup-window".to_string()],
    };
    let (mut managed, http_url) = launch_managed_browser(launch)?;
    let result = (|| -> Result<Value> {
        let ws_url = resolve_ws_from_http(&http_url)?;
        let mut connection = CdpConnection::connect(&ws_url)?;
        let cookies = connection.cdp_storage_cookies()?;
        Ok(cookie_domain_summary(&cookies))
    })();
    let _ = managed.child.kill();
    let _ = managed.child.wait();
    result
}

fn copy_local_state_for_profile(src_user_data_dir: &Path, dst_user_data_dir: &Path) -> Result<()> {
    fs::create_dir_all(dst_user_data_dir)
        .with_context(|| format!("create temp user data dir {}", dst_user_data_dir.display()))?;
    let src = src_user_data_dir.join("Local State");
    if src.exists() {
        let _ = fs::copy(&src, dst_user_data_dir.join("Local State"));
    }
    Ok(())
}

fn copy_profile_dir_for_inspection(src: &Path, dst: &Path) -> Result<()> {
    const SKIP_DIRS: &[&str] = &[
        "Service Worker",
        "Extensions",
        "IndexedDB",
        "Local Extension Settings",
        "Local Storage",
        "GPUCache",
        "Shared Dictionary",
        "SharedCache",
    ];
    const SKIP_FILES: &[&str] = &[
        "SingletonLock",
        "SingletonSocket",
        "SingletonCookie",
        "lockfile",
        "RunningChromeVersion",
        "History",
    ];
    fn copy_inner(src: &Path, dst: &Path) -> Result<()> {
        fs::create_dir_all(dst).with_context(|| format!("create {}", dst.display()))?;
        let entries = fs::read_dir(src).with_context(|| format!("read {}", src.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                let _ = copy_inner(&path, &dst.join(&name));
            } else if file_type.is_file() {
                if SKIP_FILES.contains(&name.as_str()) {
                    continue;
                }
                let _ = fs::copy(&path, dst.join(&name));
            }
        }
        Ok(())
    }
    copy_inner(src, dst)
}

fn cookie_domain_summary(cookies: &[Value]) -> Value {
    #[derive(Default)]
    struct DomainStats {
        count: usize,
        session_count: usize,
        persistent_count: usize,
        earliest_expiry: Option<i64>,
        latest_expiry: Option<i64>,
    }

    let mut domains = HashMap::<String, DomainStats>::new();
    for cookie in cookies {
        let Some(domain) = cookie.get("domain").and_then(Value::as_str) else {
            continue;
        };
        let domain = domain.trim_start_matches('.').to_string();
        if domain.is_empty() {
            continue;
        }
        let stats = domains.entry(domain).or_default();
        stats.count += 1;
        let session = cookie
            .get("session")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if session {
            stats.session_count += 1;
        } else {
            stats.persistent_count += 1;
            if let Some(expiry) = cookie.get("expires").and_then(Value::as_f64) {
                if expiry > 0.0 {
                    let expiry = expiry as i64;
                    stats.earliest_expiry = Some(
                        stats
                            .earliest_expiry
                            .map_or(expiry, |current| current.min(expiry)),
                    );
                    stats.latest_expiry = Some(
                        stats
                            .latest_expiry
                            .map_or(expiry, |current| current.max(expiry)),
                    );
                }
            }
        }
    }
    let mut rows = domains
        .into_iter()
        .map(|(domain, stats)| {
            json!({
                "domain": domain,
                "count": stats.count,
                "session_count": stats.session_count,
                "persistent_count": stats.persistent_count,
                "earliest_expiry": stats.earliest_expiry,
                "latest_expiry": stats.latest_expiry,
            })
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.get("count")
            .and_then(Value::as_u64)
            .cmp(&a.get("count").and_then(Value::as_u64))
            .then_with(|| {
                a.get("domain")
                    .and_then(Value::as_str)
                    .cmp(&b.get("domain").and_then(Value::as_str))
            })
    });
    Value::Array(rows)
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

fn run_bridge(
    listener: TcpListener,
    session_id: String,
    stop: Arc<AtomicBool>,
    errors: Arc<Mutex<Vec<String>>>,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(error) = handle_bridge_stream(stream, &session_id) {
                    errors
                        .lock()
                        .expect("browser_script bridge error registry poisoned")
                        .push(format!("{error:#}"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                errors
                    .lock()
                    .expect("browser_script bridge error registry poisoned")
                    .push(format!(
                        "accept browser_script bridge connection: {error:#}"
                    ));
                break;
            }
        }
    }
}

fn handle_bridge_stream(mut stream: TcpStream, session_id: &str) -> Result<()> {
    // The listener is nonblocking so the bridge thread can poll the stop flag.
    // On macOS, accepted streams can still surface EWOULDBLOCK on large writes
    // unless the child socket is forced back to blocking mode. Screenshots are
    // ordinary JSON responses, often hundreds of KB, so partial nonblocking
    // writes corrupt the bridge response seen by Python.
    stream
        .set_nonblocking(false)
        .context("set browser_script bridge stream blocking")?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(120)));
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let request: Value = serde_json::from_str(&line)?;
    let response = match bridge_request(session_id, &request) {
        Ok(value) => json!({ "ok": true, "result": value }),
        Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
    };
    let mut response_bytes = serde_json::to_vec(&response)?;
    response_bytes.push(b'\n');
    stream.write_all(&response_bytes)?;
    stream.flush()?;
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
        #[cfg(test)]
        "test_large_response" => {
            let bytes = request
                .get("bytes")
                .and_then(Value::as_u64)
                .unwrap_or(1_000_000) as usize;
            Ok(json!({ "blob": "x".repeat(bytes) }))
        }
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
        "meta" => {
            let meta = request.get("meta").and_then(Value::as_str).unwrap_or("");
            match meta {
                "status" => Ok(session.status_json()),
                "session" => Ok(json!({ "session_id": session.current_session_id })),
                "current_tab" => session.current_page_probe_mut(),
                "set_session" => {
                    let session_id = request
                        .get("session_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("set_session requires session_id"))?
                        .to_string();
                    let target_id = request
                        .get("target_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("set_session requires target_id"))?
                        .to_string();
                    session.current_session_id = Some(session_id.clone());
                    session.current_target_id = Some(target_id.clone());
                    session.connection_generation += 1;
                    for domain in ["Page", "DOM", "Runtime", "Network"] {
                        let _ =
                            session.cdp(&format!("{domain}.enable"), Some(&session_id), json!({}));
                    }
                    Ok(json!({
                        "session_id": session_id,
                        "target_id": target_id,
                        "browser": session.status_json(),
                    }))
                }
                "pending_dialog" => Ok(json!({ "dialog": null })),
                "drain_events" => Ok(json!({ "events": [] })),
                other => bail!("unknown browser_script bridge meta request: {other}"),
            }
        }
        "status" => Ok(session.status_json()),
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
    let encoded_helpers = general_purpose::STANDARD.encode(BROWSER_SCRIPT_HELPERS.as_bytes());
    Ok(format!(
        r#"
import base64, contextlib, io, json, os, pathlib, shutil, socket, sys, time, traceback, urllib.request

BRIDGE_PORT = {bridge_port}
CWD = pathlib.Path({cwd:?}).expanduser().resolve()
ARTIFACT_DIR = pathlib.Path({artifact_dir:?}).expanduser().resolve()
ARTIFACT_DIR.mkdir(parents=True, exist_ok=True)
VIRTUAL_HOME = pathlib.Path(os.environ.get("LLM_BROWSER_VIRTUAL_HOME") or str(ARTIFACT_DIR)).expanduser().resolve()
OUTPUTS_DIR = pathlib.Path(os.environ.get("LLM_BROWSER_OUTPUTS_DIR") or str(ARTIFACT_DIR)).expanduser().resolve()
OUTPUTS_DIR.mkdir(parents=True, exist_ok=True)

def _scan_artifact_files():
    files = set()
    for root in (ARTIFACT_DIR, OUTPUTS_DIR):
        if not root.exists():
            continue
        for path in root.rglob("*"):
            if path.is_file():
                files.add(str(path.resolve()))
    return files

__initial_artifact_files = _scan_artifact_files()
__outputs = []
__artifacts = []
__images = []

def _jsonable(value):
    try:
        json.dumps(value)
        return value
    except TypeError:
        return repr(value)

def _bridge(payload):
    with socket.create_connection(("127.0.0.1", BRIDGE_PORT), timeout=120) as sock:
        sock.sendall((json.dumps(payload) + "\n").encode())
        raw_response = bytearray()
        while not raw_response.endswith(b"\n"):
            data = sock.recv(65536)
            if not data:
                break
            raw_response.extend(data)
    if not raw_response:
        raise RuntimeError("browser bridge closed before response")
    try:
        response = json.loads(raw_response.decode())
    except json.JSONDecodeError as exc:
        sample = raw_response[:200].decode("utf-8", "replace")
        raise RuntimeError(f"browser bridge returned invalid JSON: {{exc}}; first bytes: {{sample!r}}") from exc
    if not response.get("ok"):
        raise RuntimeError(response.get("error") or "browser bridge failed")
    return response.get("result")

def _load_browser_script_helpers():
    source = base64.b64decode({encoded_helpers:?}).decode()
    exec(compile(source, "<browser_script_helpers>", "exec"), globals())

def _artifact_meta(path, kind="file", mime_type=None):
    meta = {{"path": str(path), "kind": kind}}
    if mime_type:
        meta["mime_type"] = mime_type
    return meta

def _remember_artifact(path, kind="file", mime_type=None):
    path = pathlib.Path(path).expanduser().resolve()
    path_text = str(path)
    for artifact in __artifacts:
        if artifact.get("path") == path_text:
            return artifact
    meta = _artifact_meta(path, kind, mime_type)
    __artifacts.append(meta)
    return meta

def _auto_collect_artifacts():
    image_paths = {{str(pathlib.Path(image.get("path", "")).expanduser().resolve()) for image in __images if image.get("path")}}
    for root in (ARTIFACT_DIR, OUTPUTS_DIR):
        if not root.exists():
            continue
        for path in sorted(root.rglob("*")):
            if not path.is_file():
                continue
            resolved = str(path.resolve())
            if resolved in __initial_artifact_files:
                continue
            if resolved in image_paths:
                continue
            _remember_artifact(path)

def copy_artifact(path, kind="file"):
    src = pathlib.Path(path).expanduser()
    dest = ARTIFACT_DIR / src.name
    if src.resolve() != dest.resolve():
        shutil.copy2(src, dest)
    meta = _remember_artifact(dest, kind)
    return str(dest)

def emit_image(path, label=None):
    path = pathlib.Path(path).expanduser().resolve()
    meta = {{"path": str(path), "mime_type": "image/png", "detail": "auto", "label": label}}
    __images.append(meta)
    return meta

def audit_artifact(data=None, **requirements):
    checks = {{}}
    if data is not None:
        checks["has_data"] = data is not None and data != [] and data != {{}}
        if isinstance(data, list):
            checks["record_count"] = len(data)
    checks.update({{f"requirement_{{k}}": bool(v) for k, v in requirements.items()}})
    return {{"generated_by": "audit_artifact", "checks": checks, "ready_for_done": all(checks.values()) if checks else True}}

def artifact_root():
    return str(ARTIFACT_DIR)

def outputs_dir():
    OUTPUTS_DIR.mkdir(parents=True, exist_ok=True)
    return str(OUTPUTS_DIR)

def virtual_home():
    return str(VIRTUAL_HOME)

def session_metadata():
    return {{
        "cwd": str(CWD),
        "artifact_root": str(ARTIFACT_DIR),
        "outputs_dir": str(OUTPUTS_DIR),
        "virtual_home": str(VIRTUAL_HOME),
        "virtual_home_alias": "/home/user",
        "agent_workspace": str(pathlib.Path(agent_workspace())),
    }}

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
    code = code.replace("/home/user/outputs", outputs_dir()).replace("/home/user", virtual_home())
    exec(compile(code, "<browser_script>", "exec"), globals())

stdout = io.StringIO()
stderr = io.StringIO()
ok = True
error = None
try:
    with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
        _load_browser_script_helpers()
        load_agent_helpers()
        _run_user_code()
except Exception:
    ok = False
    error = traceback.format_exc()

text = stdout.getvalue()
if stderr.getvalue():
    text += ("\n" if text else "") + stderr.getvalue()

_auto_collect_artifacts()

result = {{
    "ok": ok,
    "text": text[-{SCRIPT_MAX_OUTPUT_CHARS}:],
    "error": error,
    "data": {{}},
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
    fn browser_events_are_transition_based_not_heartbeats() {
        let mut session = BrowserSession::default();
        assert!(session.browser_events().is_empty());

        session.mode = BrowserMode::Local;
        session.endpoint = Some(Endpoint {
            kind: "local".to_string(),
            http_url: Some("http://127.0.0.1:9222".to_string()),
            ws_url: "ws://127.0.0.1:9222/devtools/browser/example".to_string(),
            candidate_id: Some("local-1".to_string()),
        });

        let first = session.browser_events();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["type"], "browser.disconnected");
        assert!(session.browser_events().is_empty());

        let connected = json!({
            "status": "connected",
            "target_id": "target-1",
            "session_id": "session-1",
            "generation": 1,
        });
        session.last_emitted_browser_payload = None;
        assert_eq!(session.browser_event_type(&connected), "browser.connected");
        session.last_emitted_browser_payload = Some(connected.clone());
        assert_eq!(
            session.browser_event_type(&json!({
                "status": "connected",
                "target_id": "target-2",
                "session_id": "session-1",
                "generation": 1,
            })),
            "browser.target_changed"
        );
        assert_eq!(
            session.browser_event_type(&json!({
                "status": "connected",
                "target_id": "target-1",
                "session_id": "session-2",
                "generation": 2,
            })),
            "browser.reconnected"
        );
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
        assert!(text.contains("next:"));
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
            r#"
print('hello')
manual = pathlib.Path('/home/user/outputs/manual.json')
manual.parent.mkdir(parents=True, exist_ok=True)
manual.write_text('{"manual": true}', encoding='utf-8')
answer = pathlib.Path(outputs_dir()) / 'answer.json'
answer.write_text(json.dumps({'ok': True}), encoding='utf-8')
"#,
            10,
        )
        .unwrap();
        assert!(output.ok, "{:?}", output.error);
        assert!(output.text.contains("hello"));
        assert!(output
            .artifacts
            .iter()
            .any(|artifact| artifact["path"].as_str().unwrap().ends_with("answer.json")));
        assert!(output
            .artifacts
            .iter()
            .any(|artifact| artifact["path"].as_str().unwrap().ends_with("manual.json")));
        for artifact in &output.artifacts {
            assert!(
                Path::new(artifact["path"].as_str().unwrap()).is_absolute(),
                "artifact path should be absolute: {artifact}"
            );
        }
    }

    #[test]
    fn browser_script_bridge_handles_large_json_responses() {
        let temp = tempfile::tempdir().unwrap();
        let session_id = "bridge-large-json";
        {
            let mut sessions = sessions()
                .lock()
                .expect("browser session registry poisoned");
            sessions.insert(session_id.to_string(), BrowserSession::default());
        }

        let output = run_browser_script(
            session_id,
            temp.path(),
            temp.path().join("artifacts"),
            r#"
data = _bridge({"kind": "test_large_response", "bytes": 2_000_000})
assert len(data["blob"]) == 2_000_000, len(data["blob"])
print("large response ok", len(data["blob"]))
"#,
            10,
        )
        .unwrap();

        assert!(output.ok, "{:?}\n{}", output.error, output.text);
        assert!(output.text.contains("large response ok 2000000"));
        assert!(
            output.browser_events.is_empty(),
            "unexpected bridge events: {:?}",
            output.browser_events
        );

        sessions()
            .lock()
            .expect("browser session registry poisoned")
            .remove(session_id);
    }

    #[test]
    fn local_profiles_command_uses_native_rust_detector() {
        let temp = tempfile::tempdir().unwrap();
        let output = run_browser_command(
            "profiles-list",
            temp.path(),
            temp.path(),
            "browser local profiles --json",
        )
        .unwrap();
        assert_eq!(output.content["source"], "rust-local-filesystem");
        assert!(output.content["profiles"].is_array());
        assert!(!output.content.to_string().contains("profile-use"));
    }

    #[test]
    fn stale_devtools_active_port_is_not_connectable() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("DevToolsActivePort"),
            "9\n/devtools/browser/stale\n",
        )
        .unwrap();
        let candidates =
            local_candidates_from_roots(vec![("Test Chrome", temp.path().to_path_buf())], &[]);
        assert_eq!(candidates.len(), 1);
        assert!(!candidates[0].connectable);
        assert_eq!(candidates[0].state, "stale-port");
        assert!(candidates[0].stale);
        assert!(candidates[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("DevToolsActivePort"));
    }

    #[test]
    fn local_profiles_inspect_missing_profile_never_mentions_external_cli() {
        let temp = tempfile::tempdir().unwrap();
        let output = run_browser_command(
            "profiles-inspect-missing",
            temp.path(),
            temp.path(),
            "browser local profiles inspect 'missing profile' --domains-only",
        )
        .unwrap();
        assert_eq!(output.content["status"], "failed");
        assert!(output.content.get("available_profiles").is_some());
        assert!(!output.content.to_string().contains("profile-use"));
    }

    #[test]
    fn local_profile_detection_reads_local_state_names_and_stable_ids() {
        let temp = tempfile::tempdir().unwrap();
        let user_data_dir = temp.path().join("Chrome");
        fs::create_dir_all(user_data_dir.join("Default")).unwrap();
        fs::create_dir_all(user_data_dir.join("Profile 10")).unwrap();
        fs::write(user_data_dir.join("Default/Preferences"), "{}").unwrap();
        fs::write(user_data_dir.join("Profile 10/Preferences"), "{}").unwrap();
        fs::write(
            user_data_dir.join("Local State"),
            r#"{
              "profile": {
                "info_cache": {
                  "Default": { "name": "Personal" },
                  "Profile 10": { "name": "Work" }
                }
              }
            }"#,
        )
        .unwrap();
        let profiles = detect_profiles_from_installs(vec![LocalBrowserInstall {
            browser_name: "Google Chrome".to_string(),
            browser_path: temp.path().join("Chrome.app"),
            user_data_dir: user_data_dir.clone(),
        }]);
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].id, "google-chrome:Default");
        assert_eq!(profiles[0].profile_name, "Personal");
        assert_eq!(profiles[1].id, "google-chrome:Profile 10");
        assert_eq!(profiles[1].display_name, "Google Chrome - Work");
    }

    #[test]
    fn local_profile_resolution_requires_exact_id_when_names_collide() {
        let profiles = vec![
            LocalBrowserProfile {
                id: "chrome:Default".to_string(),
                browser_name: "Chrome".to_string(),
                browser_path: PathBuf::from("/chrome"),
                user_data_dir: PathBuf::from("/profiles/chrome"),
                profile_dir: "Default".to_string(),
                profile_name: "Work".to_string(),
                profile_path: PathBuf::from("/profiles/chrome/Default"),
                display_name: "Chrome - Work".to_string(),
            },
            LocalBrowserProfile {
                id: "brave:Default".to_string(),
                browser_name: "Brave".to_string(),
                browser_path: PathBuf::from("/brave"),
                user_data_dir: PathBuf::from("/profiles/brave"),
                profile_dir: "Default".to_string(),
                profile_name: "Work".to_string(),
                profile_path: PathBuf::from("/profiles/brave/Default"),
                display_name: "Brave - Work".to_string(),
            },
        ];
        assert!(resolve_local_profile(&profiles, "Work")
            .unwrap_err()
            .to_string()
            .contains("multiple local profiles"));
        assert_eq!(
            resolve_local_profile(&profiles, "brave:Default")
                .unwrap()
                .browser_name,
            "Brave"
        );
    }

    #[test]
    fn profile_inspection_copy_skips_heavy_and_lock_files() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        let dst = temp.path().join("dst");
        fs::create_dir_all(src.join("Network")).unwrap();
        fs::create_dir_all(src.join("IndexedDB")).unwrap();
        fs::write(src.join("Preferences"), "{}").unwrap();
        fs::write(src.join("History"), "skip").unwrap();
        fs::write(src.join("SingletonLock"), "skip").unwrap();
        fs::write(src.join("Network/Cookies"), "copy").unwrap();
        fs::write(src.join("IndexedDB/data"), "skip").unwrap();
        copy_profile_dir_for_inspection(&src, &dst).unwrap();
        assert!(dst.join("Preferences").exists());
        assert!(dst.join("Network/Cookies").exists());
        assert!(!dst.join("History").exists());
        assert!(!dst.join("SingletonLock").exists());
        assert!(!dst.join("IndexedDB").exists());
    }

    #[test]
    fn cookie_domain_summary_never_returns_cookie_values() {
        let cookies = vec![
            json!({
                "name": "sid",
                "value": "secret",
                "domain": ".gusto.com",
                "session": false,
                "expires": 2000.0
            }),
            json!({
                "name": "tmp",
                "value": "secret2",
                "domain": "gusto.com",
                "session": true
            }),
            json!({
                "name": "other",
                "value": "secret3",
                "domain": "example.com",
                "session": false,
                "expires": 3000.0
            }),
        ];
        let summary = cookie_domain_summary(&cookies);
        let text = serde_json::to_string(&summary).unwrap();
        assert!(!text.contains("secret"));
        assert_eq!(summary[0]["domain"], "gusto.com");
        assert_eq!(summary[0]["count"], 2);
        assert_eq!(summary[0]["session_count"], 1);
        assert_eq!(summary[0]["persistent_count"], 1);
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
tid = new_tab("about:blank")
assert isinstance(tid, str) and tid, tid
assert current_tab()["targetId"] == tid, current_tab()
assert all(tab.get("targetId") for tab in list_tabs()), list_tabs()
switch_tab({"targetId": tid})
goto_url("about:blank")
js("""
(() => {
  document.title = "Browser Smoke";
  document.body.style.margin = "0";
  document.body.innerHTML = '<canvas id="ok" width="1280" height="900"></canvas>';
  const canvas = document.querySelector("#ok");
  canvas.style.display = "block";
  canvas.style.width = "1280px";
  canvas.style.height = "900px";
  const ctx = canvas.getContext("2d");
  const img = ctx.createImageData(canvas.width, canvas.height);
  let seed = 0x12345678;
  for (let i = 0; i < img.data.length; i += 4) {
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    img.data[i] = seed & 255;
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    img.data[i + 1] = seed & 255;
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    img.data[i + 2] = seed & 255;
    img.data[i + 3] = 255;
  }
  ctx.putImageData(img, 0, 0);
  return true;
})()
""")
wait_for_element("#ok")
time.sleep(0.5)
large = js("'x'.repeat(200000)")
assert len(large) == 200000, len(large)
info = page_info()
print(info)
screenshot("managed_smoke")
(pathlib.Path(outputs_dir()) / "managed-smoke.json").write_text(json.dumps(info), encoding="utf-8")
"##,
            30,
        )
        .unwrap();
        assert!(script.ok, "{:?}\n{}", script.error, script.text);
        let output_path = script
            .artifacts
            .iter()
            .find_map(|artifact| {
                artifact["path"]
                    .as_str()
                    .filter(|path| path.ends_with("managed-smoke.json"))
            })
            .expect("managed-smoke artifact");
        let output: Value =
            serde_json::from_str(&fs::read_to_string(output_path).unwrap()).unwrap();
        assert_eq!(output["title"], "Browser Smoke");
        assert!(
            !script.images.is_empty(),
            "expected screenshot image artifact"
        );

        cleanup_session(session_id);
    }

    #[test]
    #[ignore = "launches a dedicated local Chromium-family browser and attaches through remote CDP"]
    fn remote_cdp_smoke_attaches_recovers_and_preserves_target() {
        if chromium_candidate_paths(true).is_empty() {
            eprintln!("skipping remote CDP smoke: no Chromium-family browser found");
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let artifacts = temp.path().join("artifacts");
        let source_session = "remote-cdp-source";
        let remote_session = "remote-cdp-client";

        let connect = run_browser_command(
            source_session,
            temp.path(),
            &artifacts,
            "browser connect managed --headless",
        )
        .unwrap();
        assert_eq!(connect.content["status"], "connected");
        let http_url = connect.content["browser"]["endpoint"]["http_url"]
            .as_str()
            .expect("managed browser http url")
            .to_string();

        let script = run_browser_script(
            source_session,
            temp.path(),
            &artifacts,
            r##"
goto_url("data:text/html,<title>Remote CDP Smoke</title><h1 id='ok'>Remote CDP Smoke</h1>")
wait_for_element("#ok")
(pathlib.Path(outputs_dir()) / "remote-source.json").write_text(json.dumps(page_info()), encoding="utf-8")
"##,
            30,
        )
        .unwrap();
        assert!(script.ok, "{:?}\n{}", script.error, script.text);

        let connect_remote = run_browser_command(
            remote_session,
            temp.path(),
            &artifacts,
            &format!("browser connect remote-cdp --url {http_url}"),
        )
        .unwrap();
        assert_eq!(connect_remote.content["status"], "connected");
        assert_eq!(
            connect_remote.content["browser"]["owner"],
            BrowserOwner::External.as_str()
        );
        assert_eq!(connect_remote.content["browser"]["mode"], "remote-cdp");
        let before_target = connect_remote.content["browser"]["page"]["target_id"]
            .as_str()
            .expect("target id")
            .to_string();

        for command in [
            "browser recover reconnect-websocket",
            "browser recover reattach-same-target",
            "browser recover restart-runtime",
        ] {
            let recovered =
                run_browser_command(remote_session, temp.path(), &artifacts, command).unwrap();
            assert_eq!(
                recovered.content["browser"]["connection"], "connected",
                "recovery command failed: {command}: {}",
                recovered.content
            );
            assert_eq!(
                recovered.content["browser"]["page"]["target_id"], before_target,
                "target changed after {command}"
            );
        }

        let probe = run_browser_script(
            remote_session,
            temp.path(),
            &artifacts,
            r##"
info = page_info()
(pathlib.Path(outputs_dir()) / "remote-cdp-smoke.json").write_text(json.dumps(info), encoding="utf-8")
"##,
            30,
        )
        .unwrap();
        assert!(probe.ok, "{:?}\n{}", probe.error, probe.text);
        let output_path = probe
            .artifacts
            .iter()
            .find_map(|artifact| {
                artifact["path"]
                    .as_str()
                    .filter(|path| path.ends_with("remote-cdp-smoke.json"))
            })
            .expect("remote-cdp-smoke artifact");
        let output: Value =
            serde_json::from_str(&fs::read_to_string(output_path).unwrap()).unwrap();
        assert_eq!(output["title"], "Remote CDP Smoke");

        let ownership = run_browser_command(
            remote_session,
            temp.path(),
            &artifacts,
            "browser runtime ownership --json",
        )
        .unwrap();
        assert_eq!(ownership.content["owner"], BrowserOwner::External.as_str());
        assert_eq!(
            ownership.content["safe_actions"]["restart_owned_browser"],
            false
        );

        cleanup_session(remote_session);
        cleanup_session(source_session);
    }
}
