//! Rust-owned browser control plane for browser-use terminal.
//!
//! The LLM-facing split is intentional:
//! - `browser` controls connection/lifecycle/debug state.
//! - `browser_script` runs fresh Python for page interaction through this
//!   Rust-held CDP connection.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
const BROWSER_SCRIPT_INITIAL_WAIT_MS: u64 = 750;
const BROWSER_SCRIPT_DEFAULT_OBSERVE_MS: u64 = 1_000;
const BROWSER_SCRIPT_HELPERS: &str = include_str!("browser_script_helpers.py");

#[derive(Debug)]
pub struct BrowserCommandOutput {
    pub content: Value,
    pub events: Vec<Value>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct BrowserScriptOutput {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_observe_ms: Option<u64>,
    pub text: String,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnosis: Option<BrowserIssueDiagnosis>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BrowserIssueDiagnosis {
    pub summary: String,
    pub what_happened: String,
    pub next_step: String,
    pub browser_usable: bool,
    pub page_usable: bool,
    pub error_kind: String,
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
    session_id: Option<String>,
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
            session_id: None,
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
static BROWSER_SCRIPT_RUNS: OnceLock<Mutex<HashMap<String, BrowserScriptRun>>> = OnceLock::new();
static BROWSER_SCRIPT_RUN_COUNTER: AtomicU64 = AtomicU64::new(1);

struct BrowserScriptRun {
    id: String,
    session_id: String,
    child: Child,
    stdout_reader: Option<thread::JoinHandle<Vec<u8>>>,
    stderr_reader: Option<thread::JoinHandle<Vec<u8>>>,
    bridge_stop: Arc<AtomicBool>,
    bridge: Option<thread::JoinHandle<()>>,
    bridge_errors: Arc<Mutex<Vec<String>>>,
    stream_path: PathBuf,
    stream_offset: u64,
    started_at_ms: u128,
    timeout_seconds: u64,
    deadline: Instant,
}

#[derive(Default)]
struct BrowserScriptDelta {
    text: String,
    outputs: Vec<Value>,
    artifacts: Vec<Value>,
    images: Vec<Value>,
    browser_events: Vec<Value>,
    consumed_bytes: u64,
}

fn browser_script_runs() -> &'static Mutex<HashMap<String, BrowserScriptRun>> {
    BROWSER_SCRIPT_RUNS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn active_browser_script_runs_json(session_id: &str) -> Value {
    let runs = browser_script_runs()
        .lock()
        .expect("browser_script run registry poisoned");
    Value::Array(
        runs.values()
            .filter(|run| run.session_id == session_id)
            .map(|run| {
                json!({
                    "run_id": run.id,
                    "status": "running",
                    "started_at_ms": run.started_at_ms as u64,
                    "next_step": format!("browser_script action=observe run_id={}", run.id),
                })
            })
            .collect(),
    )
}

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

    if argv.first().map(String::as_str) == Some("script") {
        {
            let mut sessions = sessions()
                .lock()
                .expect("browser session registry poisoned");
            let session = sessions.entry(session_id.to_string()).or_default();
            session.session_id = Some(session_id.to_string());
            session.log(format!("browser {}", argv.join(" ")));
        }
        let content = dispatch_script_runtime(session_id, &argv)?;
        let mut sessions = sessions()
            .lock()
            .expect("browser session registry poisoned");
        let events = sessions
            .get_mut(session_id)
            .map(BrowserSession::browser_events)
            .unwrap_or_default();
        return Ok(BrowserCommandOutput { events, content });
    }

    let mut sessions = sessions()
        .lock()
        .expect("browser session registry poisoned");
    let session = sessions.entry(session_id.to_string()).or_default();
    session.session_id = Some(session_id.to_string());
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
    let mut run = spawn_browser_script(session_id, cwd, artifact_dir, code, timeout_seconds)?;
    loop {
        if run.child.try_wait()?.is_some() {
            return finish_browser_script_run(run, false);
        }
        if Instant::now() >= run.deadline {
            return finish_browser_script_run(run, true);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub fn start_browser_script(
    session_id: &str,
    cwd: impl AsRef<Path>,
    artifact_dir: impl AsRef<Path>,
    code: &str,
    timeout_seconds: u64,
) -> Result<BrowserScriptOutput> {
    let mut run = spawn_browser_script(session_id, cwd, artifact_dir, code, timeout_seconds)?;
    let initial_deadline = Instant::now() + Duration::from_millis(BROWSER_SCRIPT_INITIAL_WAIT_MS);
    loop {
        if run.child.try_wait()?.is_some() {
            return finish_browser_script_run(run, false);
        }
        if Instant::now() >= run.deadline {
            return finish_browser_script_run(run, true);
        }
        if Instant::now() >= initial_deadline {
            let mut delta = drain_browser_script_delta(&mut run).unwrap_or_default();
            let text = if delta.text.trim().is_empty() {
                format!(
                    "browser_script is still running.\nrun_id: {}\nNext: call browser_script with action=\"observe\" and run_id=\"{}\".",
                    run.id, run.id
                )
            } else {
                format!(
                    "{}\n\nbrowser_script is still running.\nrun_id: {}\nNext: observe this run again.",
                    delta.text.trim_end(),
                    run.id
                )
            };
            let run_id = run.id.clone();
            let output = BrowserScriptOutput {
                ok: true,
                status: Some("running".to_string()),
                run_id: Some(run_id.clone()),
                next_observe_ms: Some(BROWSER_SCRIPT_DEFAULT_OBSERVE_MS),
                text,
                outputs: std::mem::take(&mut delta.outputs),
                artifacts: std::mem::take(&mut delta.artifacts),
                images: std::mem::take(&mut delta.images),
                browser_events: std::mem::take(&mut delta.browser_events),
                ..Default::default()
            };
            browser_script_runs()
                .lock()
                .expect("browser_script run registry poisoned")
                .insert(run_id, run);
            return Ok(output);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub fn observe_browser_script(
    session_id: &str,
    run_id: &str,
    observe_timeout_ms: u64,
) -> Result<BrowserScriptOutput> {
    let mut run = browser_script_runs()
        .lock()
        .expect("browser_script run registry poisoned")
        .remove(run_id)
        .ok_or_else(|| anyhow!("unknown browser_script run_id {run_id:?}"))?;
    if run.session_id != session_id {
        let owner = run.session_id.clone();
        browser_script_runs()
            .lock()
            .expect("browser_script run registry poisoned")
            .insert(run.id.clone(), run);
        bail!("browser_script run {run_id} belongs to a different session ({owner})");
    }

    let timeout = Duration::from_millis(observe_timeout_ms.max(1));
    let observe_deadline = Instant::now() + timeout;
    loop {
        if run.child.try_wait()?.is_some() {
            return finish_browser_script_run(run, false);
        }
        if Instant::now() >= run.deadline {
            return finish_browser_script_run(run, true);
        }
        let delta = drain_browser_script_delta(&mut run).unwrap_or_default();
        if delta.has_content() {
            let output = browser_script_running_output(&run, Some(delta), observe_timeout_ms);
            browser_script_runs()
                .lock()
                .expect("browser_script run registry poisoned")
                .insert(run.id.clone(), run);
            return Ok(output);
        }
        if Instant::now() >= observe_deadline {
            let output = browser_script_running_output(&run, None, observe_timeout_ms);
            browser_script_runs()
                .lock()
                .expect("browser_script run registry poisoned")
                .insert(run.id.clone(), run);
            return Ok(output);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub fn cancel_browser_script(session_id: &str, run_id: &str) -> Result<BrowserScriptOutput> {
    let mut run = browser_script_runs()
        .lock()
        .expect("browser_script run registry poisoned")
        .remove(run_id)
        .ok_or_else(|| anyhow!("unknown browser_script run_id {run_id:?}"))?;
    if run.session_id != session_id {
        let owner = run.session_id.clone();
        browser_script_runs()
            .lock()
            .expect("browser_script run registry poisoned")
            .insert(run.id.clone(), run);
        bail!("browser_script run {run_id} belongs to a different session ({owner})");
    }
    let _ = run.child.kill();
    finish_cancelled_browser_script_run(run)
}

fn spawn_browser_script(
    session_id: &str,
    cwd: impl AsRef<Path>,
    artifact_dir: impl AsRef<Path>,
    code: &str,
    timeout_seconds: u64,
) -> Result<BrowserScriptRun> {
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

    let agent_workspace_dir = agent_workspace_dir_for(artifact_dir.as_ref());
    let domain_skill_roots = domain_skill_roots_for(&agent_workspace_dir);
    let run_id = new_browser_script_run_id();
    let stream_path = artifact_dir
        .as_ref()
        .join(format!(".{run_id}.events.ndjson"));
    let prelude = browser_script_prelude(
        bridge_addr.port(),
        cwd.as_ref(),
        artifact_dir.as_ref(),
        &agent_workspace_dir,
        &domain_skill_roots,
        &stream_path,
        code,
    )?;
    let mut command = browser_script_python_command();
    let mut child = command
        .arg("-c")
        .arg(prelude)
        .current_dir(cwd.as_ref())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn browser_script python")?;
    let stdout_reader = child.stdout.take().map(read_browser_script_stdout);
    let stderr_reader = child.stderr.take().map(read_browser_script_stderr);
    Ok(BrowserScriptRun {
        id: run_id,
        session_id: session_id.to_string(),
        child,
        stdout_reader,
        stderr_reader,
        bridge_stop: stop,
        bridge: Some(bridge),
        bridge_errors,
        stream_path,
        stream_offset: 0,
        started_at_ms: unix_time_ms(),
        timeout_seconds: timeout_seconds.max(1),
        deadline: Instant::now() + Duration::from_secs(timeout_seconds.max(1)),
    })
}

fn finish_browser_script_run(
    mut run: BrowserScriptRun,
    timed_out: bool,
) -> Result<BrowserScriptOutput> {
    if timed_out {
        let _ = run.child.kill();
    }
    let _ = run.child.wait().context("wait for browser_script python")?;
    run.bridge_stop.store(true, Ordering::SeqCst);
    let bridge_joined = run
        .bridge
        .take()
        .map(|bridge| join_bridge_with_timeout(bridge, Duration::from_secs(5)))
        .unwrap_or(true);
    let mut bridge_errors = run
        .bridge_errors
        .lock()
        .expect("browser_script bridge error registry poisoned")
        .clone();
    if !bridge_joined {
        bridge_errors.push(
            "browser_script bridge did not stop within 5 seconds after child exit".to_string(),
        );
    }
    let stdout = join_reader(run.stdout_reader.take());
    let stderr = join_reader(run.stderr_reader.take());
    let mut delta = drain_browser_script_delta(&mut run).unwrap_or_default();

    if timed_out {
        let error = format!(
            "browser_script timed out after {} seconds",
            run.timeout_seconds
        );
        return Ok(BrowserScriptOutput {
            ok: false,
            status: Some("failed".to_string()),
            run_id: Some(run.id),
            text: std::mem::take(&mut delta.text),
            diagnosis: Some(browser_script_failure_diagnosis(&run.session_id, &error)),
            error: Some(error),
            outputs: std::mem::take(&mut delta.outputs),
            artifacts: std::mem::take(&mut delta.artifacts),
            images: std::mem::take(&mut delta.images),
            browser_events: std::mem::take(&mut delta.browser_events),
            ..Default::default()
        });
    }

    let marker = "__BROWSER_SCRIPT_RESULT__";
    let result_line = stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(marker))
        .map(str::trim);
    let Some(result_line) = result_line else {
        let error = if stderr.trim().is_empty() {
            "browser_script did not emit a result".to_string()
        } else {
            stderr
        };
        return Ok(BrowserScriptOutput {
            ok: false,
            status: Some("failed".to_string()),
            run_id: Some(run.id),
            text: if delta.text.trim().is_empty() {
                truncate_text(&stdout, SCRIPT_MAX_OUTPUT_CHARS)
            } else {
                std::mem::take(&mut delta.text)
            },
            diagnosis: Some(browser_script_failure_diagnosis(&run.session_id, &error)),
            error: Some(error),
            outputs: std::mem::take(&mut delta.outputs),
            artifacts: std::mem::take(&mut delta.artifacts),
            images: std::mem::take(&mut delta.images),
            browser_events: std::mem::take(&mut delta.browser_events),
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
    if !delta.text.trim().is_empty() {
        response.text = std::mem::take(&mut delta.text);
    }
    if !delta.outputs.is_empty() {
        response.outputs = std::mem::take(&mut delta.outputs);
    }
    if !delta.artifacts.is_empty() {
        response.artifacts = std::mem::take(&mut delta.artifacts);
    }
    if !delta.images.is_empty() {
        response.images = std::mem::take(&mut delta.images);
    }
    if !delta.browser_events.is_empty() {
        response
            .browser_events
            .extend(std::mem::take(&mut delta.browser_events));
    }
    if !response.ok && response.diagnosis.is_none() {
        let error = response
            .error
            .as_deref()
            .unwrap_or("browser_script failed without an error message");
        response.diagnosis = Some(browser_script_failure_diagnosis(&run.session_id, error));
    }
    response.status = Some(if response.ok { "finished" } else { "failed" }.to_string());
    response.run_id = Some(run.id);
    Ok(response)
}

fn finish_cancelled_browser_script_run(mut run: BrowserScriptRun) -> Result<BrowserScriptOutput> {
    let _ = run.child.wait().context("wait for browser_script python")?;
    run.bridge_stop.store(true, Ordering::SeqCst);
    if let Some(bridge) = run.bridge.take() {
        let _ = join_bridge_with_timeout(bridge, Duration::from_secs(5));
    }
    let _ = join_reader(run.stdout_reader.take());
    let _ = join_reader(run.stderr_reader.take());
    let mut delta = drain_browser_script_delta(&mut run).unwrap_or_default();
    let text = if delta.text.trim().is_empty() {
        "browser_script cancelled. Partial images/artifacts are preserved above.".to_string()
    } else {
        format!(
            "{}\n\nbrowser_script cancelled. Partial images/artifacts are preserved above.",
            delta.text.trim_end()
        )
    };
    Ok(BrowserScriptOutput {
        ok: true,
        status: Some("cancelled".to_string()),
        run_id: Some(run.id),
        text,
        outputs: std::mem::take(&mut delta.outputs),
        artifacts: std::mem::take(&mut delta.artifacts),
        images: std::mem::take(&mut delta.images),
        browser_events: std::mem::take(&mut delta.browser_events),
        ..Default::default()
    })
}

#[derive(Debug, Default)]
struct BrowserIssueState {
    browser_connected: bool,
    page_usable: bool,
    next_step: Option<String>,
}

fn browser_script_failure_diagnosis(session_id: &str, error: &str) -> BrowserIssueDiagnosis {
    let state = browser_issue_state_for_session(session_id);
    browser_issue_diagnosis(
        classify_browser_script_failure(error),
        state.browser_connected,
        state.page_usable,
        state.next_step.as_deref(),
    )
}

fn browser_issue_state_for_session(session_id: &str) -> BrowserIssueState {
    let Ok(sessions) = sessions().lock() else {
        return BrowserIssueState::default();
    };
    let Some(session) = sessions.get(session_id) else {
        return BrowserIssueState::default();
    };
    let browser_connected = session.connection.is_some();
    BrowserIssueState {
        browser_connected,
        page_usable: browser_connected
            && session.current_target_id.is_some()
            && session.current_session_id.is_some(),
        next_step: session.next_step().map(ToOwned::to_owned),
    }
}

fn new_browser_script_run_id() -> String {
    let n = BROWSER_SCRIPT_RUN_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("bs-{}-{n}", unix_time_ms())
}

fn read_browser_script_stdout(mut stdout: ChildStdout) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    })
}

fn read_browser_script_stderr(mut stderr: ChildStderr) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    })
}

fn join_reader(reader: Option<thread::JoinHandle<Vec<u8>>>) -> String {
    reader
        .and_then(|reader| reader.join().ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        .unwrap_or_default()
}

impl BrowserScriptDelta {
    fn has_content(&self) -> bool {
        !self.text.is_empty()
            || !self.outputs.is_empty()
            || !self.artifacts.is_empty()
            || !self.images.is_empty()
            || !self.browser_events.is_empty()
    }
}

fn browser_script_running_output(
    run: &BrowserScriptRun,
    delta: Option<BrowserScriptDelta>,
    no_new_wait_ms: u64,
) -> BrowserScriptOutput {
    let mut output = BrowserScriptOutput {
        ok: true,
        status: Some("running".to_string()),
        run_id: Some(run.id.clone()),
        next_observe_ms: Some(BROWSER_SCRIPT_DEFAULT_OBSERVE_MS),
        text: format!(
            "browser_script is still running.\nrun_id: {}\nNext: call browser_script with action=\"observe\" and run_id=\"{}\".",
            run.id, run.id
        ),
        ..Default::default()
    };
    if let Some(mut delta) = delta {
        if !delta.text.trim().is_empty() {
            output.text = format!(
                "{}\n\nbrowser_script is still running.\nrun_id: {}\nNext: observe this run again.",
                delta.text.trim_end(),
                run.id
            );
        }
        output.outputs = std::mem::take(&mut delta.outputs);
        output.artifacts = std::mem::take(&mut delta.artifacts);
        output.images = std::mem::take(&mut delta.images);
        output.browser_events = std::mem::take(&mut delta.browser_events);
    } else {
        output.text = format!(
            "browser_script is still running.\nNo new output in the last {} ms.\nrun_id: {}\nNext: observe this run again.",
            no_new_wait_ms, run.id
        );
    }
    output
}

fn drain_browser_script_delta(run: &mut BrowserScriptRun) -> Result<BrowserScriptDelta> {
    let mut file = match File::open(&run.stream_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BrowserScriptDelta::default());
        }
        Err(error) => return Err(error.into()),
    };
    file.seek(SeekFrom::Start(run.stream_offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Ok(BrowserScriptDelta::default());
    }
    let mut delta = BrowserScriptDelta::default();
    let mut consumed = 0usize;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        }
        consumed += line.len();
        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_slice(trimmed)?;
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "stdout" | "stderr" => {
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    delta.text.push_str(text);
                }
            }
            "output" => delta
                .outputs
                .push(value.get("output").cloned().unwrap_or(value)),
            "artifact" => {
                if let Some(artifact) = value.get("artifact").cloned() {
                    delta.artifacts.push(artifact);
                }
            }
            "image" => {
                if let Some(image) = value.get("image").cloned() {
                    delta.images.push(image);
                }
            }
            "browser" => {
                if let Some(event) = value.get("event").cloned() {
                    delta.browser_events.push(event);
                }
            }
            _ => {}
        }
    }
    run.stream_offset = run.stream_offset.saturating_add(consumed as u64);
    delta.consumed_bytes = consumed as u64;
    Ok(delta)
}

fn browser_script_python_command() -> Command {
    if let Some(configured) = nonempty_os_var("LLM_BROWSER_BROWSER_SCRIPT_PYTHON") {
        return Command::new(configured);
    }
    if let Some(venv) = nonempty_os_var("VIRTUAL_ENV") {
        let candidate = venv_python_path(Path::new(&venv));
        if candidate.is_file() {
            return Command::new(candidate);
        }
    }
    if let Some(repo_root) = repo_root_from_manifest() {
        let candidate = venv_python_path(&repo_root.join(".venv"));
        if candidate.is_file() {
            return Command::new(candidate);
        }
        if repo_root.join("pyproject.toml").is_file() && command_exists("uv") {
            let mut command = Command::new("uv");
            command
                .arg("run")
                .arg("--project")
                .arg(repo_root)
                .arg("python");
            return command;
        }
    }
    Command::new("python3")
}

fn nonempty_os_var(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn venv_python_path(venv: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        venv.join("Scripts").join("python.exe")
    }
    #[cfg(not(windows))]
    {
        venv.join("bin").join("python")
    }
}

fn repo_root_from_manifest() -> Option<PathBuf> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return true;
            }
            #[cfg(windows)]
            {
                dir.join(format!("{name}.exe")).is_file()
            }
            #[cfg(not(windows))]
            {
                false
            }
        })
    })
}

