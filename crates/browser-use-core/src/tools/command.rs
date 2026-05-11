use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child as StdChild, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use browser_use_protocol::{SessionMeta, ToolCall};
use browser_use_store::Store;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};

const DEFAULT_YIELD_TIME_MS: u64 = 1000;
const MAX_YIELD_TIME_MS: u64 = 30_000;
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
const TOKEN_TO_CHAR_APPROX: usize = 4;

#[derive(Debug)]
pub(crate) struct CommandToolResult {
    pub(crate) content: Value,
}

#[derive(Clone, Debug)]
struct OutputChunk {
    text: String,
}

struct ManagedCommand {
    session_id: String,
    process: ManagedProcess,
    output: Arc<Mutex<Vec<OutputChunk>>>,
    read_index: usize,
    started_at: Instant,
    readers: Vec<JoinHandle<()>>,
}

enum ManagedProcess {
    Pipes {
        child: StdChild,
        stdin: Option<ChildStdin>,
    },
    Pty {
        child: Box<dyn portable_pty::Child + Send + Sync>,
        writer: Box<dyn Write + Send>,
        _master: Box<dyn MasterPty + Send>,
    },
}

#[derive(Clone, Debug)]
struct ProcessExit {
    exit_code: Option<i32>,
    success: bool,
}

static COMMANDS: OnceLock<Mutex<HashMap<String, ManagedCommand>>> = OnceLock::new();

pub(crate) fn exec_command(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
) -> Result<CommandToolResult> {
    let cmd = call
        .arguments
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if cmd.is_empty() {
        bail!("exec_command requires cmd");
    }
    let yield_time = yield_time(&call.arguments);
    let max_chars = max_output_chars(&call.arguments);
    let workdir = resolve_workdir(
        session,
        call.arguments.get("workdir").and_then(Value::as_str),
    )?;
    let shell = call
        .arguments
        .get("shell")
        .and_then(Value::as_str)
        .filter(|shell| !shell.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(default_shell);
    let login = call
        .arguments
        .get("login")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let tty_requested = call
        .arguments
        .get("tty")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    store.append_event(
        &session.id,
        "tool.started",
        json!({
            "name": "exec_command",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    store.append_event(
        &session.id,
        "command.started",
        json!({
            "tool_call_id": call.id,
            "cmd": cmd,
            "workdir": workdir,
            "shell": shell,
            "login": login,
            "tty": tty_requested,
        }),
    )?;

    let process_id = format!("cmd_{}", &call.id.replace('-', "_"));
    let output = Arc::new(Mutex::new(Vec::new()));
    let (process, readers, tty_allocated) =
        spawn_process(&shell, login, cmd, &workdir, tty_requested, output.clone())?;
    let mut managed = ManagedCommand {
        session_id: session.id.clone(),
        process,
        output,
        read_index: 0,
        started_at: Instant::now(),
        readers,
    };

    wait_for_output(yield_time, || managed.process.try_wait())?;
    if let Some(status) = managed.process.try_wait()? {
        finish_readers(&mut managed);
        let text = managed.read_recent_output();
        emit_command_output(store, &session.id, &process_id, &text)?;
        store.append_event(
            &session.id,
            "command.finished",
            json!({
                "tool_call_id": call.id,
                "session_id": process_id,
                "exit_code": status.exit_code,
                "success": status.success,
                "duration_ms": managed.started_at.elapsed().as_millis() as u64,
            }),
        )?;
        let content = command_output(CommandOutputPayload {
            session_id: None,
            running: false,
            output: &text,
            max_chars,
            exit_code: status.exit_code,
            duration: managed.started_at.elapsed(),
            tty_requested,
            tty_allocated,
        });
        store.append_event(
            &session.id,
            "tool.finished",
            json!({
                "name": "exec_command",
                "tool_call_id": call.id,
                "output": content,
            }),
        )?;
        return Ok(CommandToolResult { content });
    }

    let text = managed.read_recent_output();
    emit_command_output(store, &session.id, &process_id, &text)?;
    store.append_event(
        &session.id,
        "command.waiting",
        json!({
            "tool_call_id": call.id,
            "session_id": process_id,
            "running": true,
        }),
    )?;
    let content = command_output(CommandOutputPayload {
        session_id: Some(process_id.clone()),
        running: true,
        output: &text,
        max_chars,
        exit_code: None,
        duration: managed.started_at.elapsed(),
        tty_requested,
        tty_allocated,
    });
    commands()
        .lock()
        .expect("command registry poisoned")
        .insert(process_id, managed);
    store.append_event(
        &session.id,
        "tool.finished",
        json!({
            "name": "exec_command",
            "tool_call_id": call.id,
            "output": content,
        }),
    )?;
    Ok(CommandToolResult { content })
}

pub(crate) fn write_stdin(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
) -> Result<CommandToolResult> {
    let process_id = call
        .arguments
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if process_id.is_empty() {
        bail!("write_stdin requires session_id");
    }
    let chars = call
        .arguments
        .get("chars")
        .and_then(Value::as_str)
        .unwrap_or("");
    let yield_time = yield_time(&call.arguments);
    let max_chars = max_output_chars(&call.arguments);
    store.append_event(
        &session.id,
        "tool.started",
        json!({
            "name": "write_stdin",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;

    let mut command = {
        let mut commands = commands().lock().expect("command registry poisoned");
        commands
            .remove(process_id)
            .with_context(|| format!("unknown command session id: {process_id}"))?
    };
    if command.session_id != session.id {
        commands()
            .lock()
            .expect("command registry poisoned")
            .insert(process_id.to_string(), command);
        bail!("command session belongs to another task: {process_id}");
    }
    if !chars.is_empty() {
        command.process.write_all(chars.as_bytes())?;
    }
    wait_for_output(yield_time, || command.process.try_wait())?;
    let status = command.process.try_wait()?;
    if status.is_some() {
        finish_readers(&mut command);
    }
    let text = command.read_recent_output();
    emit_command_output(store, &session.id, process_id, &text)?;

    let running = status.is_none();
    let tty_allocated = command.process.tty_allocated();
    let content = command_output(CommandOutputPayload {
        session_id: Some(process_id.to_string()),
        running,
        output: &text,
        max_chars,
        exit_code: status.as_ref().and_then(|status| status.exit_code),
        duration: command.started_at.elapsed(),
        tty_requested: tty_allocated,
        tty_allocated,
    });
    if let Some(status) = status {
        store.append_event(
            &session.id,
            "command.finished",
            json!({
                "tool_call_id": call.id,
                "session_id": process_id,
                "exit_code": status.exit_code,
                "success": status.success,
                "duration_ms": command.started_at.elapsed().as_millis() as u64,
            }),
        )?;
    } else {
        commands()
            .lock()
            .expect("command registry poisoned")
            .insert(process_id.to_string(), command);
    }
    store.append_event(
        &session.id,
        "tool.finished",
        json!({
            "name": "write_stdin",
            "tool_call_id": call.id,
            "output": content,
        }),
    )?;
    Ok(CommandToolResult { content })
}

fn commands() -> &'static Mutex<HashMap<String, ManagedCommand>> {
    COMMANDS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn spawn_process(
    shell: &str,
    login: bool,
    cmd: &str,
    workdir: &Path,
    tty_requested: bool,
    output: Arc<Mutex<Vec<OutputChunk>>>,
) -> Result<(ManagedProcess, Vec<JoinHandle<()>>, bool)> {
    if tty_requested {
        return spawn_pty_process(shell, login, cmd, workdir, output);
    }
    spawn_pipe_process(shell, login, cmd, workdir, output)
}

fn spawn_pipe_process(
    shell: &str,
    login: bool,
    cmd: &str,
    workdir: &Path,
    output: Arc<Mutex<Vec<OutputChunk>>>,
) -> Result<(ManagedProcess, Vec<JoinHandle<()>>, bool)> {
    let mut command = Command::new(shell);
    command
        .args(shell_args(shell, login, cmd))
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn command via shell {} in {}", shell, workdir.display()))?;
    let stdin = child.stdin.take();
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_reader(stdout, output.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_reader(stderr, output));
    }
    Ok((ManagedProcess::Pipes { child, stdin }, readers, false))
}

fn spawn_pty_process(
    shell: &str,
    login: bool,
    cmd: &str,
    workdir: &Path,
    output: Arc<Mutex<Vec<OutputChunk>>>,
) -> Result<(ManagedProcess, Vec<JoinHandle<()>>, bool)> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 30,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let mut command = CommandBuilder::new(shell);
    command.args(shell_args(shell, login, cmd));
    command.cwd(workdir.as_os_str());
    let child = pair.slave.spawn_command(command).with_context(|| {
        format!(
            "spawn pty command via shell {} in {}",
            shell,
            workdir.display()
        )
    })?;
    let readers = vec![spawn_reader(reader, output)];
    Ok((
        ManagedProcess::Pty {
            child,
            writer,
            _master: pair.master,
        },
        readers,
        true,
    ))
}

impl ManagedProcess {
    fn try_wait(&mut self) -> Result<Option<ProcessExit>> {
        match self {
            Self::Pipes { child, .. } => Ok(child.try_wait()?.map(|status| ProcessExit {
                exit_code: status.code(),
                success: status.success(),
            })),
            Self::Pty { child, .. } => Ok(child.try_wait()?.map(|status| ProcessExit {
                exit_code: i32::try_from(status.exit_code()).ok(),
                success: status.success(),
            })),
        }
    }

    #[cfg(test)]
    fn wait(&mut self) -> Result<ProcessExit> {
        match self {
            Self::Pipes { child, .. } => {
                let status = child.wait()?;
                Ok(ProcessExit {
                    exit_code: status.code(),
                    success: status.success(),
                })
            }
            Self::Pty { child, .. } => {
                let status = child.wait()?;
                Ok(ProcessExit {
                    exit_code: i32::try_from(status.exit_code()).ok(),
                    success: status.success(),
                })
            }
        }
    }

    #[cfg(test)]
    fn kill(&mut self) -> Result<()> {
        match self {
            Self::Pipes { child, .. } => child.kill().map_err(Into::into),
            Self::Pty { child, .. } => child.kill().map_err(Into::into),
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Pipes { stdin, .. } => {
                let stdin = stdin.as_mut().context("command session has no stdin")?;
                stdin.write_all(bytes)?;
                stdin.flush()?;
            }
            Self::Pty { writer, .. } => {
                writer.write_all(bytes)?;
                writer.flush()?;
            }
        }
        Ok(())
    }

    fn tty_allocated(&self) -> bool {
        matches!(self, Self::Pty { .. })
    }
}

fn spawn_reader<R>(reader: R, output: Arc<Mutex<Vec<OutputChunk>>>) -> JoinHandle<()>
where
    R: std::io::Read + Send + 'static,
{
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            let mut bytes = Vec::new();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) => break,
                Ok(_) => output
                    .lock()
                    .expect("command output poisoned")
                    .push(OutputChunk {
                        text: String::from_utf8_lossy(&bytes).into_owned(),
                    }),
                Err(error) => {
                    output
                        .lock()
                        .expect("command output poisoned")
                        .push(OutputChunk {
                            text: format!("[command output read failed: {error}]\n"),
                        });
                    break;
                }
            }
        }
    })
}