fn join_bridge_with_timeout(bridge: thread::JoinHandle<()>, timeout: Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = bridge.join();
        let _ = tx.send(());
    });
    rx.recv_timeout(timeout).is_ok()
}

fn agent_workspace_dir_for(artifact_dir: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os("BH_AGENT_WORKSPACE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return path;
    }
    if artifact_dir.file_name().and_then(|name| name.to_str()) == Some("artifacts") {
        if let Some(state_dir) = artifact_dir.parent() {
            return state_dir.join("agent-workspace");
        }
    }
    if artifact_dir
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        == Some("artifacts")
    {
        if let Some(state_dir) = artifact_dir.parent().and_then(Path::parent) {
            return state_dir.join("agent-workspace");
        }
    }
    home_dir()
        .map(|home| home.join(".browser-use-terminal").join("agent-workspace"))
        .unwrap_or_else(|| PathBuf::from(".browser-use-terminal").join("agent-workspace"))
}

fn domain_skill_roots_for(agent_workspace_dir: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for var in ["BH_DOMAIN_SKILLS_ROOT", "BH_DOMAIN_SKILLS_DIR"] {
        if let Some(value) = std::env::var_os(var).filter(|value| !value.is_empty()) {
            for path in std::env::split_paths(&value) {
                push_unique_existing_dir(&mut roots, path);
            }
        }
    }
    push_unique_existing_dir(&mut roots, agent_workspace_dir.join("domain-skills"));
    if let Some(home) = home_dir() {
        push_unique_existing_dir(
            &mut roots,
            home.join(".browser-use-terminal")
                .join("agent-workspace")
                .join("domain-skills"),
        );
        push_unique_existing_dir(
            &mut roots,
            home.join("repos")
                .join("browser-harness")
                .join("agent-workspace")
                .join("domain-skills"),
        );
        push_unique_existing_dir(
            &mut roots,
            home.join("repos")
                .join("browser-harness")
                .join("domain-skills"),
        );
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(repo_root) = manifest_dir.parent().and_then(Path::parent) {
        push_unique_existing_dir(&mut roots, repo_root.join("domain-skills"));
    }
    roots
}

fn push_unique_existing_dir(roots: &mut Vec<PathBuf>, path: PathBuf) {
    if !path.is_dir() {
        return;
    }
    let key = fs::canonicalize(&path).unwrap_or(path);
    if !roots.iter().any(|existing| existing == &key) {
        roots.push(key);
    }
}

fn domain_skills_enabled() -> bool {
    match std::env::var("BH_DOMAIN_SKILLS") {
        Ok(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            !matches!(normalized.as_str(), "" | "0" | "false" | "no" | "off")
        }
        Err(_) => true,
    }
}

fn normalize_domain_like_browser(value: &str) -> String {
    let mut host = value.trim();
    if let Some(rest) = host.strip_prefix("https://") {
        host = rest;
    } else if let Some(rest) = host.strip_prefix("http://") {
        host = rest;
    }
    host = host
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(host)
        .split('@')
        .next_back()
        .unwrap_or(host)
        .split(':')
        .next()
        .unwrap_or(host);
    host.trim_start_matches("www.").to_ascii_lowercase()
}

fn domain_skill_aliases(value: &str) -> HashSet<String> {
    let host = normalize_domain_like_browser(value);
    let labels = host
        .split('.')
        .filter(|label| !label.is_empty())
        .collect::<Vec<_>>();
    let mut aliases = HashSet::from([host.clone(), host.replace('.', "-")]);
    if let Some(first) = labels.first() {
        aliases.insert((*first).to_string());
    }
    if labels.len() >= 2 {
        aliases.insert(labels[labels.len() - 2].to_string());
        aliases.insert(format!(
            "{}-{}",
            labels[labels.len() - 2],
            labels[labels.len() - 1]
        ));
    }
    if labels.len() >= 3 {
        aliases.insert(format!("{}-{}", labels[labels.len() - 2], labels[0]));
        aliases.insert(format!("{}-{}", labels[0], labels[labels.len() - 2]));
    }
    aliases
        .into_iter()
        .map(|alias| alias.replace('_', "-").to_ascii_lowercase())
        .collect()
}

fn domain_skill_matches(
    domain: &str,
    roots: &[PathBuf],
    include_content: bool,
    max_files: usize,
    max_bytes: usize,
) -> Result<Vec<Value>> {
    if !domain_skills_enabled() {
        return Ok(Vec::new());
    }
    let aliases = domain_skill_aliases(domain);
    let mut matches = Vec::new();
    let mut remaining = max_bytes;
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        let mut entries = entries.filter_map(|entry| entry.ok()).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let site = entry.file_name().to_string_lossy().to_string();
            let site_key = site.replace('_', "-").to_ascii_lowercase();
            if !aliases.contains(&site_key) {
                continue;
            }
            let mut files = collect_domain_skill_files(
                &entry.path(),
                include_content,
                max_files,
                &mut remaining,
            )?;
            if !files.is_empty() {
                files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                matches.push(json!({
                    "site": site,
                    "root": root.display().to_string(),
                    "files": files,
                }));
            }
        }
    }
    Ok(matches)
}