fn finish_readers(command: &mut ManagedCommand) {
    for reader in command.readers.drain(..) {
        let _ = reader.join();
    }
}

impl ManagedCommand {
    fn read_recent_output(&mut self) -> String {
        let output = self.output.lock().expect("command output poisoned");
        let chunks = output
            .iter()
            .skip(self.read_index)
            .map(|chunk| chunk.text.clone())
            .collect::<String>();
        self.read_index = output.len();
        chunks
    }
}

fn wait_for_output(
    yield_time: Duration,
    mut exited: impl FnMut() -> Result<Option<ProcessExit>>,
) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < yield_time {
        if exited()?.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

fn emit_command_output(
    store: &Store,
    session_id: &str,
    process_id: &str,
    text: &str,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    store.append_event(
        session_id,
        "command.output",
        json!({
            "session_id": process_id,
            "stream": "combined",
            "text": text,
        }),
    )?;
    Ok(())
}

struct CommandOutputPayload<'a> {
    session_id: Option<String>,
    running: bool,
    output: &'a str,
    max_chars: usize,
    exit_code: Option<i32>,
    duration: Duration,
    tty_requested: bool,
    tty_allocated: bool,
}

fn command_output(payload: CommandOutputPayload<'_>) -> Value {
    let (output, truncated) = cap_output(payload.output, payload.max_chars);
    json!({
        "session_id": payload.session_id,
        "running": payload.running,
        "output": output,
        "metadata": {
            "exit_code": payload.exit_code,
            "duration_ms": payload.duration.as_millis() as u64,
            "truncated": truncated,
            "tty_requested": payload.tty_requested,
            "tty_allocated": payload.tty_allocated,
        }
    })
}

fn cap_output(output: &str, max_chars: usize) -> (String, bool) {
    let char_count = output.chars().count();
    if char_count <= max_chars {
        return (output.to_string(), false);
    }
    let head = max_chars / 2;
    let tail = max_chars.saturating_sub(head);
    let head_text = output.chars().take(head).collect::<String>();
    let tail_text = output
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    (
        format!(
            "{}\n\n[... omitted {} chars ...]\n\n{}",
            head_text,
            char_count.saturating_sub(max_chars),
            tail_text
        ),
        true,
    )
}

fn max_output_chars(arguments: &Value) -> usize {
    arguments
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .map(|tokens| tokens as usize * TOKEN_TO_CHAR_APPROX)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS * TOKEN_TO_CHAR_APPROX)
}