fn collect_domain_skill_files(
    site_dir: &Path,
    include_content: bool,
    max_files: usize,
    remaining: &mut usize,
) -> Result<Vec<Value>> {
    let mut stack = vec![site_dir.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut entries = entries.filter_map(|entry| entry.ok()).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(extension.as_str(), "md" | "py") {
                continue;
            }
            let name = path
                .strip_prefix(site_dir)
                .unwrap_or(&path)
                .display()
                .to_string();
            let mut item = json!({
                "name": name,
                "path": path.display().to_string(),
            });
            if include_content && *remaining > 0 {
                let content = fs::read_to_string(&path)
                    .unwrap_or_else(|error| format!("[failed to read domain skill: {error}]"));
                let take = content
                    .char_indices()
                    .map(|(idx, _)| idx)
                    .chain(std::iter::once(content.len()))
                    .take_while(|idx| *idx <= *remaining)
                    .last()
                    .unwrap_or(0);
                item["content"] = Value::String(content[..take].to_string());
                item["truncated"] = Value::Bool(take < content.len());
                *remaining = remaining.saturating_sub(take);
            }
            files.push(item);
            if files.len() >= max_files {
                return Ok(files);
            }
        }
    }
    Ok(files)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
}

pub fn cleanup_session(session_id: &str) -> usize {
    cancel_browser_script_runs_for_session(session_id);
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

fn cancel_browser_script_runs_for_session(session_id: &str) {
    let runs = {
        let mut registry = browser_script_runs()
            .lock()
            .expect("browser_script run registry poisoned");
        let run_ids = registry
            .iter()
            .filter_map(|(run_id, run)| (run.session_id == session_id).then(|| run_id.clone()))
            .collect::<Vec<_>>();
        run_ids
            .into_iter()
            .filter_map(|run_id| registry.remove(&run_id))
            .collect::<Vec<_>>()
    };
    for mut run in runs {
        let _ = run.child.kill();
        let _ = finish_cancelled_browser_script_run(run);
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
        "domain" => dispatch_domain(argv),
        "recover" => dispatch_recover(session, argv),
        "script" => {
            let session_id = session
                .session_id
                .as_deref()
                .ok_or_else(|| anyhow!("browser script runtime is missing session id"))?;
            dispatch_script_runtime(session_id, argv)
        }
        "runtime" => dispatch_runtime(session, argv),
        other => bail!("unknown browser command: {other}. Run `browser help`."),
    }
}

fn dispatch_script_runtime(session_id: &str, argv: &[String]) -> Result<Value> {
    match argv.get(1).map(String::as_str) {
        Some("runs") => Ok(json!({
            "status": "ok",
            "active_scripts": active_browser_script_runs_json(session_id),
        })),
        Some("cancel") => {
            let run_id = argv
                .get(2)
                .map(String::as_str)
                .ok_or_else(|| anyhow!("browser script cancel requires <run_id>"))?;
            let output = cancel_browser_script(session_id, run_id)?;
            Ok(json!({
                "status": output.status.unwrap_or_else(|| "cancelled".to_string()),
                "run_id": output.run_id,
                "text": output.text,
                "images": output.images,
                "artifacts": output.artifacts,
            }))
        }
        Some(other) => bail!("unknown browser script command: {other}"),
        None => bail!("browser script requires runs or cancel"),
    }
}

fn dispatch_domain(argv: &[String]) -> Result<Value> {
    match argv.get(1).map(String::as_str) {
        Some("skills") => {
            let domain = option_value(argv, "--domain")
                .or_else(|| argv.get(2).cloned())
                .ok_or_else(|| anyhow!("browser domain skills requires --domain <domain>"))?;
            let include_content = has_flag(argv, "--include-content");
            let roots = domain_skill_roots_for(
                &std::env::var_os("BH_AGENT_WORKSPACE")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        home_dir()
                            .map(|home| home.join(".browser-use-terminal").join("agent-workspace"))
                            .unwrap_or_else(|| {
                                PathBuf::from(".browser-use-terminal").join("agent-workspace")
                            })
                    }),
            );
            Ok(json!({
                "status": "ok",
                "domain": normalize_domain_like_browser(&domain),
                "enabled": domain_skills_enabled(),
                "roots": roots.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
                "matches": domain_skill_matches(&domain, &roots, include_content, 10, 120_000)?,
                "next_step": "If matches are present and the task is site-specific, read them before inventing selectors, private API routes, or flows.",
            }))
        }
        Some(other) => bail!("unknown browser domain command: {other}"),
        None => bail!("browser domain requires skills"),
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
            Ok(local_setup_user_action_response(
                opened, profile, open_error,
            ))
        }
        Some("profiles") => dispatch_local_profiles(argv),
        Some(other) => bail!("unknown browser local command: {other}"),
        None => bail!("browser local requires list, setup, or profiles"),
    }
}

fn local_setup_user_action_response(
    opened: bool,
    profile: Option<LocalBrowserProfile>,
    open_error: Option<String>,
) -> Value {
    json!({
        "status": "needs-user-action",
        "opened": opened,
        "url": "chrome://inspect/#remote-debugging",
        "profile": profile,
        "open_error": open_error,
        "instructions": [
            "In the browser/profile that opens, enable 'Allow remote debugging for this browser instance' if Chrome reports it is blocked.",
            "If Chrome shows an additional permission prompt, click Allow.",
            "Do not retry until the user confirms that permission is enabled, then run `browser connect local` again."
        ],
        "next_step": "Wait for user confirmation, then run browser connect local."
    })
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
            "last_issue": self.last_issue_diagnosis(),
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
            "last_issue": self.last_issue_diagnosis(),
            "active_scripts": self.session_id.as_deref().map(active_browser_script_runs_json).unwrap_or_default(),
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

    fn last_issue_diagnosis(&self) -> Option<BrowserIssueDiagnosis> {
        self.last_error_kind.as_deref().map(|kind| {
            browser_issue_diagnosis(
                kind,
                self.connection.is_some(),
                self.connection.is_some()
                    && self.current_target_id.is_some()
                    && self.current_session_id.is_some(),
                self.next_step(),
            )
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
        } else if matches!(
            self.last_error_kind.as_deref(),
            Some("permission-blocked" | "cdp-disabled")
        ) && self.mode == BrowserMode::Local
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
            let disabled = local_debugging_disabled_statuses();
            if !disabled.is_empty() {
                self.last_error =
                    Some("Chrome is open, but remote debugging is turned off".to_string());
                self.last_error_kind = Some("cdp-disabled".to_string());
                return Ok(json!({
                    "status": "blocked",
                    "state": "cdp-disabled",
                    "reason": "Chrome is open, but remote debugging is turned off for this browser instance.",
                    "local_browsers": disabled,
                    "next_step": "browser local setup",
                }));
            }
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
            if candidates
                .iter()
                .any(|candidate| candidate.state == "cdp-disabled")
            {
                self.last_error =
                    Some("Chrome is open, but remote debugging is turned off".to_string());
                self.last_error_kind = Some("cdp-disabled".to_string());
                return Ok(json!({
                    "status": "blocked",
                    "state": "cdp-disabled",
                    "reason": "Chrome is open, but remote debugging is turned off for this browser instance.",
                    "candidates": candidates,
                    "next_step": "browser local setup",
                }));
            }
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
                    "next_step": candidate.next_step.as_deref().unwrap_or("Open Chrome with this profile, then run browser connect local"),
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
        let debugging_disabled = local_debugging_disabled_statuses();
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
            "cdp_disabled_count": candidates.iter().filter(|candidate| candidate.state == "cdp-disabled").count(),
            "state": if candidates.iter().any(|candidate| candidate.connectable) {
                "reachable"
            } else if candidates.iter().any(|candidate| candidate.state == "cdp-disabled") {
                "cdp-disabled"
            } else if candidates.iter().any(|candidate| candidate.stale) {
                "stale-port"
            } else if !debugging_disabled.is_empty() {
                "cdp-disabled"
            } else {
                "browser-not-running"
            },
            "detail": if candidates.iter().any(|candidate| candidate.connectable) {
                "At least one local browser CDP endpoint is reachable."
            } else if candidates.iter().any(|candidate| candidate.state == "cdp-disabled")
                || !debugging_disabled.is_empty()
            {
                "Chrome is open, but remote debugging is turned off for this browser instance."
            } else if candidates.iter().any(|candidate| candidate.stale) {
                "DevToolsActivePort files exist, but their ports are not reachable. Chrome was likely closed or restarted."
            } else {
                "No local browser CDP endpoint is reachable."
            },
            "next_step": if candidates.iter().any(|candidate| candidate.connectable) {
                "browser connect local"
            } else if candidates.iter().any(|candidate| candidate.state == "cdp-disabled")
                || !debugging_disabled.is_empty()
            {
                "browser local setup"
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
        match connection.call(method, session_id, params.clone()) {
            Ok(value) => Ok(value),
            Err(error) => {
                let mut message = format!("{error:#}");
                let is_current_session = session_id.is_some()
                    && session_id == self.current_session_id.as_deref()
                    && self.current_target_id.is_some();
                if is_current_session && is_stale_session_error(&message) {
                    self.last_error = Some(message.clone());
                    self.last_error_kind = Some("session-gone".to_string());
                    self.last_session_id = self.current_session_id.take();

                    match self.reattach_same_target() {
                        Ok(recovery)
                            if recovery.get("status").and_then(Value::as_str)
                                == Some("reattached") =>
                        {
                            let retry_session_id = self.current_session_id.clone();
                            let retry = self.connection.as_mut().map_or_else(
                                || Err(anyhow!("browser connection was lost during reattach")),
                                |connection| {
                                    connection.call(
                                        method,
                                        retry_session_id.as_deref(),
                                        params.clone(),
                                    )
                                },
                            );
                            match retry {
                                Ok(value) => {
                                    self.last_error = None;
                                    self.last_error_kind = None;
                                    return Ok(value);
                                }
                                Err(retry_error) => {
                                    message = format!("{retry_error:#}");
                                }
                            }
                        }
                        Ok(recovery) => {
                            let failure = format!(
                                "CDP {method} failed because the current session is stale and reattach did not recover it: {message}; reattach result: {recovery}"
                            );
                            self.last_error = Some(failure.clone());
                            self.last_error_kind = Some("target-gone".to_string());
                            bail!(failure);
                        }
                        Err(recovery_error) => {
                            let failure = format!(
                                "CDP {method} failed because the current session is stale and reattach failed: {message}; recovery error: {recovery_error:#}"
                            );
                            self.last_error = Some(failure.clone());
                            self.last_error_kind = Some("session-gone".to_string());
                            bail!(failure);
                        }
                    }
                }
                let error_kind = classify_browser_error(&message);
                if matches!(error_kind, "browser-closed" | "websocket-dropped")
                    && self.endpoint.is_some()
                {
                    self.last_error = Some(message.clone());
                    self.last_error_kind = Some(error_kind.to_string());
                    match self.reconnect_websocket() {
                        Ok(_) => {
                            let retry_session_id = if is_current_session {
                                self.current_session_id.clone()
                            } else {
                                session_id.map(ToOwned::to_owned)
                            };
                            let retry = self.connection.as_mut().map_or_else(
                                || Err(anyhow!("browser connection was lost during reconnect")),
                                |connection| {
                                    connection.call(
                                        method,
                                        retry_session_id.as_deref(),
                                        params.clone(),
                                    )
                                },
                            );
                            match retry {
                                Ok(value) => {
                                    self.last_error = None;
                                    self.last_error_kind = None;
                                    return Ok(value);
                                }
                                Err(retry_error) => {
                                    message = format!("{retry_error:#}");
                                }
                            }
                        }
                        Err(reconnect_error) => {
                            message = format!(
                                "{message}; reconnect after dropped CDP websocket failed: {reconnect_error:#}"
                            );
                        }
                    }
                }
                let final_error_kind = classify_browser_error(&message);
                self.last_error = Some(message.clone());
                self.last_error_kind = Some(final_error_kind.to_string());
                if should_drop_browser_connection(final_error_kind) {
                    self.connection = None;
                    self.last_target_id = self.current_target_id.take();
                    self.last_session_id = self.current_session_id.take();
                }
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
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(20)));
        }
        MaybeTlsStream::Rustls(stream) => {
            let _ = stream.sock.set_read_timeout(Some(Duration::from_secs(20)));
            let _ = stream.sock.set_write_timeout(Some(Duration::from_secs(20)));
        }
        _ => {}
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
    } else if is_stale_session_error(message) {
        "session-gone"
    } else if (lower.contains("resource temporarily unavailable")
        || lower.contains("would block")
        || lower.contains("timed out"))
        && lower.contains("read cdp")
    {
        "cdp-read-timeout"
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

fn classify_browser_script_failure(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("browser_script timed out") {
        "browser-script-timeout"
    } else if lower.contains("browser_script did not emit a result") {
        "browser-script-no-result"
    } else if lower.contains("read cdp")
        || lower.contains("send cdp")
        || lower.contains("cdp websocket")
        || lower.contains("browser is not connected")
        || lower.contains("connection refused")
        || lower.contains("couldn't connect to server")
        || lower.contains("unable to connect")
        || lower.contains("operation timed out")
        || lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("websocket closed")
        || lower.contains("already closed")
        || (lower.contains("target")
            && (lower.contains("not found")
                || lower.contains("target-gone")
                || lower.contains("no target with given id")))
        || is_stale_session_error(message)
    {
        classify_browser_error(message)
    } else {
        "browser-script-error"
    }
}

fn browser_issue_diagnosis(
    error_kind: &str,
    browser_connected: bool,
    page_usable: bool,
    status_next_step: Option<&str>,
) -> BrowserIssueDiagnosis {
    let fallback_next_step = || {
        status_next_step
            .unwrap_or("Run browser status --json to check the connection before continuing.")
            .to_string()
    };
    let (summary, what_happened, next_step, browser_usable, page_usable) = match error_kind {
        "cdp-read-timeout" => (
            if page_usable {
                "Browser is still connected; the same page should still be usable."
            } else if browser_connected {
                "Browser is still connected, but the current page attachment is unclear."
            } else {
                "The CDP read timed out and browser state needs a status check."
            },
            "A CDP read for this browser_script call timed out or returned would-block while waiting for Chrome.",
            if page_usable {
                "Continue on the same page, but rerun a smaller browser_script chunk or resume from the last checkpoint.".to_string()
            } else {
                fallback_next_step()
            },
            browser_connected,
            page_usable,
        ),
        "browser-script-timeout" => (
            if page_usable {
                "The script timed out, but the browser page should still be reusable."
            } else if browser_connected {
                "The script timed out; browser is connected but page state needs checking."
            } else {
                "The script timed out and browser state needs a status check."
            },
            "The Python worker exceeded the browser_script timeout before returning a result.",
            if page_usable {
                "Retry with a shorter bounded script, write checkpoints to files, and continue from the last completed item.".to_string()
            } else {
                fallback_next_step()
            },
            browser_connected,
            page_usable,
        ),
        "browser-script-no-result" => (
            if page_usable {
                "The script exited without a result; the browser page should still be reusable."
            } else {
                "The script exited without a result and browser state needs checking."
            },
            "The Python worker exited before emitting the browser_script result marker.",
            if page_usable {
                "Fix the script so it completes normally, then rerun on the same page.".to_string()
            } else {
                fallback_next_step()
            },
            browser_connected,
            page_usable,
        ),
        "browser-script-error" => (
            if page_usable {
                "The script failed, but the browser page should still be reusable."
            } else if browser_connected {
                "The script failed; browser is connected but page state needs checking."
            } else {
                "The script failed and browser state needs a status check."
            },
            "The Python browser_script code raised an error before completing the call.",
            if page_usable {
                "Fix the Python/browser_script code and rerun; keep using the same page state.".to_string()
            } else {
                fallback_next_step()
            },
            browser_connected,
            page_usable,
        ),
        "session-gone" => (
            if browser_connected {
                "Browser is connected, but the current page session is stale."
            } else {
                "The current page session is stale and browser state needs recovery."
            },
            "Chrome reported that the CDP session id no longer exists for the target.",
            if browser_connected {
                "Run browser recover reattach-same-target, then continue on the recovered page."
                    .to_string()
            } else {
                fallback_next_step()
            },
            browser_connected,
            false,
        ),
        "target-gone" => (
            if browser_connected {
                "Browser is connected, but the previous tab or target is gone."
            } else {
                "The previous tab or target is gone and browser state needs recovery."
            },
            "Chrome reported that the controlled target no longer exists.",
            if browser_connected {
                "Select an existing tab or create a new tab, then continue from the last checkpoint."
                    .to_string()
            } else {
                fallback_next_step()
            },
            browser_connected,
            false,
        ),
        "permission-blocked" => (
            "Chrome rejected browser control.",
            "The browser endpoint returned a permission or 403 error for CDP control.",
            status_next_step.unwrap_or("Run browser local setup, then reconnect.").to_string(),
            false,
            false,
        ),
        "cdp-disabled" => (
            "Chrome is open, but remote debugging is turned off.",
            "Chrome is running, but it is not exposing a local CDP endpoint because remote debugging is disabled for this browser instance.",
            status_next_step
                .unwrap_or("Run browser local setup, enable remote debugging, then reconnect.")
                .to_string(),
            false,
            false,
        ),
        "browser-closed" | "websocket-dropped" | "browser-not-running" | "stale-port" => (
            "Browser connection is not usable until it is recovered.",
            "The CDP websocket was closed, reset, refused, or pointed at a stale browser endpoint.",
            fallback_next_step(),
            false,
            false,
        ),
        _ => (
            if page_usable {
                "The browser page may still be reusable, but the failure needs checking."
            } else {
                "Browser state is unclear after this failure."
            },
            "The browser tool reported an unclassified failure.",
            fallback_next_step(),
            browser_connected,
            page_usable,
        ),
    };
    BrowserIssueDiagnosis {
        summary: summary.to_string(),
        what_happened: what_happened.to_string(),
        next_step,
        browser_usable,
        page_usable,
        error_kind: error_kind.to_string(),
    }
}

fn should_drop_browser_connection(error_kind: &str) -> bool {
    matches!(error_kind, "browser-closed" | "websocket-dropped")
}

fn is_stale_session_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("session")
        && (lower.contains("not found")
            || lower.contains("no session")
            || lower.contains("session closed")
            || lower.contains("session with given id"))
}

fn local_connect_error_reason(kind: &str, raw_error: &str) -> String {
    match kind {
        "permission-blocked" => {
            "A local Chrome DevTools endpoint is reachable, but Chrome rejected CDP control. Remote debugging permission is likely blocked for this browser instance.".to_string()
        }
        "cdp-disabled" => {
            "Chrome is open, but remote debugging is turned off for this browser instance."
                .to_string()
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
        "permission-blocked" | "cdp-disabled" => "browser local setup",
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
    #[serde(skip_serializing_if = "Option::is_none")]
    browser_path: Option<PathBuf>,
    profile_path: PathBuf,
    http_url: Option<String>,
    ws_url: String,
    source: String,
    connectable: bool,
    state: String,
    stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    browser_running: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_debugging_enabled: Option<bool>,
    reason: Option<String>,
    next_step: Option<String>,
}

#[derive(Debug, Clone)]
struct LocalCandidateRoot {
    browser_name: String,
    browser_path: Option<PathBuf>,
    user_data_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
struct LocalBrowserDebuggingStatus {
    browser_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    browser_path: Option<PathBuf>,
    user_data_dir: PathBuf,
    browser_running: bool,
    remote_debugging_enabled: Option<bool>,
}

fn local_candidates() -> Vec<LocalCandidate> {
    let mut roots = local_candidate_roots_from_installs(known_local_browser_installs());
    let mut seen_roots = roots
        .iter()
        .map(|root| (root.browser_name.clone(), root.user_data_dir.clone()))
        .collect::<HashSet<_>>();
    for (browser_name, user_data_dir) in known_profile_roots() {
        if seen_roots.insert((browser_name.to_string(), user_data_dir.clone())) {
            roots.push(LocalCandidateRoot {
                browser_name: browser_name.to_string(),
                browser_path: None,
                user_data_dir,
            });
        }
    }
    local_candidates_from_candidate_roots(roots, &[9222_u16, 9223])
}

fn local_candidate_roots_from_installs(
    installs: Vec<LocalBrowserInstall>,
) -> Vec<LocalCandidateRoot> {
    installs
        .into_iter()
        .map(|install| LocalCandidateRoot {
            browser_name: install.browser_name,
            browser_path: Some(install.browser_path),
            user_data_dir: install.user_data_dir,
        })
        .collect()
}

#[cfg(test)]
fn local_candidates_from_roots(
    roots: Vec<(&'static str, PathBuf)>,
    probe_ports: &[u16],
) -> Vec<LocalCandidate> {
    let roots = roots
        .into_iter()
        .map(|(browser_name, user_data_dir)| LocalCandidateRoot {
            browser_name: browser_name.to_string(),
            browser_path: None,
            user_data_dir,
        })
        .collect();
    local_candidates_from_candidate_roots(roots, probe_ports)
}

fn local_candidates_from_candidate_roots(
    roots: Vec<LocalCandidateRoot>,
    probe_ports: &[u16],
) -> Vec<LocalCandidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        let active = root.user_data_dir.join("DevToolsActivePort");
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
        let browser_running = root
            .browser_path
            .as_deref()
            .and_then(|path| browser_process_running(&root.browser_name, path));
        let remote_debugging_enabled = remote_debugging_user_enabled(&root.user_data_dir);
        let (state, reason, next_step) = if connectable {
            ("reachable", None, "browser connect local --candidate <id>")
        } else {
            let (state, reason, next_step) =
                local_disconnected_candidate_details(browser_running, remote_debugging_enabled);
            (state, Some(reason), next_step)
        };
        candidates.push(LocalCandidate {
            id,
            browser_name: root.browser_name,
            browser_path: root.browser_path,
            profile_path: root.user_data_dir,
            http_url,
            ws_url,
            source: active.display().to_string(),
            connectable,
            state: state.to_string(),
            stale: !connectable,
            browser_running,
            remote_debugging_enabled,
            reason,
            next_step: Some(next_step.to_string()),
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
            browser_path: None,
            profile_path: PathBuf::new(),
            http_url: Some(http_url),
            ws_url,
            source: "port-probe".to_string(),
            connectable: true,
            state: "reachable".to_string(),
            stale: false,
            browser_running: None,
            remote_debugging_enabled: None,
            reason: None,
            next_step: Some("browser connect local --candidate <id>".to_string()),
        });
    }
    candidates
}

fn local_debugging_disabled_statuses() -> Vec<LocalBrowserDebuggingStatus> {
    known_local_browser_installs()
        .into_iter()
        .filter_map(|install| {
            let browser_running =
                browser_process_running(&install.browser_name, &install.browser_path)?;
            let remote_debugging_enabled = remote_debugging_user_enabled(&install.user_data_dir);
            (browser_running && remote_debugging_enabled == Some(false)).then_some(
                LocalBrowserDebuggingStatus {
                    browser_name: install.browser_name,
                    browser_path: Some(install.browser_path),
                    user_data_dir: install.user_data_dir,
                    browser_running,
                    remote_debugging_enabled,
                },
            )
        })
        .collect()
}

fn local_disconnected_candidate_details(
    browser_running: Option<bool>,
    remote_debugging_enabled: Option<bool>,
) -> (&'static str, String, &'static str) {
    if browser_running == Some(true) && remote_debugging_enabled == Some(false) {
        return (
            "cdp-disabled",
            "Chrome is open, but remote debugging is turned off for this browser instance."
                .to_string(),
            "browser local setup",
        );
    }
    if browser_running == Some(true) {
        return (
            "stale-port",
            "DevToolsActivePort exists, but the recorded CDP port is not reachable. Chrome appears open, but it is not exposing that debug endpoint.".to_string(),
            "Open Chrome with this profile, then run browser connect local",
        );
    }
    (
        "stale-port",
        "DevToolsActivePort exists, but the recorded CDP port is not reachable. Chrome was likely closed or the debug server stopped.".to_string(),
        "Open Chrome with this profile, then run browser connect local",
    )
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

fn remote_debugging_user_enabled(user_data_dir: &Path) -> Option<bool> {
    let raw = fs::read_to_string(user_data_dir.join("Local State")).ok()?;
    let value = serde_json::from_str::<Value>(&raw).ok()?;
    remote_debugging_user_enabled_from_local_state(&value)
}

fn remote_debugging_user_enabled_from_local_state(value: &Value) -> Option<bool> {
    value
        .pointer("/devtools/remote_debugging/user-enabled")
        .and_then(Value::as_bool)
}

#[cfg(unix)]
fn browser_process_running(_browser_name: &str, browser_path: &Path) -> Option<bool> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,comm=,args="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let browser_path = browser_path.to_string_lossy();
    Some(
        !browser_path.is_empty()
            && text
                .lines()
                .any(|line| line.contains(browser_path.as_ref())),
    )
}

#[cfg(windows)]
fn browser_process_running(_browser_name: &str, browser_path: &Path) -> Option<bool> {
    let output = Command::new("tasklist")
        .args(["/FO", "CSV"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    let executable = browser_path
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();
    Some(!executable.is_empty() && text.contains(&executable))
}

#[cfg(not(any(unix, windows)))]
fn browser_process_running(_browser_name: &str, _browser_path: &Path) -> Option<bool> {
    None
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
    #[cfg(test)]
    {
        let kind = request.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind == "test_large_response" {
            let bytes = request
                .get("bytes")
                .and_then(Value::as_u64)
                .unwrap_or(1_000_000) as usize;
            return Ok(json!({ "blob": "x".repeat(bytes) }));
        }
    }

    let mut session = {
        let mut sessions = sessions()
            .lock()
            .expect("browser session registry poisoned");
        sessions.remove(session_id).ok_or_else(|| {
            anyhow!("browser is not connected or is busy; run `browser status --json`")
        })?
    };
    session.session_id = Some(session_id.to_string());
    let result = bridge_request_with_session(&mut session, request);
    sessions()
        .lock()
        .expect("browser session registry poisoned")
        .insert(session_id.to_string(), session);
    result
}

fn bridge_request_with_session(session: &mut BrowserSession, request: &Value) -> Result<Value> {
    let kind = request.get("kind").and_then(Value::as_str).unwrap_or("");
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
    agent_workspace_dir: &Path,
    domain_skill_roots: &[PathBuf],
    stream_path: &Path,
    user_code: &str,
) -> Result<String> {
    let encoded_code = general_purpose::STANDARD.encode(user_code.as_bytes());
    let encoded_helpers = general_purpose::STANDARD.encode(BROWSER_SCRIPT_HELPERS.as_bytes());
    let domain_skill_roots_json = serde_json::to_string(
        &domain_skill_roots
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
    )?;
    Ok(format!(
        r#"
import base64, contextlib, io, json, os, pathlib, shutil, socket, sys, time, traceback, urllib.request

BRIDGE_PORT = {bridge_port}
CWD = pathlib.Path({cwd:?}).expanduser().resolve()
ARTIFACT_DIR = pathlib.Path({artifact_dir:?}).expanduser().resolve()
STREAM_PATH = pathlib.Path({stream_path:?}).expanduser().resolve()
AGENT_WORKSPACE_DIR = pathlib.Path({agent_workspace_dir:?}).expanduser().resolve()
DOMAIN_SKILL_ROOTS = json.loads({domain_skill_roots_json:?})
ARTIFACT_DIR.mkdir(parents=True, exist_ok=True)
STREAM_PATH.parent.mkdir(parents=True, exist_ok=True)
OUTPUTS_DIR = CWD
OUTPUTS_DIR.mkdir(parents=True, exist_ok=True)

def _stream_event(event):
    try:
        with STREAM_PATH.open("a", encoding="utf-8") as f:
            f.write(json.dumps(event, default=_jsonable) + "\n")
            f.flush()
    except Exception:
        pass

class _BrowserScriptStream:
    def __init__(self, kind):
        self.kind = kind
        self._parts = []
    def write(self, value):
        value = str(value)
        self._parts.append(value)
        if value:
            _stream_event({{"type": self.kind, "text": value}})
        return len(value)
    def flush(self):
        pass
    def isatty(self):
        return False
    def getvalue(self):
        return "".join(self._parts)

def _collectable_files(root):
    if root.resolve() == OUTPUTS_DIR.resolve():
        for path in root.iterdir():
            if path.is_file():
                yield path
        return
    for path in root.rglob("*"):
        if path.is_file():
            yield path

def _scan_artifact_files():
    files = set()
    for root in (ARTIFACT_DIR, OUTPUTS_DIR):
        if not root.exists():
            continue
        for path in _collectable_files(root):
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
    _stream_event({{"type": "artifact", "artifact": meta}})
    return meta

def _auto_collect_artifacts():
    image_paths = {{str(pathlib.Path(image.get("path", "")).expanduser().resolve()) for image in __images if image.get("path")}}
    stream_path = str(STREAM_PATH.resolve())
    for root in (ARTIFACT_DIR, OUTPUTS_DIR):
        if not root.exists():
            continue
        for path in sorted(_collectable_files(root)):
            resolved = str(path.resolve())
            if resolved in __initial_artifact_files:
                continue
            if resolved == stream_path:
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
    _stream_event({{"type": "image", "image": meta}})
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

def session_metadata():
    return {{
        "cwd": str(CWD),
        "artifact_root": str(ARTIFACT_DIR),
        "outputs_dir": str(OUTPUTS_DIR),
        "agent_workspace": str(pathlib.Path(agent_workspace())),
    }}

def agent_workspace():
    configured = os.environ.get("BH_AGENT_WORKSPACE")
    if configured:
        path = pathlib.Path(configured).expanduser()
    else:
        path = AGENT_WORKSPACE_DIR
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

stdout = _BrowserScriptStream("stdout")
stderr = _BrowserScriptStream("stderr")
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
    "data": {{"domain_skills": globals().get("__last_domain_skills", [])}} if globals().get("__last_domain_skills") else {{}},
    "outputs": __outputs,
    "artifacts": __artifacts,
    "images": __images,
    "browser_events": [],
}}
sys.__stdout__.write("__BROWSER_SCRIPT_RESULT__" + json.dumps(result, default=_jsonable) + "\n")
sys.__stdout__.flush()
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
    use std::sync::MutexGuard;

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct EnvRestore {
        _guard: MutexGuard<'static, ()>,
        values: Vec<(&'static str, Option<String>)>,
    }

    impl EnvRestore {
        fn set(vars: &[(&'static str, &str)]) -> Self {
            let guard = ENV_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .expect("env lock poisoned");
            let values = vars
                .iter()
                .map(|(key, _)| (*key, std::env::var(key).ok()))
                .collect::<Vec<_>>();
            for (key, value) in vars {
                std::env::set_var(key, value);
            }
            Self {
                _guard: guard,
                values,
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, value) in self.values.drain(..) {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }

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
    fn local_setup_waits_for_user_confirmation_before_retry() {
        let status = local_setup_user_action_response(false, None, None);
        assert_eq!(status["status"], "needs-user-action");
        assert!(status["next_step"]
            .as_str()
            .unwrap()
            .contains("Wait for user confirmation"));
        assert!(status["instructions"][2]
            .as_str()
            .unwrap()
            .contains("Do not retry until the user confirms"));
    }

    #[test]
    fn stale_session_errors_are_classified_for_reattach() {
        let message = r#"CDP Runtime.evaluate failed: {"code":-32001,"message":"Session with given id not found."}"#;
        assert!(is_stale_session_error(message));
        assert_eq!(classify_browser_error(message), "session-gone");
    }

    #[test]
    fn cdp_read_timeouts_are_not_classified_as_dropped_websockets() {
        let message =
            "read CDP Runtime.evaluate: IO error: Resource temporarily unavailable (os error 35)";
        assert_eq!(classify_browser_error(message), "cdp-read-timeout");
        assert!(!should_drop_browser_connection(classify_browser_error(
            message
        )));
    }

    #[test]
    fn cdp_read_timeout_diagnosis_keeps_page_reusable() {
        let diagnosis = browser_issue_diagnosis("cdp-read-timeout", true, true, None);
        assert!(diagnosis.browser_usable);
        assert!(diagnosis.page_usable);
        assert!(diagnosis.summary.contains("same page"));
        assert!(diagnosis.next_step.contains("smaller browser_script chunk"));
    }

    #[test]
    fn browser_script_tracebacks_are_not_treated_as_dropped_websockets() {
        let message = "Traceback (most recent call last):\nNameError: name 'x' is not defined";
        assert_eq!(
            classify_browser_script_failure(message),
            "browser-script-error"
        );
        let diagnosis =
            browser_issue_diagnosis(classify_browser_script_failure(message), true, true, None);
        assert!(diagnosis.browser_usable);
        assert!(diagnosis.page_usable);
        assert!(diagnosis.next_step.contains("Fix the Python"));
    }

    #[test]
    fn runtime_evaluate_script_errors_are_not_websocket_drops() {
        let message = r#"RuntimeError: CDP Runtime.evaluate failed: {"code":-32000,"message":"Exception thrown"}"#;
        assert_eq!(
            classify_browser_script_failure(message),
            "browser-script-error"
        );
    }

    #[test]
    fn terminal_websocket_errors_still_drop_browser_connection() {
        assert!(should_drop_browser_connection(classify_browser_error(
            "read CDP Target.getTargets: IO error: Connection reset by peer"
        )));
        assert!(should_drop_browser_connection(classify_browser_error(
            "CDP websocket closed: None"
        )));
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
        assert!(help.contains("browser domain skills --domain"));
        assert!(help.contains("browser_script"));
        assert!(help
            .to_ascii_lowercase()
            .contains("remote start means start and connect"));
    }

    #[test]
    fn browser_domain_skills_command_lists_matching_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("domain-skills");
        fs::create_dir_all(root.join("parity")).unwrap();
        fs::write(
            root.join("parity/scraping.md"),
            "# Parity\n\nUse the stable API before DOM scraping.",
        )
        .unwrap();
        let root_text = root.display().to_string();
        let _env = EnvRestore::set(&[
            ("BH_DOMAIN_SKILLS_ROOT", &root_text),
            ("BH_DOMAIN_SKILLS", "1"),
        ]);

        let output = run_browser_command(
            "domain-skills",
            temp.path(),
            temp.path(),
            "browser domain skills --domain https://www.parity.test/path --include-content --json",
        )
        .unwrap();

        assert_eq!(output.content["status"], "ok");
        assert_eq!(output.content["matches"][0]["site"], "parity");
        assert_eq!(
            output.content["matches"][0]["files"][0]["name"],
            "scraping.md"
        );
        assert!(output.content["matches"][0]["files"][0]["content"]
            .as_str()
            .unwrap()
            .contains("stable API"));
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
answer = pathlib.Path(outputs_dir()) / 'answer.json'
answer.write_text(json.dumps({'ok': True}), encoding='utf-8')
print(session_metadata()["outputs_dir"])
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
        assert!(output.text.contains(temp.path().to_str().unwrap()));
        for artifact in &output.artifacts {
            assert!(
                Path::new(artifact["path"].as_str().unwrap()).is_absolute(),
                "artifact path should be absolute: {artifact}"
            );
        }
    }

    #[test]
    fn browser_script_domain_skills_are_available_without_browser() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("domain-skills");
        fs::create_dir_all(root.join("parity")).unwrap();
        fs::write(
            root.join("parity/scraping.md"),
            "# Parity\n\nRead this before inventing selectors.",
        )
        .unwrap();
        let root_text = root.display().to_string();
        let _env = EnvRestore::set(&[
            ("BH_DOMAIN_SKILLS_ROOT", &root_text),
            ("BH_DOMAIN_SKILLS", "1"),
        ]);
        let output = run_browser_script(
            "script-domain-skills",
            temp.path(),
            temp.path().join("artifacts"),
            r#"
skills = domain_skills_for_url("https://www.parity.test/search", include_content=True)
assert skills[0]["site"] == "parity", skills
assert "before inventing selectors" in skills[0]["files"][0]["content"], skills
print(json.dumps(skills))
"#,
            10,
        )
        .unwrap();

        assert!(output.ok, "{:?}\n{}", output.error, output.text);
        assert!(output.text.contains("before inventing selectors"));
    }

    #[test]
    fn browser_script_helpers_survive_user_time_imports() {
        let temp = tempfile::tempdir().unwrap();
        let output = run_browser_script(
            "script-time-shadow",
            temp.path(),
            temp.path().join("artifacts"),
            r#"
from time import time
path = _write_b64_artifact("time_shadow", base64.b64encode(b"ok").decode(), suffix=".bin", mime_type="application/octet-stream")
assert pathlib.Path(path).read_bytes() == b"ok"
print("time shadow ok")
"#,
            10,
        )
        .unwrap();

        assert!(output.ok, "{:?}\n{}", output.error, output.text);
        assert!(output.text.contains("time shadow ok"));
    }

    #[test]
    fn browser_script_js_return_detection_ignores_nested_callbacks() {
        let temp = tempfile::tempdir().unwrap();
        let output = run_browser_script(
            "script-js-return-detection",
            temp.path(),
            temp.path().join("artifacts"),
            r#"
assert _has_return_statement("const x = 1; return x")
assert _has_return_statement("if (true) { return 1; }")
assert not _has_return_statement("Array.from(items).map((el) => { return el.id; })")
assert _has_return_statement("const ids = items.map((el) => { return el.id; }); return ids")
print("return detection ok")
"#,
            10,
        )
        .unwrap();

        assert!(output.ok, "{:?}\n{}", output.error, output.text);
        assert!(output.text.contains("return detection ok"));
    }

    #[test]
    fn browser_script_uses_project_python_environment_when_available() {
        let Some(repo_root) = repo_root_from_manifest() else {
            eprintln!("skipping project python environment test: missing repo root");
            return;
        };
        if !repo_root.join(".venv").is_dir() && !command_exists("uv") {
            eprintln!("skipping project python environment test: no .venv or uv");
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let output = run_browser_script(
            "script-project-python-env",
            temp.path(),
            temp.path().join("artifacts"),
            r#"
import bs4
print("bs4 available", bs4.__version__)
"#,
            10,
        )
        .unwrap();

        assert!(output.ok, "{:?}\n{}", output.error, output.text);
        assert!(output.text.contains("bs4 available"));
    }

    #[test]
    fn browser_script_fill_input_builds_valid_helper_js() {
        let temp = tempfile::tempdir().unwrap();
        let output = run_browser_script(
            "script-fill-input-js",
            temp.path(),
            temp.path().join("artifacts"),
            r#"
seen = []

def js(expression, *args, **kwargs):
    seen.append(expression)
    return True

def cdp(*args, **kwargs):
    return {}

def press_key(key):
    seen.append(("key", key))
    return True

fill_input("input", "ab")
assert any("return true;})()" in item for item in seen if isinstance(item, str)), seen
assert not any("return true;}})()" in item for item in seen if isinstance(item, str)), seen
assert any("change" in item and "})();" in item for item in seen if isinstance(item, str)), seen
print("fill_input js ok")
"#,
            10,
        )
        .unwrap();

        assert!(output.ok, "{:?}\n{}", output.error, output.text);
        assert!(output.text.contains("fill_input js ok"));
    }

    #[test]
    fn browser_script_press_key_accepts_common_chord_strings() {
        let temp = tempfile::tempdir().unwrap();
        let output = run_browser_script(
            "script-press-key-chords",
            temp.path(),
            temp.path().join("artifacts"),
            r#"
seen = []

def cdp(method, **params):
    seen.append((method, params))
    return {}

press_key("Meta+A")
events = [params for method, params in seen if method == "Input.dispatchKeyEvent"]
assert events[0]["type"] == "rawKeyDown", events
assert events[0]["key"] == "A", events
assert events[0]["modifiers"] == 4, events
assert "text" not in events[0], events
assert not any(event.get("type") == "char" for event in events), events

seen.clear()
press_key("Ctrl+Shift+Tab")
events = [params for method, params in seen if method == "Input.dispatchKeyEvent"]
assert events[0]["key"] == "Tab", events
assert events[0]["modifiers"] == 10, events
print("press_key chords ok")
"#,
            10,
        )
        .unwrap();

        assert!(output.ok, "{:?}\n{}", output.error, output.text);
        assert!(output.text.contains("press_key chords ok"));
    }

    #[test]
    fn browser_script_http_get_matches_proxy_gzip_and_binary_contracts() {
        let temp = tempfile::tempdir().unwrap();
        let output = run_browser_script(
            "script-http-get",
            temp.path(),
            temp.path().join("artifacts"),
            r#"
import gzip
import http.server
import socketserver
import threading
import types

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def do_GET(self):
        if self.path == "/gzip":
            assert self.headers.get("X-Parity") == "yes", dict(self.headers)
            body = gzip.compress(b"hello gzip")
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Encoding", "gzip")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path == "/binary":
            body = bytes([0, 159, 255])
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(404)
        self.end_headers()

server = socketserver.TCPServer(("127.0.0.1", 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
base = f"http://127.0.0.1:{server.server_address[1]}"
try:
    text = http_get(base + "/gzip", headers={"X-Parity": "yes"})
    assert text == "hello gzip"
    assert text.status_code == 200
    assert text.text == "hello gzip"
    assert text.content == b"hello gzip"
    blob = http_get(base + "/binary")
    assert blob == bytes([0, 159, 255])
    assert blob.status_code == 200
    assert blob.content == bytes([0, 159, 255])
finally:
    server.shutdown()
    server.server_close()

class FakeFetchModule:
    @staticmethod
    def fetch_sync(url, headers=None, timeout_ms=None):
        assert headers == {"X": "1"}
        assert timeout_ms == 1234
        if url.endswith("/binary"):
            return types.SimpleNamespace(
                text="",
                content=bytes([0, 159, 255]),
                status_code=202,
                headers={"x-proxy": "yes"},
                url=url,
            )
        return types.SimpleNamespace(text="proxied", status_code=202, headers={"x-proxy": "yes"}, url=url)

sys.modules["fetch_use"] = FakeFetchModule
os.environ["BROWSER_USE_API_KEY"] = "test"
proxied = http_get("https://example.test/data", headers={"X": "1"}, timeout=1.234)
assert proxied == "proxied"
assert proxied.status_code == 202
assert proxied.headers["x-proxy"] == "yes"
proxied_binary = http_get("https://example.test/binary", headers={"X": "1"}, timeout=1.234, binary=True)
assert proxied_binary == bytes([0, 159, 255])
assert proxied_binary.status_code == 202
assert proxied_binary.content == bytes([0, 159, 255])
print("http_get parity ok")
"#,
            10,
        )
        .unwrap();

        assert!(output.ok, "{:?}\n{}", output.error, output.text);
        assert!(output.text.contains("http_get parity ok"));
    }

    #[test]
    fn browser_script_timeout_returns_tool_failure() {
        let temp = tempfile::tempdir().unwrap();
        let output = run_browser_script(
            "script-timeout",
            temp.path(),
            temp.path().join("artifacts"),
            "import time\ntime.sleep(5)",
            1,
        )
        .unwrap();

        assert!(!output.ok);
        assert!(output
            .error
            .as_deref()
            .is_some_and(|error| error.contains("browser_script timed out after 1 seconds")));
    }

    #[test]
    fn browser_script_start_returns_immediate_result_for_fast_scripts() {
        let temp = tempfile::tempdir().unwrap();
        let output = start_browser_script(
            "script-fast-start",
            temp.path(),
            temp.path().join("artifacts"),
            "print('fast result')",
            5,
        )
        .unwrap();

        assert!(output.ok);
        assert_eq!(output.status.as_deref(), Some("finished"));
        assert!(output.text.contains("fast result"));
        assert!(output.run_id.is_some());
    }

    #[test]
    fn browser_script_start_observe_finishes_slow_scripts() {
        let temp = tempfile::tempdir().unwrap();
        let session_id = "script-start-observe";
        let started = start_browser_script(
            session_id,
            temp.path(),
            temp.path().join("artifacts"),
            "import time\nprint('chunk one')\ntime.sleep(1.2)\nprint('chunk two')",
            5,
        )
        .unwrap();

        assert!(started.ok);
        assert_eq!(started.status.as_deref(), Some("running"));
        assert!(started.text.contains("chunk one"));
        let run_id = started.run_id.as_deref().unwrap();

        let mut finished = observe_browser_script(session_id, run_id, 2_500).unwrap();
        if finished.status.as_deref() == Some("running") {
            finished = observe_browser_script(session_id, run_id, 2_500).unwrap();
        }
        assert!(finished.ok);
        assert_eq!(finished.status.as_deref(), Some("finished"));
        assert!(finished.text.contains("chunk two"));
        assert!(
            finished.artifacts.is_empty(),
            "internal stream file leaked as artifact: {:?}",
            finished.artifacts
        );
    }

    #[test]
    fn browser_script_observe_can_return_no_new_output() {
        let temp = tempfile::tempdir().unwrap();
        let session_id = "script-observe-empty";
        let started = start_browser_script(
            session_id,
            temp.path(),
            temp.path().join("artifacts"),
            "import time\ntime.sleep(1.2)\nprint('late')",
            5,
        )
        .unwrap();

        assert_eq!(started.status.as_deref(), Some("running"));
        let run_id = started.run_id.as_deref().unwrap();
        let observed = observe_browser_script(session_id, run_id, 50).unwrap();
        assert!(observed.ok);
        assert_eq!(observed.status.as_deref(), Some("running"));
        assert!(observed.text.contains("No new output"));

        let _ = cancel_browser_script(session_id, run_id);
    }

    #[test]
    fn browser_script_observe_returns_images_before_final_result() {
        let temp = tempfile::tempdir().unwrap();
        let session_id = "script-observe-image";
        let code = r#"
import pathlib, time
path = pathlib.Path(outputs_dir()) / "before_failure.png"
path.write_bytes(b"\x89PNG")
emit_image(path, label="before failure")
time.sleep(1.2)
print("finished")
"#;
        let started = start_browser_script(
            session_id,
            temp.path(),
            temp.path().join("artifacts"),
            code,
            5,
        )
        .unwrap();

        assert_eq!(started.status.as_deref(), Some("running"));
        assert_eq!(started.images.len(), 1);
        assert_eq!(
            started.images[0].get("label").and_then(Value::as_str),
            Some("before failure")
        );
        let run_id = started.run_id.as_deref().unwrap();
        let _ = cancel_browser_script(session_id, run_id);
    }

    #[test]
    fn browser_status_lists_active_script_runs() {
        let temp = tempfile::tempdir().unwrap();
        let session_id = "script-status-active-runs";
        let started = start_browser_script(
            session_id,
            temp.path(),
            temp.path().join("artifacts"),
            "import time\ntime.sleep(1.2)",
            5,
        )
        .unwrap();
        assert_eq!(started.status.as_deref(), Some("running"));

        let status = run_browser_command(session_id, temp.path(), temp.path(), "status --json")
            .unwrap()
            .content;
        assert_eq!(
            status["active_scripts"][0]["run_id"],
            started.run_id.as_deref().unwrap()
        );

        let _ = cancel_browser_script(session_id, started.run_id.as_deref().unwrap());
    }

    #[test]
    fn browser_script_cancel_command_stops_active_run() {
        let temp = tempfile::tempdir().unwrap();
        let session_id = "script-cancel-command";
        let started = start_browser_script(
            session_id,
            temp.path(),
            temp.path().join("artifacts"),
            "import time\ntime.sleep(5)",
            10,
        )
        .unwrap();
        let run_id = started.run_id.as_deref().unwrap();

        let cancelled = run_browser_command(
            session_id,
            temp.path(),
            temp.path(),
            &format!("script cancel {run_id}"),
        )
        .unwrap()
        .content;

        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["run_id"], run_id);
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
    fn remote_debugging_flag_reads_chrome_local_state() {
        let value = json!({
            "devtools": {
                "remote_debugging": {
                    "user-enabled": false
                }
            }
        });
        assert_eq!(
            remote_debugging_user_enabled_from_local_state(&value),
            Some(false)
        );
    }

    #[test]
    fn running_browser_with_disabled_cdp_gets_specific_local_state() {
        let (state, reason, next_step) =
            local_disconnected_candidate_details(Some(true), Some(false));
        assert_eq!(state, "cdp-disabled");
        assert!(reason.contains("remote debugging is turned off"));
        assert_eq!(next_step, "browser local setup");
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