fn yield_time(arguments: &Value) -> Duration {
    let millis = arguments
        .get("yield_time_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_YIELD_TIME_MS)
        .min(MAX_YIELD_TIME_MS);
    Duration::from_millis(millis)
}

fn resolve_workdir(session: &SessionMeta, workdir: Option<&str>) -> Result<PathBuf> {
    let cwd = Path::new(&session.cwd);
    let Some(workdir) = workdir.filter(|value| !value.trim().is_empty()) else {
        return Ok(cwd.to_path_buf());
    };
    let path = Path::new(workdir);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    Ok(resolved)
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

fn shell_args(shell: &str, login: bool, cmd: &str) -> Vec<String> {
    let name = Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(shell);
    if login && matches!(name, "bash" | "zsh" | "sh") {
        vec!["-lc".to_string(), cmd.to_string()]
    } else {
        vec!["-c".to_string(), cmd.to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_use_protocol::SessionStatus;
    use tempfile::TempDir;

    fn test_session(tmp: &TempDir) -> (Store, SessionMeta) {
        let store = Store::open(tmp.path().join("state")).expect("store");
        let cwd = tmp.path().join("work");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = store.create_session(None, cwd).expect("session");
        (store, session)
    }

    #[test]
    fn exec_command_returns_completed_output() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let result = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_completed".to_string(),
                name: "exec_command".to_string(),
                arguments: json!({"cmd": "printf hello", "yield_time_ms": 5000}),
            },
        )
        .expect("exec");

        assert_eq!(result.content["running"], false);
        assert_eq!(result.content["output"], "hello");
        let events = store.events_for_session(&session.id).expect("events");
        assert!(events
            .iter()
            .any(|event| event.event_type == "command.finished"));
    }

    #[test]
    fn exec_command_can_allocate_pty() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let result = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_pty".to_string(),
                name: "exec_command".to_string(),
                arguments: json!({
                    "cmd": "printf pty-ok",
                    "tty": true,
                    "yield_time_ms": 5000,
                }),
            },
        )
        .expect("exec");

        assert_eq!(result.content["running"], false);
        assert!(result.content["output"]
            .as_str()
            .expect("output")
            .contains("pty-ok"));
        assert_eq!(result.content["metadata"]["tty_allocated"], true);
    }

    #[test]
    fn exec_command_can_be_polled_with_write_stdin() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let started = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_running".to_string(),
                name: "exec_command".to_string(),
                arguments: json!({
                    "cmd": "python3 -u -c \"import sys; print('ready', flush=True); [print('echo:' + line.strip(), flush=True) for line in sys.stdin]\"",
                    "yield_time_ms": 100,
                }),
            },
        )
        .expect("exec");
        let process_id = started.content["session_id"]
            .as_str()
            .expect("session id")
            .to_string();
        assert_eq!(started.content["running"], true);

        let written = write_stdin(
            &store,
            &session,
            &ToolCall {
                id: "call_write_stdin".to_string(),
                name: "write_stdin".to_string(),
                arguments: json!({
                    "session_id": process_id,
                    "chars": "hello\n",
                    "yield_time_ms": 200,
                }),
            },
        )
        .expect("write stdin");

        assert_eq!(written.content["running"], true);
        assert!(written.content["output"]
            .as_str()
            .expect("output")
            .contains("echo:hello"));
        stop_for_test(&process_id);
    }

    #[test]
    fn write_stdin_rejects_cross_session_process_access() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let other = store
            .create_session(None, tmp.path().join("other"))
            .expect("other session");
        let started = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_cross".to_string(),
                name: "exec_command".to_string(),
                arguments: json!({
                    "cmd": "python3 -u -c \"import time; print('ready', flush=True); time.sleep(5)\"",
                    "yield_time_ms": 100,
                }),
            },
        )
        .expect("exec");
        let process_id = started.content["session_id"].as_str().expect("session id");
        let error = write_stdin(
            &store,
            &other,
            &ToolCall {
                id: "call_cross_write".to_string(),
                name: "write_stdin".to_string(),
                arguments: json!({"session_id": process_id, "chars": ""}),
            },
        )
        .expect_err("cross session access should fail");
        assert!(error.to_string().contains("another task"));
        assert_eq!(session.status, SessionStatus::Created);
        stop_for_test(process_id);
    }

    fn stop_for_test(process_id: &str) {
        if let Some(mut command) = commands()
            .lock()
            .expect("command registry poisoned")
            .remove(process_id)
        {
            let _ = command.process.kill();
            let _ = command.process.wait();
            finish_readers(&mut command);
        }
    }
}
