use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child as StdChild, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use browser_use_protocol::{SessionMeta, ToolCall};
use browser_use_store::{Store, StoreNotifier};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use tree_sitter::{Node, Parser, Tree};
use tree_sitter_bash::LANGUAGE as BASH;

const DEFAULT_EXEC_YIELD_TIME_MS: u64 = 10_000;
const DEFAULT_SHELL_COMMAND_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_WRITE_STDIN_YIELD_TIME_MS: u64 = 250;
const MIN_YIELD_TIME_MS: u64 = 250;
const MIN_EMPTY_POLL_YIELD_TIME_MS: u64 = 5_000;
const MAX_YIELD_TIME_MS: u64 = 30_000;
const MAX_EMPTY_POLL_YIELD_TIME_MS: u64 = 300_000;
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
const UNIFIED_EXEC_OUTPUT_MAX_BYTES: usize = 1024 * 1024;
const MAX_UNIFIED_EXEC_PROCESSES: usize = 64;
const TOKEN_TO_CHAR_APPROX: usize = 4;
const WRITE_STDIN_REACTION_SETTLE_MS: u64 = 100;
const POST_EXIT_READER_DRAIN_TIMEOUT_MS: u64 = 50;
const BACKGROUND_TRAILING_OUTPUT_GRACE_MS: u64 = 100;
const SHELL_COMMAND_TIMEOUT_EXIT_CODE: i32 = 124;
const SHELL_COMMAND_IO_DRAIN_TIMEOUT_MS: u64 = 2_000;
const STDIN_CLOSED_MESSAGE: &str =
    "stdin is closed for this session; rerun exec_command with tty=true to keep stdin open";
const UNIFIED_EXEC_ENV: [(&str, &str); 10] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("LANG", "C.UTF-8"),
    ("LC_CTYPE", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    ("COLORTERM", ""),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GH_PAGER", "cat"),
    ("CODEX_CI", "1"),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellType {
    Zsh,
    Bash,
    PowerShell,
    Sh,
    Cmd,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ShellSpec {
    shell_type: ShellType,
    shell_path: PathBuf,
}

impl ShellSpec {
    fn name(&self) -> &'static str {
        match self.shell_type {
            ShellType::Zsh => "zsh",
            ShellType::Bash => "bash",
            ShellType::PowerShell => "powershell",
            ShellType::Sh => "sh",
            ShellType::Cmd => "cmd",
        }
    }

    fn path_string(&self) -> String {
        self.shell_path.to_string_lossy().to_string()
    }

    fn derive_exec_argv(&self, command: &str, use_login_shell: bool) -> Vec<String> {
        let shell_path = self.path_string();
        match self.shell_type {
            ShellType::Zsh | ShellType::Bash | ShellType::Sh => {
                let flag = if use_login_shell { "-lc" } else { "-c" };
                vec![shell_path, flag.to_string(), command.to_string()]
            }
            ShellType::PowerShell => {
                let mut args = vec![shell_path];
                if !use_login_shell {
                    args.push("-NoProfile".to_string());
                }
                args.push("-Command".to_string());
                args.push(command.to_string());
                args
            }
            ShellType::Cmd => vec![shell_path, "/c".to_string(), command.to_string()],
        }
    }

    fn exec_args(&self, command: &str, use_login_shell: bool) -> Vec<String> {
        self.derive_exec_argv(command, use_login_shell)
            .into_iter()
            .skip(1)
            .collect()
    }
}

#[derive(Debug)]
pub(crate) struct CommandToolResult {
    #[cfg(test)]
    pub(crate) content: Value,
    pub(crate) model_text: String,
}

struct ManagedCommand {
    session_id: String,
    tool_call_id: String,
    process: ManagedProcess,
    output: Arc<Mutex<HeadTailBuffer>>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    started_at: Instant,
    last_used: Instant,
    background_finished: bool,
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

#[derive(Debug)]
struct HeadTailBuffer {
    max_bytes: usize,
    head_budget: usize,
    tail_budget: usize,
    head: VecDeque<Vec<u8>>,
    tail: VecDeque<Vec<u8>>,
    head_bytes: usize,
    tail_bytes: usize,
    omitted_bytes: usize,
}

impl Default for HeadTailBuffer {
    fn default() -> Self {
        Self::new(UNIFIED_EXEC_OUTPUT_MAX_BYTES)
    }
}

impl HeadTailBuffer {
    fn new(max_bytes: usize) -> Self {
        let head_budget = max_bytes / 2;
        let tail_budget = max_bytes.saturating_sub(head_budget);
        Self {
            max_bytes,
            head_budget,
            tail_budget,
            head: VecDeque::new(),
            tail: VecDeque::new(),
            head_bytes: 0,
            tail_bytes: 0,
            omitted_bytes: 0,
        }
    }

    fn retained_bytes(&self) -> usize {
        self.head_bytes.saturating_add(self.tail_bytes)
    }

    #[cfg(test)]
    fn omitted_bytes(&self) -> usize {
        self.omitted_bytes
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.retained_bytes());
        for chunk in &self.head {
            out.extend_from_slice(chunk);
        }
        for chunk in &self.tail {
            out.extend_from_slice(chunk);
        }
        out
    }

    fn push_chunk(&mut self, chunk: Vec<u8>) {
        if self.max_bytes == 0 {
            self.omitted_bytes = self.omitted_bytes.saturating_add(chunk.len());
            return;
        }
        if self.head_bytes < self.head_budget {
            let remaining_head = self.head_budget.saturating_sub(self.head_bytes);
            if chunk.len() <= remaining_head {
                self.head_bytes = self.head_bytes.saturating_add(chunk.len());
                self.head.push_back(chunk);
                return;
            }
            let (head_part, tail_part) = chunk.split_at(remaining_head);
            if !head_part.is_empty() {
                self.head_bytes = self.head_bytes.saturating_add(head_part.len());
                self.head.push_back(head_part.to_vec());
            }
            self.push_to_tail(tail_part.to_vec());
            return;
        }
        self.push_to_tail(chunk);
    }

    fn drain_bytes(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.retained_bytes());
        for chunk in self.head.drain(..) {
            out.extend_from_slice(&chunk);
        }
        for chunk in self.tail.drain(..) {
            out.extend_from_slice(&chunk);
        }
        self.head_bytes = 0;
        self.tail_bytes = 0;
        self.omitted_bytes = 0;
        out
    }

    fn push_to_tail(&mut self, chunk: Vec<u8>) {
        if self.tail_budget == 0 {
            self.omitted_bytes = self.omitted_bytes.saturating_add(chunk.len());
            return;
        }
        if chunk.len() >= self.tail_budget {
            let start = chunk.len().saturating_sub(self.tail_budget);
            let kept = chunk[start..].to_vec();
            let dropped = chunk.len().saturating_sub(kept.len());
            self.omitted_bytes = self
                .omitted_bytes
                .saturating_add(self.tail_bytes)
                .saturating_add(dropped);
            self.tail.clear();
            self.tail_bytes = kept.len();
            self.tail.push_back(kept);
            return;
        }
        self.tail_bytes = self.tail_bytes.saturating_add(chunk.len());
        self.tail.push_back(chunk);
        self.trim_tail_to_budget();
    }

    fn trim_tail_to_budget(&mut self) {
        let mut excess = self.tail_bytes.saturating_sub(self.tail_budget);
        while excess > 0 {
            match self.tail.front_mut() {
                Some(front) if excess >= front.len() => {
                    excess -= front.len();
                    self.tail_bytes = self.tail_bytes.saturating_sub(front.len());
                    self.omitted_bytes = self.omitted_bytes.saturating_add(front.len());
                    self.tail.pop_front();
                }
                Some(front) => {
                    front.drain(..excess);
                    self.tail_bytes = self.tail_bytes.saturating_sub(excess);
                    self.omitted_bytes = self.omitted_bytes.saturating_add(excess);
                    break;
                }
                None => break,
            }
        }
    }
}

static COMMANDS: OnceLock<Mutex<HashMap<i64, ManagedCommand>>> = OnceLock::new();
static NEXT_PROCESS_ID: AtomicI64 = AtomicI64::new(1000);

#[cfg(test)]
pub(crate) fn exec_command(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
) -> Result<CommandToolResult> {
    exec_command_with_budget(store, session, call, DEFAULT_MAX_OUTPUT_TOKENS)
}

pub(crate) fn exec_command_with_budget(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
    tool_output_token_budget: usize,
) -> Result<CommandToolResult> {
    let raw_cmd = call
        .arguments
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if raw_cmd.is_empty() {
        bail!("exec_command requires cmd");
    }
    let cmd = raw_cmd.to_string();
    if let Some(reason) = dangerous_command_rejection(&cmd) {
        bail!("{reason}");
    }
    let sandbox_permissions = requested_sandbox_permissions(&call.arguments)?;
    let additional_permissions_requested =
        validate_additional_permissions_argument_shape(&call.arguments)?;
    let effective_sandbox_permissions = if additional_permissions_requested
        && matches!(sandbox_permissions, SandboxPermissions::UseDefault)
    {
        SandboxPermissions::WithAdditionalPermissions
    } else {
        sandbox_permissions
    };
    if effective_sandbox_permissions.requests_sandbox_override() {
        bail!(
            "approval policy is Never; reject command — you cannot ask for escalated permissions if the approval policy is Never"
        );
    }
    let yield_time = yield_time(&call.arguments, DEFAULT_EXEC_YIELD_TIME_MS);
    let max_chars = max_output_chars(&call.arguments, tool_output_token_budget);
    let workdir = resolve_workdir(
        session,
        call.arguments.get("workdir").and_then(Value::as_str),
    )?;
    let shell = call
        .arguments
        .get("shell")
        .and_then(Value::as_str)
        .filter(|shell| !shell.trim().is_empty())
        .map(resolve_model_shell)
        .unwrap_or_else(default_user_shell);
    let login = call
        .arguments
        .get("login")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let tty_requested = call
        .arguments
        .get("tty")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let process_id = allocate_process_id();

    store.append_event(
        &session.id,
        "tool.started",
        json!({
            "name": "exec_command",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;

    let output = Arc::new(Mutex::new(HeadTailBuffer::default()));
    let transcript = Arc::new(Mutex::new(HeadTailBuffer::default()));
    let (process, readers, tty_allocated) = spawn_process(
        &shell,
        login,
        &cmd,
        &workdir,
        tty_requested,
        output.clone(),
        transcript.clone(),
        &session.id,
    )?;
    store.append_event(
        &session.id,
        "command.started",
        json!({
            "tool_call_id": call.id,
            "session_id": process_id,
            "cmd": cmd,
            "workdir": workdir,
            "shell": shell.path_string(),
            "shell_type": shell.name(),
            "login": login,
            "tty": tty_requested,
        }),
    )?;
    let mut managed = ManagedCommand {
        session_id: session.id.clone(),
        tool_call_id: call.id.clone(),
        process,
        output,
        transcript,
        started_at: Instant::now(),
        last_used: Instant::now(),
        background_finished: false,
        readers,
    };

    wait_for_output(yield_time, || managed.process.try_wait())?;
    if let Some(status) = managed.process.try_wait()? {
        let _ = finish_readers_after_exit(&mut managed);
        let text = managed.read_recent_output();
        let aggregated_output = managed.read_transcript_output();
        emit_command_output(store, &session.id, Some(process_id), &text)?;
        emit_command_finished(
            store,
            &session.id,
            process_id,
            &call.id,
            &status,
            managed.started_at.elapsed(),
            &aggregated_output,
        )?;
        let payload = CommandOutputPayload {
            chunk_id: generate_chunk_id(),
            session_id: None,
            running: false,
            output: &text,
            max_chars,
            exit_code: status.exit_code,
            duration: managed.started_at.elapsed(),
            tty_requested,
            tty_allocated,
            write_error: None,
        };
        let content = command_output(&payload);
        let model_text = command_model_text(&payload);
        store.append_event(
            &session.id,
            "tool.finished",
            json!({
                "name": "exec_command",
                "tool_call_id": call.id,
                "output": content,
            }),
        )?;
        return Ok(CommandToolResult {
            #[cfg(test)]
            content,
            model_text,
        });
    }

    let text = managed.read_recent_output();
    emit_command_output(store, &session.id, Some(process_id), &text)?;
    store.append_event(
        &session.id,
        "command.waiting",
        json!({
            "tool_call_id": call.id,
            "session_id": process_id,
            "running": true,
        }),
    )?;
    let payload = CommandOutputPayload {
        chunk_id: generate_chunk_id(),
        session_id: Some(process_id),
        running: true,
        output: &text,
        max_chars,
        exit_code: None,
        duration: managed.started_at.elapsed(),
        tty_requested,
        tty_allocated,
        write_error: None,
    };
    let content = command_output(&payload);
    let model_text = command_model_text(&payload);
    store_running_command(store, process_id, managed);
    spawn_background_completion_watcher(
        store.state_dir().to_path_buf(),
        store.notifier(),
        process_id,
    );
    store.append_event(
        &session.id,
        "tool.finished",
        json!({
            "name": "exec_command",
            "tool_call_id": call.id,
            "output": content,
        }),
    )?;
    Ok(CommandToolResult {
        #[cfg(test)]
        content,
        model_text,
    })
}

pub(crate) fn shell_command_with_budget(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
    tool_output_token_budget: usize,
    allow_login_shell: bool,
) -> Result<CommandToolResult> {
    let raw_command = call
        .arguments
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if raw_command.is_empty() {
        bail!("shell_command requires command");
    }
    if let Some(reason) = dangerous_command_rejection(raw_command) {
        bail!("{reason}");
    }
    let login = match call.arguments.get("login").and_then(Value::as_bool) {
        Some(true) if !allow_login_shell => {
            bail!("login shell is disabled by config; omit `login` or set it to false.");
        }
        Some(login) => login,
        None => allow_login_shell,
    };
    let timeout = shell_command_timeout(&call.arguments);
    let max_chars = tool_output_token_budget
        .min(DEFAULT_MAX_OUTPUT_TOKENS)
        .saturating_mul(TOKEN_TO_CHAR_APPROX);
    let workdir = resolve_workdir(
        session,
        call.arguments.get("workdir").and_then(Value::as_str),
    )?;
    let shell = default_user_shell();

    store.append_event(
        &session.id,
        "tool.started",
        json!({
            "name": "shell_command",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    store.append_event(
        &session.id,
        "command.started",
        json!({
            "tool_call_id": call.id,
            "session_id": Value::Null,
            "cmd": raw_command,
            "workdir": workdir,
            "shell": shell.path_string(),
            "shell_type": shell.name(),
            "login": login,
            "tty": false,
        }),
    )?;

    let started_at = Instant::now();
    let mut child = spawn_shell_command_process(&shell, login, raw_command, &workdir, &session.id)?;
    let stdout = child.stdout.take().context("stdout pipe not available")?;
    let stderr = child.stderr.take().context("stderr pipe not available")?;
    let stdout_rx = spawn_capped_reader(stdout);
    let stderr_rx = spawn_capped_reader(stderr);

    let (status, timed_out) = wait_for_shell_command(&mut child, timeout)?;
    let duration = started_at.elapsed();
    let stdout = recv_capped_reader(stdout_rx);
    let stderr = recv_capped_reader(stderr_rx);
    let aggregated_output = aggregate_shell_output(&stdout, &stderr);
    emit_command_output(store, &session.id, None, &aggregated_output)?;

    let exit_code = if timed_out {
        SHELL_COMMAND_TIMEOUT_EXIT_CODE
    } else {
        status.code().unwrap_or(-1)
    };
    let success = exit_code == 0 && !timed_out;
    emit_shell_command_finished(
        store,
        &session.id,
        &call.id,
        exit_code,
        success,
        duration,
        &stdout,
        &stderr,
        &aggregated_output,
        timed_out,
    )?;
    let content = shell_command_output(exit_code, success, duration, &aggregated_output, timed_out);
    let model_text = shell_command_model_text(
        exit_code,
        duration,
        &aggregated_output,
        timed_out,
        max_chars,
    );
    store.append_event(
        &session.id,
        "tool.finished",
        json!({
            "name": "shell_command",
            "tool_call_id": call.id,
            "output": content,
        }),
    )?;
    Ok(CommandToolResult {
        #[cfg(test)]
        content,
        model_text,
    })
}

#[derive(Clone, Debug, Default)]
struct CappedOutput {
    bytes: Vec<u8>,
}

impl CappedOutput {
    fn push(&mut self, chunk: &[u8]) {
        if self.bytes.len() >= UNIFIED_EXEC_OUTPUT_MAX_BYTES {
            return;
        }
        let remaining = UNIFIED_EXEC_OUTPUT_MAX_BYTES.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).to_string()
    }
}

fn spawn_shell_command_process(
    shell: &ShellSpec,
    login: bool,
    command_text: &str,
    workdir: &Path,
    thread_id: &str,
) -> Result<StdChild> {
    let shell_path = shell.path_string();
    let mut command = Command::new(&shell_path);
    command
        .args(shell.exec_args(command_text, login))
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_unified_exec_env_to_command(&mut command, thread_id);
    command.spawn().with_context(|| {
        format!(
            "spawn shell_command via shell {} in {}",
            shell_path,
            workdir.display()
        )
    })
}

fn spawn_capped_reader<R>(mut reader: R) -> mpsc::Receiver<CappedOutput>
where
    R: Read + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut output = CappedOutput::default();
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => output.push(&buffer[..n]),
                Err(_) => break,
            }
        }
        let _ = tx.send(output);
    });
    rx
}

fn recv_capped_reader(rx: mpsc::Receiver<CappedOutput>) -> String {
    rx.recv_timeout(Duration::from_millis(SHELL_COMMAND_IO_DRAIN_TIMEOUT_MS))
        .unwrap_or_default()
        .text()
}

fn wait_for_shell_command(child: &mut StdChild, timeout: Duration) -> Result<(ExitStatus, bool)> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status, false));
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let status = child.wait()?;
            return Ok((status, true));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn aggregate_shell_output(stdout: &str, stderr: &str) -> String {
    let stdout_bytes = stdout.as_bytes();
    let stderr_bytes = stderr.as_bytes();
    let total_len = stdout_bytes.len().saturating_add(stderr_bytes.len());
    let mut aggregated = Vec::with_capacity(total_len.min(UNIFIED_EXEC_OUTPUT_MAX_BYTES));
    if total_len <= UNIFIED_EXEC_OUTPUT_MAX_BYTES {
        aggregated.extend_from_slice(stdout_bytes);
        aggregated.extend_from_slice(stderr_bytes);
    } else {
        let want_stdout = stdout_bytes.len().min(UNIFIED_EXEC_OUTPUT_MAX_BYTES / 3);
        let stderr_take = stderr_bytes
            .len()
            .min(UNIFIED_EXEC_OUTPUT_MAX_BYTES.saturating_sub(want_stdout));
        let remaining = UNIFIED_EXEC_OUTPUT_MAX_BYTES.saturating_sub(want_stdout + stderr_take);
        let stdout_take =
            want_stdout + remaining.min(stdout_bytes.len().saturating_sub(want_stdout));
        aggregated.extend_from_slice(&stdout_bytes[..stdout_take]);
        aggregated.extend_from_slice(&stderr_bytes[..stderr_take]);
    }
    String::from_utf8_lossy(&aggregated).to_string()
}

#[cfg(test)]
pub(crate) fn write_stdin(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
) -> Result<CommandToolResult> {
    write_stdin_with_budget(store, session, call, DEFAULT_MAX_OUTPUT_TOKENS)
}

pub(crate) fn write_stdin_with_budget(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
    tool_output_token_budget: usize,
) -> Result<CommandToolResult> {
    let process_id = write_stdin_session_id(&call.arguments)?;
    let chars = call
        .arguments
        .get("chars")
        .and_then(Value::as_str)
        .unwrap_or("");
    let yield_time = write_stdin_yield_time(&call.arguments, chars);
    let max_chars = max_output_chars(&call.arguments, tool_output_token_budget);
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
            .remove(&process_id)
            .with_context(|| format!("unknown command session id: {process_id}"))?
    };
    if command.session_id != session.id {
        commands()
            .lock()
            .expect("command registry poisoned")
            .insert(process_id, command);
        bail!("command session belongs to another task: {process_id}");
    }
    command.last_used = Instant::now();
    if !chars.is_empty() && !command.process.tty_allocated() {
        commands()
            .lock()
            .expect("command registry poisoned")
            .insert(process_id, command);
        bail!(STDIN_CLOSED_MESSAGE);
    }
    let pending_write_error = if !chars.is_empty() {
        match command.process.write_all(chars.as_bytes()) {
            Ok(()) => {
                thread::sleep(Duration::from_millis(WRITE_STDIN_REACTION_SETTLE_MS));
                None
            }
            Err(error) => Some(format!("{error:#}")),
        }
    } else {
        None
    };
    wait_for_output(yield_time, || command.process.try_wait())?;
    let status = command.process.try_wait()?;
    if status.is_some() {
        let _ = finish_readers_after_exit(&mut command);
    }
    let text = command.read_recent_output();
    emit_command_output(store, &session.id, Some(process_id), &text)?;

    let running = status.is_none();
    let write_error = if running {
        if let Some(message) = pending_write_error.as_ref() {
            store.append_event(
                &session.id,
                "command.write_error",
                json!({
                    "tool_call_id": call.id,
                    "session_id": process_id,
                    "error": message,
                }),
            )?;
        }
        pending_write_error.as_deref()
    } else {
        None
    };
    let tty_allocated = command.process.tty_allocated();
    let payload = CommandOutputPayload {
        chunk_id: generate_chunk_id(),
        session_id: running.then_some(process_id),
        running,
        output: &text,
        max_chars,
        exit_code: status.as_ref().and_then(|status| status.exit_code),
        duration: command.started_at.elapsed(),
        tty_requested: tty_allocated,
        tty_allocated,
        write_error,
    };
    let content = command_output(&payload);
    let model_text = command_model_text(&payload);
    if let Some(status) = status {
        if !command.background_finished {
            let aggregated_output = command.read_transcript_output();
            emit_command_finished(
                store,
                &session.id,
                process_id,
                &command.tool_call_id,
                &status,
                command.started_at.elapsed(),
                &aggregated_output,
            )?;
        }
    } else {
        commands()
            .lock()
            .expect("command registry poisoned")
            .insert(process_id, command);
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
    Ok(CommandToolResult {
        #[cfg(test)]
        content,
        model_text,
    })
}

fn commands() -> &'static Mutex<HashMap<i64, ManagedCommand>> {
    COMMANDS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn allocate_process_id() -> i64 {
    loop {
        let process_id = if should_use_deterministic_process_ids() {
            NEXT_PROCESS_ID.fetch_add(1, Ordering::Relaxed)
        } else {
            random_process_id()
        };
        let commands = commands().lock().expect("command registry poisoned");
        if !commands.contains_key(&process_id) {
            return process_id;
        }
    }
}

fn should_use_deterministic_process_ids() -> bool {
    cfg!(test)
}

fn random_process_id() -> i64 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    let raw = u128::from_be_bytes(bytes);
    1_000 + i64::try_from(raw % 99_000).expect("bounded process id")
}

fn store_running_command(store: &Store, process_id: i64, managed: ManagedCommand) {
    let pruned = {
        let mut commands = commands().lock().expect("command registry poisoned");
        let pruned = prune_processes_if_needed(&mut commands);
        commands.insert(process_id, managed);
        pruned
    };
    if let Some(mut command) = pruned {
        let _ = command.process.kill();
        let _ = command.process.wait();
        let _ = finish_readers_after_exit(&mut command);
        let _ = store.append_event(
            &command.session_id,
            "command.pruned",
            json!({
                "reason": "max_unified_exec_processes",
            }),
        );
    }
}

fn prune_processes_if_needed(
    commands: &mut HashMap<i64, ManagedCommand>,
) -> Option<ManagedCommand> {
    if commands.len() < MAX_UNIFIED_EXEC_PROCESSES {
        return None;
    }
    let meta = commands
        .iter()
        .map(|(process_id, command)| (*process_id, command.last_used, command.background_finished))
        .collect::<Vec<_>>();
    let process_id = process_id_to_prune_from_meta(&meta)?;
    commands.remove(&process_id)
}

fn process_id_to_prune_from_meta(meta: &[(i64, Instant, bool)]) -> Option<i64> {
    if meta.is_empty() {
        return None;
    }
    let mut by_recency = meta.to_vec();
    by_recency.sort_by_key(|(_, last_used, _)| std::cmp::Reverse(*last_used));
    let protected = by_recency
        .iter()
        .take(8)
        .map(|(process_id, _, _)| *process_id)
        .collect::<std::collections::HashSet<_>>();

    let mut lru = meta.to_vec();
    lru.sort_by_key(|(_, last_used, _)| *last_used);
    if let Some((process_id, _, _)) = lru
        .iter()
        .find(|(process_id, _, exited)| !protected.contains(process_id) && *exited)
    {
        return Some(*process_id);
    }
    lru.into_iter()
        .find(|(process_id, _, _)| !protected.contains(process_id))
        .map(|(process_id, _, _)| process_id)
}

fn spawn_background_completion_watcher(
    state_dir: PathBuf,
    notifier: Option<StoreNotifier>,
    process_id: i64,
) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(100));
        let status = {
            let mut commands = commands().lock().expect("command registry poisoned");
            let Some(command) = commands.get_mut(&process_id) else {
                return;
            };
            if command.background_finished {
                return;
            }
            let status = match command.process.try_wait() {
                Ok(Some(status)) => status,
                Ok(None) => continue,
                Err(_) => return,
            };
            status
        };
        thread::sleep(Duration::from_millis(BACKGROUND_TRAILING_OUTPUT_GRACE_MS));
        let event = {
            let mut commands = commands().lock().expect("command registry poisoned");
            let Some(command) = commands.get_mut(&process_id) else {
                return;
            };
            if command.background_finished {
                return;
            }
            let readers_finished = finish_readers_after_exit(command);
            let recent_output = command.read_recent_output();
            let aggregated_output = command.read_transcript_output();
            command.background_finished = true;
            if !readers_finished {
                command.detach_reader_buffers();
            }
            Some((
                command.session_id.clone(),
                command.tool_call_id.clone(),
                status,
                command.started_at.elapsed(),
                recent_output,
                aggregated_output,
            ))
        };
        if let Some((
            session_id,
            tool_call_id,
            status,
            duration,
            recent_output,
            aggregated_output,
        )) = event
        {
            if let Ok(store) = Store::open_with_optional_notifier(&state_dir, notifier.clone()) {
                let _ = emit_command_output(&store, &session_id, Some(process_id), &recent_output);
                let _ = emit_command_finished(
                    &store,
                    &session_id,
                    process_id,
                    &tool_call_id,
                    &status,
                    duration,
                    &aggregated_output,
                );
            }
        }
        return;
    });
}

fn write_stdin_session_id(arguments: &Value) -> Result<i64> {
    let Some(value) = arguments.get("session_id") else {
        bail!("write_stdin requires numeric session_id");
    };
    let Some(process_id) = value.as_i64() else {
        bail!("write_stdin requires numeric session_id");
    };
    if process_id <= 0 {
        bail!("write_stdin requires positive session_id");
    }
    Ok(process_id)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SandboxPermissions {
    UseDefault,
    RequireEscalated,
    WithAdditionalPermissions,
}

impl SandboxPermissions {
    fn requests_sandbox_override(self) -> bool {
        !matches!(self, Self::UseDefault)
    }
}

fn requested_sandbox_permissions(arguments: &Value) -> Result<SandboxPermissions> {
    let Some(value) = arguments.get("sandbox_permissions") else {
        return Ok(SandboxPermissions::UseDefault);
    };
    let Some(raw) = value.as_str().map(str::trim) else {
        bail!("sandbox_permissions must be a string");
    };
    match raw {
        "use_default" => Ok(SandboxPermissions::UseDefault),
        "require_escalated" => Ok(SandboxPermissions::RequireEscalated),
        "with_additional_permissions" => Ok(SandboxPermissions::WithAdditionalPermissions),
        other => bail!(
            "unsupported sandbox_permissions value {other:?}; expected use_default, require_escalated, or with_additional_permissions"
        ),
    }
}

fn validate_additional_permissions_argument_shape(arguments: &Value) -> Result<bool> {
    let Some(value) = arguments.get("additional_permissions") else {
        return Ok(false);
    };
    if value.is_null() {
        return Ok(false);
    }
    if !value.is_object() {
        bail!(
            "failed to parse function arguments: invalid type: {}, expected struct AdditionalPermissionProfile",
            json_type_name(value)
        );
    }
    Ok(true)
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[allow(dead_code)]
pub(crate) fn cleanup_session_commands(session_id: &str) -> usize {
    cleanup_commands_matching(|command| command.session_id == session_id)
}

#[allow(dead_code)]
pub(crate) fn cleanup_all_commands() -> usize {
    cleanup_commands_matching(|_| true)
}

fn cleanup_commands_matching(predicate: impl Fn(&ManagedCommand) -> bool) -> usize {
    let mut pending = Vec::new();
    {
        let mut commands = commands().lock().expect("command registry poisoned");
        let process_ids = commands
            .iter()
            .filter_map(|(process_id, command)| predicate(command).then_some(*process_id))
            .collect::<Vec<_>>();
        for process_id in process_ids {
            if let Some(command) = commands.remove(&process_id) {
                pending.push(command);
            }
        }
    }

    let count = pending.len();
    for mut command in pending {
        let _ = command.process.kill();
        let _ = command.process.wait();
        let _ = finish_readers_after_exit(&mut command);
    }
    count
}

#[cfg(test)]
pub(crate) fn exec_command_is_known_read_only(arguments: &Value) -> bool {
    command_arguments_are_known_read_only(arguments, "cmd")
}

#[cfg(test)]
pub(crate) fn shell_command_is_known_read_only(arguments: &Value) -> bool {
    command_arguments_are_known_read_only(arguments, "command")
}

#[cfg(test)]
fn command_arguments_are_known_read_only(arguments: &Value, command_key: &str) -> bool {
    let cmd = arguments
        .get(command_key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if cmd.is_empty() {
        return false;
    }
    let Some(commands) = commands_for_local_exec_policy(cmd) else {
        return false;
    };
    !commands.is_empty()
        && commands
            .iter()
            .all(|words| command_words_are_known_read_only(words))
}

fn spawn_process(
    shell: &ShellSpec,
    login: bool,
    cmd: &str,
    workdir: &Path,
    tty_requested: bool,
    output: Arc<Mutex<HeadTailBuffer>>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    thread_id: &str,
) -> Result<(ManagedProcess, Vec<JoinHandle<()>>, bool)> {
    if tty_requested {
        return spawn_pty_process(shell, login, cmd, workdir, output, transcript, thread_id);
    }
    spawn_pipe_process(shell, login, cmd, workdir, output, transcript, thread_id)
}

fn spawn_pipe_process(
    shell: &ShellSpec,
    login: bool,
    cmd: &str,
    workdir: &Path,
    output: Arc<Mutex<HeadTailBuffer>>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    thread_id: &str,
) -> Result<(ManagedProcess, Vec<JoinHandle<()>>, bool)> {
    let shell_path = shell.path_string();
    let mut command = Command::new(&shell_path);
    command
        .args(shell.exec_args(cmd, login))
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_unified_exec_env_to_command(&mut command, thread_id);
    let mut child = command.spawn().with_context(|| {
        format!(
            "spawn command via shell {} in {}",
            shell_path,
            workdir.display()
        )
    })?;
    let stdin = child.stdin.take();
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_reader(stdout, output.clone(), transcript.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_reader(stderr, output, transcript));
    }
    Ok((ManagedProcess::Pipes { child, stdin }, readers, false))
}

fn spawn_pty_process(
    shell: &ShellSpec,
    login: bool,
    cmd: &str,
    workdir: &Path,
    output: Arc<Mutex<HeadTailBuffer>>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    thread_id: &str,
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
    let shell_path = shell.path_string();
    let mut command = CommandBuilder::new(&shell_path);
    command.args(shell.exec_args(cmd, login));
    command.cwd(workdir.as_os_str());
    apply_unified_exec_env_to_pty_command(&mut command, thread_id);
    let child = pair.slave.spawn_command(command).with_context(|| {
        format!(
            "spawn pty command via shell {} in {}",
            shell_path,
            workdir.display()
        )
    })?;
    let readers = vec![spawn_reader(reader, output, transcript)];
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

fn apply_unified_exec_env_to_command(command: &mut Command, thread_id: &str) {
    for (key, value) in UNIFIED_EXEC_ENV {
        command.env(key, value);
    }
    command.env("CODEX_THREAD_ID", thread_id);
}

fn apply_unified_exec_env_to_pty_command(command: &mut CommandBuilder, thread_id: &str) {
    for (key, value) in UNIFIED_EXEC_ENV {
        command.env(key, value);
    }
    command.env("CODEX_THREAD_ID", thread_id);
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

fn spawn_reader<R>(
    mut reader: R,
    output: Arc<Mutex<HeadTailBuffer>>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
) -> JoinHandle<()>
where
    R: std::io::Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => push_command_output_chunk(&output, &transcript, buffer[..n].to_vec()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    push_command_output_chunk(
                        &output,
                        &transcript,
                        format!("[command output read failed: {error}]\n").into_bytes(),
                    );
                    break;
                }
            }
        }
    })
}

fn push_command_output_chunk(
    output: &Arc<Mutex<HeadTailBuffer>>,
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    chunk: Vec<u8>,
) {
    output
        .lock()
        .expect("command output poisoned")
        .push_chunk(chunk.clone());
    transcript
        .lock()
        .expect("command transcript poisoned")
        .push_chunk(chunk);
}

fn finish_readers_after_exit(command: &mut ManagedCommand) -> bool {
    let timeout = Duration::from_millis(POST_EXIT_READER_DRAIN_TIMEOUT_MS);
    let started = Instant::now();
    let mut pending = Vec::new();
    std::mem::swap(&mut pending, &mut command.readers);
    while !pending.is_empty() {
        let mut still_running = Vec::new();
        for reader in pending.drain(..) {
            if reader.is_finished() {
                let _ = reader.join();
            } else {
                still_running.push(reader);
            }
        }
        if still_running.is_empty() {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        thread::sleep(Duration::from_millis(5).min(remaining));
        pending = still_running;
    }
    true
}

impl ManagedCommand {
    fn read_recent_output(&mut self) -> String {
        let mut output = self.output.lock().expect("command output poisoned");
        String::from_utf8_lossy(&output.drain_bytes()).to_string()
    }

    fn read_transcript_output(&self) -> String {
        let transcript = self.transcript.lock().expect("command transcript poisoned");
        String::from_utf8_lossy(&transcript.to_bytes()).to_string()
    }

    fn detach_reader_buffers(&mut self) {
        self.output = Arc::new(Mutex::new(HeadTailBuffer::default()));
        self.transcript = Arc::new(Mutex::new(HeadTailBuffer::default()));
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
    process_id: Option<i64>,
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

fn emit_command_finished(
    store: &Store,
    session_id: &str,
    process_id: i64,
    tool_call_id: &str,
    status: &ProcessExit,
    duration: Duration,
    aggregated_output: &str,
) -> Result<()> {
    store.append_event(
        session_id,
        "command.finished",
        json!({
            "tool_call_id": tool_call_id,
            "session_id": process_id,
            "exit_code": status.exit_code,
            "success": status.success,
            "duration_ms": duration.as_millis() as u64,
            "stdout": aggregated_output,
            "stderr": "",
            "aggregated_output": aggregated_output,
            "timed_out": false,
        }),
    )?;
    Ok(())
}

fn emit_shell_command_finished(
    store: &Store,
    session_id: &str,
    tool_call_id: &str,
    exit_code: i32,
    success: bool,
    duration: Duration,
    stdout: &str,
    stderr: &str,
    aggregated_output: &str,
    timed_out: bool,
) -> Result<()> {
    store.append_event(
        session_id,
        "command.finished",
        json!({
            "tool_call_id": tool_call_id,
            "session_id": Value::Null,
            "exit_code": exit_code,
            "success": success,
            "duration_ms": duration.as_millis() as u64,
            "stdout": stdout,
            "stderr": stderr,
            "aggregated_output": aggregated_output,
            "timed_out": timed_out,
        }),
    )?;
    Ok(())
}

struct CommandOutputPayload<'a> {
    chunk_id: String,
    session_id: Option<i64>,
    running: bool,
    output: &'a str,
    max_chars: usize,
    exit_code: Option<i32>,
    duration: Duration,
    tty_requested: bool,
    tty_allocated: bool,
    write_error: Option<&'a str>,
}

fn shell_command_output(
    exit_code: i32,
    success: bool,
    duration: Duration,
    output: &str,
    timed_out: bool,
) -> Value {
    json!({
        "running": false,
        "output": output,
        "metadata": {
            "exit_code": exit_code,
            "duration_ms": duration.as_millis() as u64,
            "success": success,
            "timed_out": timed_out,
        }
    })
}

fn command_output(payload: &CommandOutputPayload<'_>) -> Value {
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
            "write_error": payload.write_error,
        }
    })
}

fn shell_command_model_text(
    exit_code: i32,
    duration: Duration,
    output: &str,
    timed_out: bool,
    max_chars: usize,
) -> String {
    let duration_seconds = ((duration.as_secs_f32()) * 10.0).round() / 10.0;
    let content = if timed_out {
        format!(
            "command timed out after {} milliseconds\n{}",
            duration.as_millis(),
            output
        )
    } else {
        output.to_string()
    };
    let max_tokens = max_chars / TOKEN_TO_CHAR_APPROX;
    let (formatted_output, truncated) = truncate_for_model_text(&content, max_tokens);
    let mut sections = Vec::new();
    sections.push(format!("Exit code: {exit_code}"));
    sections.push(format!("Wall time: {duration_seconds} seconds"));
    if truncated {
        sections.push(format!("Total output lines: {}", content.lines().count()));
    }
    sections.push("Output:".to_string());
    sections.push(formatted_output);
    sections.join("\n")
}

fn command_model_text(payload: &CommandOutputPayload<'_>) -> String {
    let output =
        codex_formatted_truncate_text(payload.output, payload.max_chars / TOKEN_TO_CHAR_APPROX);
    let mut sections = Vec::new();
    if !payload.chunk_id.is_empty() {
        sections.push(format!("Chunk ID: {}", payload.chunk_id));
    }
    sections.push(format!(
        "Wall time: {:.4} seconds",
        payload.duration.as_secs_f64()
    ));
    if let Some(exit_code) = payload.exit_code {
        sections.push(format!("Process exited with code {exit_code}"));
    }
    if payload.running {
        if let Some(session_id) = &payload.session_id {
            sections.push(format!("Process running with session ID {session_id}"));
        }
    }
    if let Some(error) = payload.write_error {
        sections.push(format!("Write error: {error}"));
    }
    sections.push(format!(
        "Original token count: {}",
        approximate_token_count(payload.output)
    ));
    sections.push("Output:".to_string());
    sections.push(output);
    sections.join("\n")
}

fn truncate_for_model_text(content: &str, max_tokens: usize) -> (String, bool) {
    if content.len() <= max_tokens.saturating_mul(TOKEN_TO_CHAR_APPROX) {
        return (content.to_string(), false);
    }
    (truncate_middle_with_token_budget(content, max_tokens), true)
}

fn generate_chunk_id() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    format!("{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2])
}

fn approximate_token_count(text: &str) -> usize {
    text.len().div_ceil(TOKEN_TO_CHAR_APPROX)
}

pub(crate) fn codex_formatted_truncate_text(content: &str, max_tokens: usize) -> String {
    if content.len() <= max_tokens.saturating_mul(TOKEN_TO_CHAR_APPROX) {
        return content.to_string();
    }
    let total_lines = content.lines().count();
    let truncated = truncate_middle_with_token_budget(content, max_tokens);
    format!("Total output lines: {total_lines}\n\n{truncated}")
}

fn truncate_middle_with_token_budget(content: &str, max_tokens: usize) -> String {
    if content.is_empty() {
        return String::new();
    }
    truncate_middle_with_byte_estimate(content, max_tokens.saturating_mul(TOKEN_TO_CHAR_APPROX))
}

fn truncate_middle_with_byte_estimate(content: &str, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return format!("…{} tokens truncated…", approximate_token_count(content));
    }
    if content.len() <= max_bytes {
        return content.to_string();
    }

    let left_budget = max_bytes / 2;
    let right_budget = max_bytes.saturating_sub(left_budget);
    let tail_start_target = content.len().saturating_sub(right_budget);
    let mut prefix_end = 0usize;
    let mut suffix_start = content.len();
    let mut removed_chars = 0usize;
    let mut suffix_started = false;

    for (idx, ch) in content.char_indices() {
        let char_end = idx + ch.len_utf8();
        if char_end <= left_budget {
            prefix_end = char_end;
            continue;
        }
        if idx >= tail_start_target {
            if !suffix_started {
                suffix_start = idx;
                suffix_started = true;
            }
            continue;
        }
        removed_chars = removed_chars.saturating_add(1);
    }

    if suffix_start < prefix_end {
        suffix_start = prefix_end;
    }
    let removed_bytes = content.len().saturating_sub(max_bytes);
    let removed_tokens = removed_bytes.div_ceil(TOKEN_TO_CHAR_APPROX);
    let marker = if removed_tokens == 0 {
        format!("…{removed_chars} chars truncated…")
    } else {
        format!("…{removed_tokens} tokens truncated…")
    };
    format!(
        "{}{}{}",
        &content[..prefix_end],
        marker,
        &content[suffix_start..]
    )
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

fn max_output_chars(arguments: &Value, tool_output_token_budget: usize) -> usize {
    let tokens = arguments
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .and_then(|tokens| usize::try_from(tokens).ok())
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
        .min(DEFAULT_MAX_OUTPUT_TOKENS)
        .min(tool_output_token_budget);
    tokens.saturating_mul(TOKEN_TO_CHAR_APPROX)
}

fn write_stdin_yield_time(arguments: &Value, chars: &str) -> Duration {
    let millis = if chars.is_empty() {
        arguments
            .get("yield_time_ms")
            .and_then(Value::as_u64)
            .unwrap_or(MIN_EMPTY_POLL_YIELD_TIME_MS)
            .max(MIN_YIELD_TIME_MS)
            .clamp(MIN_EMPTY_POLL_YIELD_TIME_MS, MAX_EMPTY_POLL_YIELD_TIME_MS)
    } else {
        arguments
            .get("yield_time_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_WRITE_STDIN_YIELD_TIME_MS)
            .max(MIN_YIELD_TIME_MS)
            .min(MAX_YIELD_TIME_MS)
    };
    Duration::from_millis(millis)
}

fn yield_time(arguments: &Value, default_ms: u64) -> Duration {
    let millis = arguments
        .get("yield_time_ms")
        .and_then(Value::as_u64)
        .unwrap_or(default_ms)
        .clamp(MIN_YIELD_TIME_MS, MAX_YIELD_TIME_MS);
    Duration::from_millis(millis)
}

fn shell_command_timeout(arguments: &Value) -> Duration {
    let millis = arguments
        .get("timeout_ms")
        .or_else(|| arguments.get("timeout"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_SHELL_COMMAND_TIMEOUT_MS);
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

fn default_user_shell() -> ShellSpec {
    let path_dirs = path_dirs_from_env();
    let default_shell_path = std::env::var_os("SHELL").map(PathBuf::from);
    default_user_shell_with_path_lookup(&path_dirs, default_shell_path)
}

fn resolve_model_shell(shell: &str) -> ShellSpec {
    let path_dirs = path_dirs_from_env();
    let default_shell_path = std::env::var_os("SHELL").map(PathBuf::from);
    resolve_model_shell_with_path_lookup(shell, &path_dirs, default_shell_path)
}

fn resolve_model_shell_with_path_lookup(
    shell: &str,
    path_dirs: &[PathBuf],
    default_shell_path: Option<PathBuf>,
) -> ShellSpec {
    let path = PathBuf::from(shell.trim());
    detect_shell_type(&path)
        .and_then(|shell_type| get_shell(shell_type, Some(&path), path_dirs, default_shell_path))
        .unwrap_or_else(ultimate_fallback_shell)
}

fn default_user_shell_with_path_lookup(
    path_dirs: &[PathBuf],
    default_shell_path: Option<PathBuf>,
) -> ShellSpec {
    if cfg!(windows) {
        return get_shell(
            ShellType::PowerShell,
            None,
            path_dirs,
            default_shell_path.clone(),
        )
        .unwrap_or_else(ultimate_fallback_shell);
    }

    let user_default_shell = default_shell_path
        .as_ref()
        .and_then(|shell| detect_shell_type(shell))
        .and_then(|shell_type| get_shell(shell_type, None, path_dirs, default_shell_path.clone()));

    let shell_with_fallback = if cfg!(target_os = "macos") {
        user_default_shell
            .or_else(|| get_shell(ShellType::Zsh, None, path_dirs, default_shell_path.clone()))
            .or_else(|| get_shell(ShellType::Bash, None, path_dirs, default_shell_path.clone()))
    } else {
        user_default_shell
            .or_else(|| get_shell(ShellType::Bash, None, path_dirs, default_shell_path.clone()))
            .or_else(|| get_shell(ShellType::Zsh, None, path_dirs, default_shell_path.clone()))
    };

    shell_with_fallback.unwrap_or_else(ultimate_fallback_shell)
}

fn get_shell(
    shell_type: ShellType,
    provided_path: Option<&PathBuf>,
    path_dirs: &[PathBuf],
    default_shell_path: Option<PathBuf>,
) -> Option<ShellSpec> {
    match shell_type {
        ShellType::Zsh => get_shell_from_candidates(
            ShellType::Zsh,
            provided_path,
            "zsh",
            &["/bin/zsh"],
            path_dirs,
            default_shell_path,
        ),
        ShellType::Bash => get_shell_from_candidates(
            ShellType::Bash,
            provided_path,
            "bash",
            &["/bin/bash"],
            path_dirs,
            default_shell_path,
        ),
        ShellType::PowerShell => get_shell_from_candidates(
            ShellType::PowerShell,
            provided_path,
            "pwsh",
            powershell_core_fallback_paths(),
            path_dirs,
            default_shell_path.clone(),
        )
        .or_else(|| {
            get_shell_from_candidates(
                ShellType::PowerShell,
                provided_path,
                "powershell",
                powershell_legacy_fallback_paths(),
                path_dirs,
                default_shell_path,
            )
        }),
        ShellType::Sh => get_shell_from_candidates(
            ShellType::Sh,
            provided_path,
            "sh",
            &["/bin/sh"],
            path_dirs,
            default_shell_path,
        ),
        ShellType::Cmd => get_shell_from_candidates(
            ShellType::Cmd,
            provided_path,
            "cmd",
            cmd_fallback_paths(),
            path_dirs,
            default_shell_path,
        ),
    }
}

fn get_shell_from_candidates(
    shell_type: ShellType,
    provided_path: Option<&PathBuf>,
    binary_name: &str,
    fallback_paths: &[&str],
    path_dirs: &[PathBuf],
    default_shell_path: Option<PathBuf>,
) -> Option<ShellSpec> {
    if let Some(shell_path) = provided_path.and_then(|path| file_exists(path)) {
        return Some(ShellSpec {
            shell_type,
            shell_path,
        });
    }

    if let Some(default_shell_path) = default_shell_path
        .as_ref()
        .filter(|path| detect_shell_type(path) == Some(shell_type))
        .and_then(|path| file_exists(path))
    {
        return Some(ShellSpec {
            shell_type,
            shell_path: default_shell_path,
        });
    }

    if let Some(shell_path) = find_on_path(binary_name, path_dirs) {
        return Some(ShellSpec {
            shell_type,
            shell_path,
        });
    }

    fallback_paths
        .iter()
        .find_map(|path| file_exists(Path::new(path)))
        .map(|shell_path| ShellSpec {
            shell_type,
            shell_path,
        })
}

fn path_dirs_from_env() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default()
}

fn file_exists(path: &Path) -> Option<PathBuf> {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file())
        .then(|| path.to_path_buf())
}

fn find_on_path(binary_name: &str, path_dirs: &[PathBuf]) -> Option<PathBuf> {
    path_dirs
        .iter()
        .map(|dir| dir.join(binary_name))
        .find(|path| path_is_executable_file(path))
}

fn path_is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    path_is_executable_metadata(&metadata)
}

#[cfg(unix)]
fn path_is_executable_metadata(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn path_is_executable_metadata(_metadata: &std::fs::Metadata) -> bool {
    true
}

fn powershell_core_fallback_paths() -> &'static [&'static str] {
    if cfg!(windows) {
        &[r#"C:\Program Files\PowerShell\7\pwsh.exe"#]
    } else {
        &["/usr/local/bin/pwsh"]
    }
}

fn powershell_legacy_fallback_paths() -> &'static [&'static str] {
    if cfg!(windows) {
        &[r#"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"#]
    } else {
        &[]
    }
}

fn cmd_fallback_paths() -> &'static [&'static str] {
    if cfg!(windows) {
        &[r#"C:\Windows\System32\cmd.exe"#]
    } else {
        &[]
    }
}

fn ultimate_fallback_shell() -> ShellSpec {
    if cfg!(windows) {
        ShellSpec {
            shell_type: ShellType::Cmd,
            shell_path: PathBuf::from("cmd.exe"),
        }
    } else {
        ShellSpec {
            shell_type: ShellType::Sh,
            shell_path: PathBuf::from("/bin/sh"),
        }
    }
}

fn detect_shell_type(shell_path: &Path) -> Option<ShellType> {
    match shell_path.as_os_str().to_str() {
        Some("zsh") => Some(ShellType::Zsh),
        Some("sh") => Some(ShellType::Sh),
        Some("cmd") => Some(ShellType::Cmd),
        Some("bash") => Some(ShellType::Bash),
        Some("pwsh") => Some(ShellType::PowerShell),
        Some("powershell") => Some(ShellType::PowerShell),
        _ => {
            let shell_name = shell_path.file_stem()?;
            let shell_name_path = Path::new(shell_name);
            if shell_name_path == shell_path {
                None
            } else {
                detect_shell_type(shell_name_path)
            }
        }
    }
}

fn dangerous_command_rejection(cmd: &str) -> Option<String> {
    let dangerous = commands_for_local_exec_policy(cmd)
        .unwrap_or_default()
        .iter()
        .any(|words| command_words_might_be_dangerous(words));
    dangerous.then(|| {
        format!(
            "exec_command rejected `{}`: destructive command would require Codex approval, but this harness has no command approval flow yet",
            cmd
        )
    })
}

fn commands_for_local_exec_policy(cmd: &str) -> Option<Vec<Vec<String>>> {
    let commands = rough_shell_word_commands(cmd)?;
    if commands.len() == 1 {
        if let Some(inner) = shell_lc_plain_commands(&commands[0]) {
            return Some(inner);
        }
    }
    Some(commands)
}

fn shell_lc_plain_commands(command: &[String]) -> Option<Vec<Vec<String>>> {
    let [shell, flag, script] = command else {
        return None;
    };
    if !matches!(flag.as_str(), "-lc" | "-c") {
        return None;
    }
    let shell_name = executable_name_lookup_key(shell)?;
    if !matches!(shell_name.as_str(), "bash" | "zsh" | "sh") {
        return None;
    }
    let inner = rough_shell_word_commands(script)?;
    (!inner.is_empty()).then_some(inner)
}

fn rough_shell_word_commands(cmd: &str) -> Option<Vec<Vec<String>>> {
    let tree = try_parse_shell(cmd)?;
    try_parse_word_only_commands_sequence(&tree, cmd)
}

fn try_parse_shell(shell_lc_arg: &str) -> Option<Tree> {
    let lang = BASH.into();
    let mut parser = Parser::new();
    parser.set_language(&lang).expect("load bash grammar");
    parser.parse(shell_lc_arg, None)
}

fn try_parse_word_only_commands_sequence(tree: &Tree, src: &str) -> Option<Vec<Vec<String>>> {
    if tree.root_node().has_error() {
        return None;
    }

    const ALLOWED_KINDS: &[&str] = &[
        "program",
        "list",
        "pipeline",
        "command",
        "command_name",
        "word",
        "string",
        "string_content",
        "raw_string",
        "number",
        "concatenation",
    ];
    const ALLOWED_PUNCT_TOKENS: &[&str] = &["&&", "||", ";", "|", "\"", "'"];

    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    let mut command_nodes = Vec::new();
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if node.is_named() {
            if !ALLOWED_KINDS.contains(&kind) {
                return None;
            }
            if kind == "command" {
                command_nodes.push(node);
            }
        } else {
            if kind.chars().any(|ch| "&;|".contains(ch)) && !ALLOWED_PUNCT_TOKENS.contains(&kind) {
                return None;
            }
            if !(ALLOWED_PUNCT_TOKENS.contains(&kind) || kind.trim().is_empty()) {
                return None;
            }
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }

    command_nodes.sort_by_key(Node::start_byte);

    let mut commands = Vec::new();
    for node in command_nodes {
        let words = parse_plain_command_from_node(node, src)?;
        commands.push(words);
    }
    Some(commands)
}

fn parse_plain_command_from_node(cmd: Node<'_>, src: &str) -> Option<Vec<String>> {
    if cmd.kind() != "command" {
        return None;
    }
    let mut words = Vec::new();
    let mut cursor = cmd.walk();
    for child in cmd.named_children(&mut cursor) {
        match child.kind() {
            "command_name" => {
                let word_node = child.named_child(0)?;
                if word_node.kind() != "word" {
                    return None;
                }
                words.push(word_node.utf8_text(src.as_bytes()).ok()?.to_owned());
            }
            "word" | "number" => {
                words.push(child.utf8_text(src.as_bytes()).ok()?.to_owned());
            }
            "string" => {
                words.push(parse_double_quoted_string(child, src)?);
            }
            "raw_string" => {
                words.push(parse_raw_string(child, src)?);
            }
            "concatenation" => {
                let mut concatenated = String::new();
                let mut concat_cursor = child.walk();
                for part in child.named_children(&mut concat_cursor) {
                    match part.kind() {
                        "word" | "number" => {
                            concatenated.push_str(part.utf8_text(src.as_bytes()).ok()?);
                        }
                        "string" => {
                            concatenated.push_str(&parse_double_quoted_string(part, src)?);
                        }
                        "raw_string" => {
                            concatenated.push_str(&parse_raw_string(part, src)?);
                        }
                        _ => return None,
                    }
                }
                if concatenated.is_empty() {
                    return None;
                }
                words.push(concatenated);
            }
            _ => return None,
        }
    }
    Some(words)
}

fn parse_double_quoted_string(node: Node<'_>, src: &str) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut cursor = node.walk();
    for part in node.named_children(&mut cursor) {
        if part.kind() != "string_content" {
            return None;
        }
    }
    let raw = node.utf8_text(src.as_bytes()).ok()?;
    let stripped = raw
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))?;
    Some(stripped.to_string())
}

fn parse_raw_string(node: Node<'_>, src: &str) -> Option<String> {
    if node.kind() != "raw_string" {
        return None;
    }
    let raw = node.utf8_text(src.as_bytes()).ok()?;
    raw.strip_prefix('\'')
        .and_then(|text| text.strip_suffix('\''))
        .map(str::to_owned)
}

#[cfg(test)]
fn command_words_are_known_read_only(words: &[String]) -> bool {
    let Some(first) = words.first().map(String::as_str) else {
        return false;
    };
    let command = executable_name_lookup_key(first).unwrap_or_else(|| first.to_string());
    match command.as_str() {
        "numfmt" | "tac" if cfg!(target_os = "linux") => true,
        "cat" | "cd" | "cut" | "echo" | "expr" | "false" | "grep" | "head" | "id" | "ls" | "nl"
        | "paste" | "pwd" | "rev" | "seq" | "stat" | "tail" | "tr" | "true" | "uname" | "uniq"
        | "wc" | "which" | "whoami" => true,
        "base64" => base64_command_is_read_only(&words[1..]),
        "find" => find_command_is_read_only(&words[1..]),
        "rg" => rg_command_is_read_only(&words[1..]),
        "git" => git_command_is_read_only(words),
        "sed" => sed_command_is_read_only(words),
        _ => false,
    }
}

fn command_words_might_be_dangerous(words: &[String]) -> bool {
    let Some(first) = words
        .first()
        .and_then(|word| executable_name_lookup_key(word))
    else {
        return false;
    };
    match first.as_str() {
        "rm" => matches!(words.get(1).map(String::as_str), Some("-f" | "-rf")),
        "sudo" => command_words_might_be_dangerous(&words[1..]),
        _ => false,
    }
}

#[cfg(test)]
fn base64_command_is_read_only(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        matches!(arg.as_str(), "-o" | "--output")
            || arg.starts_with("--output=")
            || (arg.starts_with("-o") && arg != "-o")
    })
}

#[cfg(test)]
fn rg_command_is_read_only(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--pre" | "--hostname-bin" | "--search-zip" | "-z"
        ) || arg.starts_with("--pre=")
            || arg.starts_with("--hostname-bin=")
    })
}

#[cfg(test)]
fn sed_command_is_read_only(words: &[String]) -> bool {
    words.len() <= 4
        && words.get(1).map(String::as_str) == Some("-n")
        && is_valid_sed_n_arg(words.get(2).map(String::as_str))
}

#[cfg(test)]
fn git_command_is_read_only(words: &[String]) -> bool {
    let Some((subcommand_idx, subcommand)) =
        find_git_subcommand(words, &["status", "log", "diff", "show", "branch"])
    else {
        return false;
    };

    let global_args = &words[1..subcommand_idx];
    if git_has_unsafe_global_option(global_args) {
        return false;
    }

    let subcommand_args = &words[subcommand_idx + 1..];
    match subcommand {
        "status" | "log" | "diff" | "show" => git_subcommand_args_are_read_only(subcommand_args),
        "branch" => {
            git_subcommand_args_are_read_only(subcommand_args)
                && git_branch_is_read_only(subcommand_args)
        }
        _ => false,
    }
}

#[cfg(test)]
fn find_git_subcommand<'a>(words: &'a [String], subcommands: &[&str]) -> Option<(usize, &'a str)> {
    let first = words
        .first()
        .and_then(|word| executable_name_lookup_key(word))?;
    if first != "git" {
        return None;
    }

    let mut skip_next = false;
    for (idx, arg) in words.iter().enumerate().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }

        let arg = arg.as_str();
        if is_git_global_option_with_inline_value(arg) {
            continue;
        }
        if is_git_global_option_with_value(arg) {
            skip_next = true;
            continue;
        }
        if arg == "--" || arg.starts_with('-') {
            continue;
        }
        if subcommands.contains(&arg) {
            return Some((idx, arg));
        }
        return None;
    }
    None
}

#[cfg(test)]
fn git_branch_is_read_only(args: &[String]) -> bool {
    if args.is_empty() {
        return true;
    }

    let mut saw_read_only_flag = false;
    for arg in args.iter().map(String::as_str) {
        match arg {
            "--list" | "-l" | "--show-current" | "-a" | "--all" | "-r" | "--remotes" | "-v"
            | "-vv" | "--verbose" => saw_read_only_flag = true,
            _ if arg.starts_with("--format=") => saw_read_only_flag = true,
            _ => return false,
        }
    }
    saw_read_only_flag
}

#[derive(Clone, Copy)]
#[cfg(test)]
enum GitOptionPattern {
    Exact(&'static str),
    ShortWithInlineValue(&'static str),
    Prefix(&'static str),
}

#[cfg(test)]
impl GitOptionPattern {
    fn matches(self, arg: &str) -> bool {
        match self {
            Self::Exact(option) => arg == option,
            Self::ShortWithInlineValue(option) => {
                arg.starts_with(option) && arg.len() > option.len()
            }
            Self::Prefix(prefix) => arg.starts_with(prefix),
        }
    }
}

#[cfg(test)]
const UNSAFE_GIT_GLOBAL_OPTIONS: &[GitOptionPattern] = &[
    GitOptionPattern::Exact("-C"),
    GitOptionPattern::ShortWithInlineValue("-C"),
    GitOptionPattern::Exact("-c"),
    GitOptionPattern::ShortWithInlineValue("-c"),
    GitOptionPattern::Exact("-p"),
    GitOptionPattern::Exact("--config-env"),
    GitOptionPattern::Prefix("--config-env="),
    GitOptionPattern::Exact("--exec-path"),
    GitOptionPattern::Prefix("--exec-path="),
    GitOptionPattern::Exact("--git-dir"),
    GitOptionPattern::Prefix("--git-dir="),
    GitOptionPattern::Exact("--namespace"),
    GitOptionPattern::Prefix("--namespace="),
    GitOptionPattern::Exact("--paginate"),
    GitOptionPattern::Exact("--super-prefix"),
    GitOptionPattern::Prefix("--super-prefix="),
    GitOptionPattern::Exact("--work-tree"),
    GitOptionPattern::Prefix("--work-tree="),
];

#[cfg(test)]
const UNSAFE_GIT_SUBCOMMAND_OPTIONS: &[GitOptionPattern] = &[
    GitOptionPattern::Exact("--output"),
    GitOptionPattern::Prefix("--output="),
    GitOptionPattern::Exact("--ext-diff"),
    GitOptionPattern::Exact("--textconv"),
    GitOptionPattern::Exact("--exec"),
    GitOptionPattern::Prefix("--exec="),
];

#[cfg(test)]
fn git_has_unsafe_global_option(global_args: &[String]) -> bool {
    global_args
        .iter()
        .map(String::as_str)
        .any(|arg| git_matches_option_pattern(arg, UNSAFE_GIT_GLOBAL_OPTIONS))
}

#[cfg(test)]
fn git_subcommand_args_are_read_only(args: &[String]) -> bool {
    !args
        .iter()
        .map(String::as_str)
        .any(|arg| git_matches_option_pattern(arg, UNSAFE_GIT_SUBCOMMAND_OPTIONS))
}

#[cfg(test)]
fn git_matches_option_pattern(arg: &str, patterns: &[GitOptionPattern]) -> bool {
    patterns.iter().any(|pattern| pattern.matches(arg))
}

#[cfg(test)]
fn is_git_global_option_with_value(arg: &str) -> bool {
    matches!(
        arg,
        "-C" | "-c"
            | "--config-env"
            | "--exec-path"
            | "--git-dir"
            | "--namespace"
            | "--super-prefix"
            | "--work-tree"
    )
}

#[cfg(test)]
fn is_git_global_option_with_inline_value(arg: &str) -> bool {
    matches!(
        arg,
        s if s.starts_with("--config-env=")
            || s.starts_with("--exec-path=")
            || s.starts_with("--git-dir=")
            || s.starts_with("--namespace=")
            || s.starts_with("--super-prefix=")
            || s.starts_with("--work-tree=")
    ) || ((arg.starts_with("-C") || arg.starts_with("-c")) && arg.len() > 2)
}

#[cfg(test)]
fn find_command_is_read_only(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fls" | "-fprint" | "-fprint0"
        ) || arg == "-fprintf"
    })
}

#[cfg(test)]
fn is_valid_sed_n_arg(arg: Option<&str>) -> bool {
    let Some(arg) = arg else {
        return false;
    };
    let Some(core) = arg.strip_suffix('p') else {
        return false;
    };
    let parts = core.split(',').collect::<Vec<_>>();
    match parts.as_slice() {
        [num] => !num.is_empty() && num.chars().all(|ch| ch.is_ascii_digit()),
        [start, end] => {
            !start.is_empty()
                && !end.is_empty()
                && start.chars().all(|ch| ch.is_ascii_digit())
                && end.chars().all(|ch| ch.is_ascii_digit())
        }
        _ => false,
    }
}

fn executable_name_lookup_key(raw: &str) -> Option<String> {
    Path::new(raw)
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
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

    fn assert_codex_chunk_id(model_text: &str) {
        let first_line = model_text.lines().next().expect("chunk id line");
        let chunk_id = first_line
            .strip_prefix("Chunk ID: ")
            .expect("chunk id prefix");
        assert_eq!(chunk_id.len(), 6);
        assert!(chunk_id.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[cfg(unix)]
    fn make_executable_for_test(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod");
    }

    #[cfg(not(unix))]
    fn make_executable_for_test(_path: &Path) {}

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
                namespace: None,
                arguments: json!({"cmd": "printf hello", "yield_time_ms": 5000}),
            },
        )
        .expect("exec");

        assert_eq!(result.content["running"], false);
        assert_eq!(result.content["output"], "hello");
        assert_codex_chunk_id(&result.model_text);
        assert!(result.model_text.contains("Wall time: "));
        assert!(result.model_text.contains("Process exited with code 0"));
        assert!(result.model_text.contains("Original token count: "));
        assert!(result.model_text.ends_with("Output:\nhello"));
        let events = store.events_for_session(&session.id).expect("events");
        assert!(events
            .iter()
            .any(|event| event.event_type == "command.finished"));
        let started = events
            .iter()
            .find(|event| event.event_type == "command.started")
            .expect("command started event");
        assert_eq!(started.payload["login"], true);
        assert!(started.payload["shell"].as_str().is_some());
        assert!(started.payload["shell_type"].as_str().is_some());
        assert!(started.payload["session_id"].as_i64().is_some());
    }

    #[test]
    fn shell_command_returns_codex_legacy_output_without_session() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let result = shell_command_with_budget(
            &store,
            &session,
            &ToolCall {
                id: "call_shell_completed".to_string(),
                name: "shell_command".to_string(),
                namespace: None,
                arguments: json!({"command": "printf legacy", "timeout_ms": 5000}),
            },
            DEFAULT_MAX_OUTPUT_TOKENS,
            true,
        )
        .expect("shell command");

        assert_eq!(result.content["running"], false);
        assert_eq!(result.content["output"], "legacy");
        assert!(result.model_text.starts_with("Exit code: 0\nWall time: "));
        assert!(result.model_text.ends_with("Output:\nlegacy"));
        assert!(!result
            .model_text
            .contains("Process running with session ID"));
        assert!(!result.model_text.contains("Chunk ID:"));

        let events = store.events_for_session(&session.id).expect("events");
        assert!(events.iter().any(|event| {
            event.event_type == "tool.started" && event.payload["name"] == "shell_command"
        }));
        assert!(!events.iter().any(|event| {
            event.event_type == "tool.started" && event.payload["name"] == "exec_command"
        }));
        let started = events
            .iter()
            .find(|event| event.event_type == "command.started")
            .expect("command started event");
        assert_eq!(started.payload["session_id"], Value::Null);
        assert_eq!(started.payload["cmd"], "printf legacy");
        assert_eq!(started.payload["login"], true);
        assert_eq!(started.payload["tty"], false);
        let finished = events
            .iter()
            .find(|event| event.event_type == "command.finished")
            .expect("command finished event");
        assert_eq!(finished.payload["session_id"], Value::Null);
        assert_eq!(finished.payload["exit_code"], 0);
        assert_eq!(finished.payload["timed_out"], false);
        assert_eq!(finished.payload["aggregated_output"], "legacy");
    }

    #[test]
    fn shell_command_timeout_is_hard_and_does_not_keep_session() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let result = shell_command_with_budget(
            &store,
            &session,
            &ToolCall {
                id: "call_shell_timeout".to_string(),
                name: "shell_command".to_string(),
                namespace: None,
                arguments: json!({"command": "sleep 2", "timeout_ms": 50}),
            },
            DEFAULT_MAX_OUTPUT_TOKENS,
            true,
        )
        .expect("shell timeout");

        assert_eq!(result.content["running"], false);
        assert_eq!(
            result.content["metadata"]["exit_code"],
            SHELL_COMMAND_TIMEOUT_EXIT_CODE
        );
        assert_eq!(result.content["metadata"]["timed_out"], true);
        assert!(result.model_text.starts_with(&format!(
            "Exit code: {}\nWall time: ",
            SHELL_COMMAND_TIMEOUT_EXIT_CODE
        )));
        assert!(result.model_text.contains("command timed out after"));
        assert!(!result
            .model_text
            .contains("Process running with session ID"));

        let events = store.events_for_session(&session.id).expect("events");
        assert!(!events
            .iter()
            .any(|event| event.event_type == "command.waiting"));
        let finished = events
            .iter()
            .find(|event| event.event_type == "command.finished")
            .expect("command finished event");
        assert_eq!(finished.payload["session_id"], Value::Null);
        assert_eq!(
            finished.payload["exit_code"],
            SHELL_COMMAND_TIMEOUT_EXIT_CODE
        );
        assert_eq!(finished.payload["timed_out"], true);
    }

    #[test]
    fn shell_command_rejects_login_when_disabled_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let error = shell_command_with_budget(
            &store,
            &session,
            &ToolCall {
                id: "call_shell_login_disabled".to_string(),
                name: "shell_command".to_string(),
                namespace: None,
                arguments: json!({"command": "printf nope", "login": true}),
            },
            DEFAULT_MAX_OUTPUT_TOKENS,
            false,
        )
        .expect_err("disabled login should be rejected");

        assert!(format!("{error:#}").contains("login shell is disabled by config"));
        let events = store.events_for_session(&session.id).expect("events");
        assert!(!events
            .iter()
            .any(|event| event.event_type == "command.started"));
    }

    #[test]
    fn shell_detection_matches_codex_paths() {
        assert_eq!(detect_shell_type(Path::new("zsh")), Some(ShellType::Zsh));
        assert_eq!(detect_shell_type(Path::new("bash")), Some(ShellType::Bash));
        assert_eq!(
            detect_shell_type(Path::new("pwsh")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(
            detect_shell_type(Path::new("powershell.exe")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(
            detect_shell_type(Path::new("/usr/local/bin/pwsh")),
            Some(ShellType::PowerShell)
        );
        assert_eq!(detect_shell_type(Path::new("/bin/sh")), Some(ShellType::Sh));
        assert_eq!(
            detect_shell_type(Path::new("cmd.exe")),
            Some(ShellType::Cmd)
        );
        assert_eq!(detect_shell_type(Path::new("fish")), None);
    }

    #[test]
    fn shell_exec_argv_matches_codex_shell_types() {
        let bash = ShellSpec {
            shell_type: ShellType::Bash,
            shell_path: PathBuf::from("/bin/bash"),
        };
        assert_eq!(
            bash.derive_exec_argv("printf ok", true),
            vec!["/bin/bash", "-lc", "printf ok"]
        );
        assert_eq!(
            bash.derive_exec_argv("printf ok", false),
            vec!["/bin/bash", "-c", "printf ok"]
        );

        let pwsh = ShellSpec {
            shell_type: ShellType::PowerShell,
            shell_path: PathBuf::from("pwsh"),
        };
        assert_eq!(
            pwsh.derive_exec_argv("Write-Output ok", true),
            vec!["pwsh", "-Command", "Write-Output ok"]
        );
        assert_eq!(
            pwsh.derive_exec_argv("Write-Output ok", false),
            vec!["pwsh", "-NoProfile", "-Command", "Write-Output ok"]
        );

        let cmd = ShellSpec {
            shell_type: ShellType::Cmd,
            shell_path: PathBuf::from("cmd"),
        };
        assert_eq!(
            cmd.derive_exec_argv("echo ok", true),
            vec!["cmd", "/c", "echo ok"]
        );
        assert_eq!(
            cmd.derive_exec_argv("echo ok", false),
            vec!["cmd", "/c", "echo ok"]
        );
    }

    #[test]
    fn model_shell_resolution_uses_codex_type_fallbacks() {
        let tmp = TempDir::new().expect("tmp");
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        for name in ["pwsh", "powershell", "cmd", "bash"] {
            let path = bin.join(name);
            std::fs::write(&path, "").expect("fake shell");
            make_executable_for_test(&path);
        }
        let path_dirs = vec![bin.clone()];

        assert_eq!(
            resolve_model_shell_with_path_lookup("pwsh", &path_dirs, None),
            ShellSpec {
                shell_type: ShellType::PowerShell,
                shell_path: bin.join("pwsh"),
            }
        );
        assert_eq!(
            resolve_model_shell_with_path_lookup("powershell", &path_dirs, None),
            ShellSpec {
                shell_type: ShellType::PowerShell,
                shell_path: bin.join("pwsh"),
            }
        );
        assert_eq!(
            resolve_model_shell_with_path_lookup("cmd", &path_dirs, None),
            ShellSpec {
                shell_type: ShellType::Cmd,
                shell_path: bin.join("cmd"),
            }
        );
        assert_eq!(
            resolve_model_shell_with_path_lookup("/definitely/missing/fish", &path_dirs, None),
            ultimate_fallback_shell()
        );
    }

    #[test]
    fn exec_command_applies_codex_unified_exec_environment() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let result = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_env".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "printf '%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s' \"$NO_COLOR\" \"$TERM\" \"$LANG\" \"$LC_CTYPE\" \"$LC_ALL\" \"$COLORTERM\" \"$PAGER\" \"$GIT_PAGER\" \"$GH_PAGER\" \"$CODEX_CI\" \"$CODEX_THREAD_ID\"",
                    "yield_time_ms": 5000,
                }),
            },
        )
        .expect("exec");

        assert_eq!(
            result.content["output"],
            json!(format!(
                "1|dumb|C.UTF-8|C.UTF-8|C.UTF-8||cat|cat|cat|1|{}",
                session.id
            ))
        );
    }

    #[test]
    fn exec_command_unknown_model_shell_falls_back_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let result = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_unknown_shell".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "printf ok",
                    "shell": "not-a-real-shell-for-codex-compat",
                    "yield_time_ms": 5000,
                }),
            },
        )
        .expect("exec");

        assert_eq!(result.content["output"], "ok");
        let events = store.events_for_session(&session.id).expect("events");
        let started = events
            .iter()
            .find(|event| event.event_type == "command.started")
            .expect("command started event");
        assert_eq!(
            started.payload["shell"],
            json!(ultimate_fallback_shell().path_string())
        );
        assert_eq!(
            started.payload["shell_type"],
            json!(ultimate_fallback_shell().name())
        );
    }

    #[test]
    fn running_command_emits_background_finished_event_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let started = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_background_finish".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import time; print('ready', flush=True); time.sleep(1.0); print('done', flush=True)\"",
                    "yield_time_ms": 50,
                }),
            },
        )
        .expect("exec");
        let process_id = started.content["session_id"].as_i64().expect("session id");
        assert_eq!(started.content["running"], true);

        for _ in 0..60 {
            let events = store.events_for_session(&session.id).expect("events");
            if events.iter().any(|event| {
                event.event_type == "command.finished"
                    && event.payload["session_id"] == json!(process_id)
            }) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        let before_poll_events = store.events_for_session(&session.id).expect("events");
        let trailing_output = before_poll_events
            .iter()
            .find(|event| {
                event.event_type == "command.output"
                    && event.payload["session_id"] == json!(process_id)
                    && event
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.contains("done"))
            })
            .expect("trailing background command.output");
        let finished_events_before_poll = before_poll_events
            .iter()
            .filter(|event| {
                event.event_type == "command.finished"
                    && event.payload["session_id"] == json!(process_id)
            })
            .collect::<Vec<_>>();
        assert_eq!(finished_events_before_poll.len(), 1);
        let transcript = finished_events_before_poll[0].payload["aggregated_output"]
            .as_str()
            .expect("aggregated output");
        assert!(
            transcript.contains("ready") && transcript.contains("done"),
            "background finish should retain the whole Codex-style transcript: {transcript:?}"
        );
        assert_eq!(
            finished_events_before_poll[0].payload["stdout"],
            finished_events_before_poll[0].payload["aggregated_output"]
        );
        assert_eq!(finished_events_before_poll[0].payload["stderr"], "");
        assert_eq!(finished_events_before_poll[0].payload["timed_out"], false);
        assert!(trailing_output.seq < finished_events_before_poll[0].seq);

        let polled = write_stdin(
            &store,
            &session,
            &ToolCall {
                id: "call_poll_background_finish".to_string(),
                name: "write_stdin".to_string(),
                namespace: None,
                arguments: json!({"session_id": process_id, "chars": "", "yield_time_ms": 5000}),
            },
        )
        .expect("poll");
        assert_eq!(polled.content["running"], false);
        assert_eq!(polled.content["session_id"], Value::Null);

        let after_poll_events = store.events_for_session(&session.id).expect("events");
        let finished_after_poll = after_poll_events
            .iter()
            .filter(|event| {
                event.event_type == "command.finished"
                    && event.payload["session_id"] == json!(process_id)
            })
            .count();
        assert_eq!(finished_after_poll, 1);
    }

    #[test]
    fn background_finish_waits_for_trailing_output_grace_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let started = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_background_trailing_output".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import subprocess, sys, time; print('ready', flush=True); time.sleep(0.6); print('done', flush=True); subprocess.Popen(['python3','-u','-c','import time; time.sleep(0.05); print(\\\"tail\\\", flush=True)'], stdout=sys.stdout, stderr=sys.stderr)\"",
                    "yield_time_ms": 50,
                }),
            },
        )
        .expect("exec");
        let process_id = started.content["session_id"].as_i64().expect("session id");
        assert_eq!(started.content["running"], true);

        let mut finished = None;
        for _ in 0..40 {
            let events = store.events_for_session(&session.id).expect("events");
            finished = events.into_iter().find(|event| {
                event.event_type == "command.finished"
                    && event.payload["session_id"] == json!(process_id)
            });
            if finished.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        let finished = finished.expect("background command.finished event");
        let transcript = finished.payload["aggregated_output"]
            .as_str()
            .expect("aggregated output");
        assert!(
            transcript.contains("done") && transcript.contains("tail"),
            "background finish should wait for trailing post-exit output: {transcript:?}"
        );
    }

    #[test]
    fn completed_exec_does_not_wait_for_background_descendant_pipe_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let started = Instant::now();
        let result = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_post_exit_drain".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import subprocess, sys; print('done', flush=True); subprocess.Popen(['sleep','2'], stdout=sys.stdout, stderr=sys.stderr)\"",
                    "yield_time_ms": 5000,
                }),
            },
        )
        .expect("exec");

        assert!(
            started.elapsed() < Duration::from_millis(1000),
            "post-exit reader drain should be capped instead of waiting for a background descendant"
        );
        assert_eq!(result.content["running"], false);
        assert!(result.content["output"]
            .as_str()
            .unwrap_or_default()
            .contains("done"));
    }

    #[test]
    fn background_finish_does_not_wait_for_background_descendant_pipe_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let started = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_background_post_exit_drain".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import subprocess, sys, time; print('ready', flush=True); time.sleep(0.6); print('done', flush=True); subprocess.Popen(['sleep','2'], stdout=sys.stdout, stderr=sys.stderr)\"",
                    "yield_time_ms": 50,
                }),
            },
        )
        .expect("exec");
        let process_id = started.content["session_id"].as_i64().expect("session id");
        assert_eq!(started.content["running"], true);

        let wait_started = Instant::now();
        let mut finished = None;
        for _ in 0..30 {
            let events = store.events_for_session(&session.id).expect("events");
            finished = events.into_iter().find(|event| {
                event.event_type == "command.finished"
                    && event.payload["session_id"] == json!(process_id)
            });
            if finished.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        let finished = finished.expect("background command.finished event");
        assert!(
            wait_started.elapsed() < Duration::from_millis(1500),
            "background watcher should cap post-exit reader drain"
        );
        assert!(finished.payload["aggregated_output"]
            .as_str()
            .unwrap_or_default()
            .contains("done"));
    }

    #[test]
    fn resolve_workdir_does_not_rewrite_absolute_paths() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        drop(store);

        let result = resolve_workdir(&session, Some("/opt/runtime")).expect("resolve");

        assert_eq!(result, std::path::PathBuf::from("/opt/runtime"));
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
                namespace: None,
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
    fn exec_command_streams_partial_output_without_newline_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let result = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_partial_prompt".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import sys, time; sys.stdout.write('prompt> '); sys.stdout.flush(); time.sleep(1.0)\"",
                    "yield_time_ms": 250,
                }),
            },
        )
        .expect("exec");

        assert_eq!(result.content["running"], true);
        assert_eq!(result.content["output"], "prompt> ");
        assert!(result.model_text.ends_with("Output:\nprompt> "));
    }

    #[test]
    fn exec_command_pty_streams_partial_output_without_newline_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let result = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_pty_partial_prompt".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import sys, time; sys.stdout.write('pty> '); sys.stdout.flush(); time.sleep(1.0)\"",
                    "yield_time_ms": 250,
                    "tty": true,
                }),
            },
        )
        .expect("exec");

        assert_eq!(result.content["running"], true);
        assert!(result.content["output"]
            .as_str()
            .expect("output")
            .contains("pty> "));
        assert!(result.model_text.contains("Output:\n"));
        assert!(result.model_text.contains("pty> "));
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
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import sys; print('ready', flush=True); [print('echo:' + line.strip(), flush=True) for line in sys.stdin]\"",
                    "tty": true,
                    "yield_time_ms": 100,
                }),
            },
        )
        .expect("exec");
        let process_id = started.content["session_id"]
            .as_i64()
            .expect("session id")
            .to_owned();
        assert!(process_id >= 1000);
        assert_eq!(started.content["running"], true);
        assert!(started
            .model_text
            .contains(&format!("Process running with session ID {process_id}")));
        assert!(started.model_text.contains("Output:\nready"));

        let written = write_stdin(
            &store,
            &session,
            &ToolCall {
                id: "call_write_stdin".to_string(),
                name: "write_stdin".to_string(),
                namespace: None,
                arguments: json!({
                    "session_id": process_id,
                    "chars": "hello\n",
                    "yield_time_ms": 200,
                }),
            },
        )
        .expect("write stdin");

        assert_eq!(written.content["running"], true);
        assert!(written
            .model_text
            .contains(&format!("Process running with session ID {process_id}")));
        assert!(written.content["output"]
            .as_str()
            .expect("output")
            .contains("echo:hello"));
        stop_for_test(process_id);
    }

    #[test]
    fn write_stdin_non_empty_wait_includes_codex_reaction_window() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let started = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_settle".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import sys, time; print('ready', flush=True); sys.stdin.readline(); time.sleep(0.28); print('settled', flush=True); time.sleep(1.0)\"",
                    "tty": true,
                    "yield_time_ms": 100,
                }),
            },
        )
        .expect("exec");
        let process_id = started.content["session_id"].as_i64().expect("session id");
        assert!(started.content["output"]
            .as_str()
            .expect("output")
            .contains("ready"));

        let written = write_stdin(
            &store,
            &session,
            &ToolCall {
                id: "call_write_settle".to_string(),
                name: "write_stdin".to_string(),
                namespace: None,
                arguments: json!({
                    "session_id": process_id,
                    "chars": "go\n",
                    "yield_time_ms": 250,
                }),
            },
        )
        .expect("write stdin");

        assert!(written.content["output"]
            .as_str()
            .expect("output")
            .contains("settled"));
        stop_for_test(process_id);
    }

    #[test]
    fn write_stdin_finished_process_omits_session_id_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let started = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_finishes_before_poll".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import time; print('ready', flush=True); time.sleep(1.0); print('done', flush=True)\"",
                    "yield_time_ms": 50,
                }),
            },
        )
        .expect("exec");
        let process_id = started.content["session_id"].as_i64().expect("session id");
        thread::sleep(Duration::from_millis(1300));

        let polled = write_stdin(
            &store,
            &session,
            &ToolCall {
                id: "call_poll_finished_no_session".to_string(),
                name: "write_stdin".to_string(),
                namespace: None,
                arguments: json!({"session_id": process_id, "chars": "", "yield_time_ms": 5000}),
            },
        )
        .expect("poll");

        assert_eq!(polled.content["running"], false);
        assert_eq!(polled.content["session_id"], Value::Null);
        assert!(!polled
            .model_text
            .contains(&format!("Process running with session ID {process_id}")));
    }

    #[test]
    fn write_stdin_after_process_exit_returns_completion_without_stale_write_error() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let started = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_tty_exits_before_write".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import time; print('ready', flush=True); time.sleep(1.0); print('done', flush=True)\"",
                    "tty": true,
                    "yield_time_ms": 50,
                }),
            },
        )
        .expect("exec");
        let process_id = started.content["session_id"].as_i64().expect("session id");
        thread::sleep(Duration::from_millis(1300));

        let written = write_stdin(
            &store,
            &session,
            &ToolCall {
                id: "call_write_after_exit".to_string(),
                name: "write_stdin".to_string(),
                namespace: None,
                arguments: json!({
                    "session_id": process_id,
                    "chars": "late\n",
                    "yield_time_ms": 5000,
                }),
            },
        )
        .expect("write after exit");

        assert_eq!(written.content["running"], false);
        assert_eq!(written.content["session_id"], Value::Null);
        assert_eq!(written.content["metadata"]["write_error"], Value::Null);
        assert!(!written.model_text.contains("Write error:"));
        let events = store.events_for_session(&session.id).expect("events");
        assert!(!events.iter().any(|event| {
            event.event_type == "command.write_error"
                && event.payload["tool_call_id"] == json!("call_write_after_exit")
        }));
    }

    #[test]
    fn cleanup_session_commands_kills_explicit_session_without_touching_others() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let other_cwd = tmp.path().join("other");
        std::fs::create_dir_all(&other_cwd).expect("other cwd");
        let other = store
            .create_session(None, other_cwd)
            .expect("other session");
        let first = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_cleanup_first".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import time; print('first', flush=True); time.sleep(5)\"",
                    "yield_time_ms": 50,
                }),
            },
        )
        .expect("first");
        let second = exec_command(
            &store,
            &other,
            &ToolCall {
                id: "call_cleanup_second".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import time; print('second', flush=True); time.sleep(5)\"",
                    "yield_time_ms": 50,
                }),
            },
        )
        .expect("second");
        let first_id = first.content["session_id"].as_i64().expect("first id");
        let second_id = second.content["session_id"].as_i64().expect("second id");

        assert_eq!(cleanup_session_commands(&session.id), 1);
        let commands = commands().lock().expect("command registry poisoned");
        assert!(!commands.contains_key(&first_id));
        assert!(commands.contains_key(&second_id));
        drop(commands);
        stop_for_test(second_id);
    }

    #[test]
    fn command_model_text_uses_codex_style_token_truncation() {
        let output =
            "this is an example of a long output that should be truncated\nalso some other line";
        let payload = CommandOutputPayload {
            chunk_id: "chunk_test".to_string(),
            session_id: None,
            running: false,
            output,
            max_chars: 10 * TOKEN_TO_CHAR_APPROX,
            exit_code: Some(0),
            duration: Duration::from_millis(1250),
            tty_requested: false,
            tty_allocated: false,
            write_error: None,
        };

        let text = command_model_text(&payload);

        assert!(text.starts_with("Chunk ID: chunk_test\nWall time: 1.2500 seconds\n"));
        assert!(text.contains("Process exited with code 0\n"));
        assert!(text.contains("Original token count: 21\n"));
        assert!(text.contains("Output:\nTotal output lines: 2\n\n"));
        assert!(text.contains("tokens truncated"));
        assert!(!text.contains("omitted"));
        assert!(!text.contains("chars truncated"));
    }

    #[test]
    fn command_model_text_includes_write_errors_like_codex_recovery_context() {
        let payload = CommandOutputPayload {
            chunk_id: "chunk_write".to_string(),
            session_id: Some(1000),
            running: false,
            output: "",
            max_chars: 1000,
            exit_code: Some(1),
            duration: Duration::from_millis(12),
            tty_requested: true,
            tty_allocated: true,
            write_error: Some("broken pipe"),
        };

        let text = command_model_text(&payload);

        assert!(text.contains("Write error: broken pipe"));
        assert!(text.contains("Process exited with code 1"));
    }

    #[test]
    fn max_output_tokens_is_capped_to_local_policy_like_codex() {
        assert_eq!(
            max_output_chars(&json!({"max_output_tokens": 6}), DEFAULT_MAX_OUTPUT_TOKENS),
            6 * TOKEN_TO_CHAR_APPROX
        );
        assert_eq!(
            max_output_chars(
                &json!({"max_output_tokens": DEFAULT_MAX_OUTPUT_TOKENS + 1}),
                DEFAULT_MAX_OUTPUT_TOKENS,
            ),
            DEFAULT_MAX_OUTPUT_TOKENS * TOKEN_TO_CHAR_APPROX
        );
        assert_eq!(
            max_output_chars(
                &json!({"max_output_tokens": u64::MAX}),
                DEFAULT_MAX_OUTPUT_TOKENS
            ),
            DEFAULT_MAX_OUTPUT_TOKENS * TOKEN_TO_CHAR_APPROX
        );
        assert_eq!(
            max_output_chars(&json!({"max_output_tokens": u64::MAX}), 6),
            6 * TOKEN_TO_CHAR_APPROX
        );
        assert_eq!(
            max_output_chars(&json!({"max_output_tokens": 6}), 200),
            6 * TOKEN_TO_CHAR_APPROX
        );
    }

    #[test]
    fn exec_command_clamps_requested_output_to_active_model_policy_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let result = exec_command_with_budget(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_policy_clamp".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -c \"print('0123456789abcdef' * 20)\"",
                    "yield_time_ms": 5000,
                    "max_output_tokens": 70_000,
                }),
            },
            6,
        )
        .expect("exec");

        assert_eq!(result.content["metadata"]["truncated"], true);
        assert!(result.model_text.contains("Original token count: "));
        assert!(result.model_text.contains("tokens truncated"));
        assert!(!result.model_text.contains("Full tool output saved to:"));
    }

    #[test]
    fn head_tail_buffer_preserves_prefix_and_suffix_like_codex() {
        let mut buffer = HeadTailBuffer::new(10);
        buffer.push_chunk(b"0123456789".to_vec());
        buffer.push_chunk(b"ab".to_vec());

        assert_eq!(buffer.retained_bytes(), 10);
        assert!(buffer.omitted_bytes() > 0);
        let rendered = String::from_utf8_lossy(&buffer.to_bytes()).to_string();
        assert!(rendered.starts_with("01234"), "{rendered:?}");
        assert!(rendered.ends_with("89ab"), "{rendered:?}");

        let drained = String::from_utf8_lossy(&buffer.drain_bytes()).to_string();
        assert_eq!(drained, rendered);
        assert_eq!(buffer.retained_bytes(), 0);
        assert_eq!(buffer.omitted_bytes(), 0);
    }

    #[test]
    fn write_stdin_empty_poll_uses_codex_background_wait_bounds() {
        assert_eq!(
            write_stdin_yield_time(&json!({}), "").as_millis(),
            u128::from(MIN_EMPTY_POLL_YIELD_TIME_MS)
        );
        assert_eq!(
            write_stdin_yield_time(&json!({"yield_time_ms": 10}), "").as_millis(),
            u128::from(MIN_EMPTY_POLL_YIELD_TIME_MS)
        );
        assert_eq!(
            write_stdin_yield_time(&json!({"yield_time_ms": 120_000}), "").as_millis(),
            120_000
        );
        assert_eq!(
            write_stdin_yield_time(&json!({"yield_time_ms": 999_000}), "").as_millis(),
            u128::from(MAX_EMPTY_POLL_YIELD_TIME_MS)
        );
        assert_eq!(
            write_stdin_yield_time(&json!({"yield_time_ms": 999_000}), "input").as_millis(),
            u128::from(MAX_YIELD_TIME_MS)
        );
    }

    #[test]
    fn command_pruning_prefers_old_exited_processes_like_codex() {
        let now = Instant::now();
        let meta = (0..70)
            .map(|idx| {
                (
                    1000 + idx,
                    now + Duration::from_millis(idx as u64),
                    idx == 10 || idx == 20,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(process_id_to_prune_from_meta(&meta), Some(1010));

        let live_meta = meta
            .iter()
            .map(|(process_id, last_used, _)| (*process_id, *last_used, false))
            .collect::<Vec<_>>();
        assert_eq!(process_id_to_prune_from_meta(&live_meta), Some(1000));
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
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import time; print('ready', flush=True); time.sleep(5)\"",
                    "yield_time_ms": 100,
                }),
            },
        )
        .expect("exec");
        let process_id = started.content["session_id"].as_i64().expect("session id");
        let error = write_stdin(
            &store,
            &other,
            &ToolCall {
                id: "call_cross_write".to_string(),
                name: "write_stdin".to_string(),
                namespace: None,
                arguments: json!({"session_id": process_id, "chars": ""}),
            },
        )
        .expect_err("cross session access should fail");
        assert!(error.to_string().contains("another task"));
        assert_eq!(session.status, SessionStatus::Created);
        stop_for_test(process_id);
    }

    #[test]
    fn write_stdin_requires_numeric_session_id_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);

        let error = write_stdin(
            &store,
            &session,
            &ToolCall {
                id: "call_write_string_id".to_string(),
                name: "write_stdin".to_string(),
                namespace: None,
                arguments: json!({"session_id": "1000", "chars": ""}),
            },
        )
        .expect_err("string session ids should not match Codex schema");

        assert!(error.to_string().contains("numeric session_id"));
    }

    #[test]
    fn exec_command_non_tty_stdin_is_closed_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let result = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_closed_stdin_by_default".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -c \"import sys; data = sys.stdin.read(); print('closed' if data == '' else data)\"",
                    "yield_time_ms": 5000,
                }),
            },
        )
        .expect("exec");

        assert_eq!(result.content["running"], false);
        assert_eq!(result.content["output"], "closed\n");
    }

    #[test]
    fn exec_command_large_output_uses_codex_style_head_tail_buffer() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let result = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_large_head_tail".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -c \"import sys; sys.stdout.write('A' * 700000); sys.stdout.write('B' * 700000)\"",
                    "yield_time_ms": 5000,
                    "max_output_tokens": DEFAULT_MAX_OUTPUT_TOKENS,
                }),
            },
        )
        .expect("exec");

        let output = result.content["output"].as_str().expect("output");
        assert!(output.len() <= UNIFIED_EXEC_OUTPUT_MAX_BYTES + 128);
        assert!(
            output.starts_with("AAAA"),
            "missing head: {:?}",
            &output[..20]
        );
        assert!(output.ends_with("BBBB"), "missing tail");
        assert!(
            output.contains("omitted") || result.model_text.contains("tokens truncated"),
            "large output should be marked truncated for local JSON or model text"
        );
    }

    #[test]
    fn write_stdin_rejects_non_tty_input_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let started = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_closed_stdin".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "python3 -u -c \"import time; print('ready', flush=True); time.sleep(5)\"",
                    "yield_time_ms": 50,
                }),
            },
        )
        .expect("exec");
        let process_id = started.content["session_id"]
            .as_i64()
            .expect("session id")
            .to_owned();

        let error = write_stdin(
            &store,
            &session,
            &ToolCall {
                id: "call_write_non_tty".to_string(),
                name: "write_stdin".to_string(),
                namespace: None,
                arguments: json!({
                    "session_id": process_id,
                    "chars": "stop\n",
                    "yield_time_ms": 50,
                }),
            },
        )
        .expect_err("non-tty stdin writes should fail like Codex");

        assert_eq!(error.to_string(), STDIN_CLOSED_MESSAGE);

        assert!(
            commands()
                .lock()
                .expect("command registry poisoned")
                .contains_key(&process_id),
            "rejected write should leave command session available"
        );
        stop_for_test(process_id);
    }

    #[test]
    fn exec_command_rejects_codex_dangerous_rm_before_spawn() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let victim = tmp.path().join("work").join("victim");
        std::fs::create_dir_all(&victim).expect("victim");

        let error = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_rm_rf".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({"cmd": "rm -rf victim", "yield_time_ms": 5000}),
            },
        )
        .expect_err("dangerous rm should not spawn without approval support");

        assert!(error.to_string().contains("destructive command"));
        assert!(victim.exists(), "rejected command must not remove files");
        let events = store.events_for_session(&session.id).expect("events");
        assert!(!events
            .iter()
            .any(|event| event.event_type == "command.started"));
    }

    #[test]
    fn exec_command_rejects_nested_codex_dangerous_rm_before_spawn() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);

        let error = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_nested_rm".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "bash -lc 'printf ok && sudo rm -f victim'",
                    "yield_time_ms": 5000,
                }),
            },
        )
        .expect_err("nested dangerous rm should not spawn without approval support");

        assert!(error.to_string().contains("destructive command"));
        let events = store.events_for_session(&session.id).expect("events");
        assert!(!events
            .iter()
            .any(|event| event.event_type == "command.started"));
    }

    #[test]
    fn exec_command_rejects_sandbox_override_without_approval_flow() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);

        let error = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_escalated".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "printf no-spawn",
                    "sandbox_permissions": "require_escalated",
                    "justification": "Do you want to run this without sandbox restrictions?",
                    "prefix_rule": ["printf"],
                }),
            },
        )
        .expect_err("unsupported escalation should not spawn");

        assert!(error.to_string().contains("approval policy is Never"));
        let events = store.events_for_session(&session.id).expect("events");
        assert!(!events
            .iter()
            .any(|event| event.event_type == "command.started"));
    }

    #[test]
    fn exec_command_rejects_additional_permission_override_without_approval_flow() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);

        let error = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_additional_permissions".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "printf no-spawn",
                    "sandbox_permissions": "with_additional_permissions",
                }),
            },
        )
        .expect_err("unsupported additional permissions should not spawn");

        assert!(error.to_string().contains("approval policy is Never"));
        let events = store.events_for_session(&session.id).expect("events");
        assert!(!events
            .iter()
            .any(|event| event.event_type == "command.started"));
    }

    #[test]
    fn exec_command_treats_additional_permissions_as_sandbox_override() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);

        let error = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_disabled_additional_permissions".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "printf no-spawn",
                    "sandbox_permissions": "use_default",
                    "additional_permissions": {
                        "network": { "enabled": true }
                    },
                }),
            },
        )
        .expect_err("additional_permissions should request a sandbox override");

        assert!(error.to_string().contains("approval policy is Never"));
        let events = store.events_for_session(&session.id).expect("events");
        assert!(!events
            .iter()
            .any(|event| event.event_type == "command.started"));
    }

    #[test]
    fn exec_command_rejects_malformed_additional_permissions_before_override() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);

        let error = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_bad_additional_permissions".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "printf no-spawn",
                    "sandbox_permissions": "with_additional_permissions",
                    "additional_permissions": "network",
                }),
            },
        )
        .expect_err("malformed additional_permissions should fail argument parsing");

        assert!(error
            .to_string()
            .contains("failed to parse function arguments"));
        let events = store.events_for_session(&session.id).expect("events");
        assert!(!events
            .iter()
            .any(|event| event.event_type == "command.started"));
    }

    #[test]
    fn exec_command_accepts_null_additional_permissions_as_absent() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);

        let result = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_null_additional_permissions".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "printf ok",
                    "sandbox_permissions": "use_default",
                    "additional_permissions": null,
                    "yield_time_ms": 5000,
                }),
            },
        )
        .expect("null additional_permissions should deserialize like absence");

        assert_eq!(result.content["running"], false);
        assert_eq!(result.content["output"], "ok");
        let events = store.events_for_session(&session.id).expect("events");
        assert!(events
            .iter()
            .any(|event| event.event_type == "command.started"));
    }

    #[test]
    fn exec_command_rejects_invalid_sandbox_permission_value() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);

        let error = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_bad_sandbox_permissions".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "printf no-spawn",
                    "sandbox_permissions": "danger_full_access",
                }),
            },
        )
        .expect_err("unknown sandbox_permissions should not spawn");

        assert!(error
            .to_string()
            .contains("unsupported sandbox_permissions value"));
        let events = store.events_for_session(&session.id).expect("events");
        assert!(!events
            .iter()
            .any(|event| event.event_type == "command.started"));
    }

    #[test]
    fn exec_command_accepts_default_sandbox_permission_metadata() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);

        let result = exec_command(
            &store,
            &session,
            &ToolCall {
                id: "call_exec_default_sandbox_permissions".to_string(),
                name: "exec_command".to_string(),
                namespace: None,
                arguments: json!({
                    "cmd": "printf ok",
                    "sandbox_permissions": "use_default",
                    "yield_time_ms": 5000,
                }),
            },
        )
        .expect("default sandbox metadata should be accepted");

        assert_eq!(result.content["running"], false);
        assert_eq!(result.content["output"], "ok");
        let events = store.events_for_session(&session.id).expect("events");
        assert!(events
            .iter()
            .any(|event| event.event_type == "command.started"));
        assert!(events.iter().any(|event| {
            event.event_type == "tool.started"
                && event.payload["arguments"]["sandbox_permissions"] == "use_default"
        }));
        assert!(events
            .iter()
            .filter(|event| event.event_type == "command.started")
            .all(|event| event.payload.get("sandbox_permissions").is_none()));
    }

    #[test]
    fn dangerous_detection_matches_codex_rm_subset() {
        assert!(dangerous_command_rejection("rm -rf /tmp/codex").is_some());
        assert!(
            dangerous_command_rejection("/bin/rm -rf /tmp/codex").is_some(),
            "local keeps path-normalized rm rejection as no-approval hardening"
        );
        assert!(dangerous_command_rejection("sudo rm -f /tmp/codex").is_some());
        assert!(dangerous_command_rejection(
            "bash -lc 'cargo install cargo-insta && rm -rf /tmp/codex'"
        )
        .is_some());
        assert!(
            dangerous_command_rejection("rm -fr /tmp/codex").is_none(),
            "Codex's current direct dangerous classifier only matches -f and -rf"
        );
        assert!(
            dangerous_command_rejection("git reset --hard").is_none(),
            "git reset is governed by prompt/approval policy, not Codex's dangerous-command classifier"
        );
    }

    #[test]
    fn exec_read_only_detection_matches_common_inspection_commands() {
        assert!(exec_command_is_known_read_only(
            &json!({"cmd": "rg -n browser src"})
        ));
        assert!(exec_command_is_known_read_only(
            &json!({"cmd": "sed -n '1,20p' Cargo.toml"})
        ));
        assert!(exec_command_is_known_read_only(
            &json!({"cmd": "git status --short"})
        ));
        assert!(exec_command_is_known_read_only(
            &json!({"cmd": "git branch --show-current"})
        ));
        assert!(exec_command_is_known_read_only(
            &json!({"cmd": "ls && pwd"})
        ));
        assert!(exec_command_is_known_read_only(
            &json!({"cmd": "bash -lc 'git status --short && rg -n browser src | wc -l'"})
        ));
        assert!(shell_command_is_known_read_only(
            &json!({"command": "git status --short"})
        ));
        assert!(!exec_command_is_known_read_only(
            &json!({"cmd": "git worktree list --porcelain"})
        ));
        assert!(!exec_command_is_known_read_only(
            &json!({"cmd": "git -ccore.pager=cat status"})
        ));
        assert!(!exec_command_is_known_read_only(
            &json!({"cmd": "cargo test"})
        ));
        assert!(!exec_command_is_known_read_only(
            &json!({"cmd": "git branch -D old"})
        ));
        assert!(!exec_command_is_known_read_only(
            &json!({"cmd": "git worktree add ../new"})
        ));
        assert!(!exec_command_is_known_read_only(
            &json!({"cmd": "sed -n '/pattern/p' file"})
        ));
        assert!(!exec_command_is_known_read_only(
            &json!({"cmd": "sed -i s/a/b/ file"})
        ));
        assert!(!exec_command_is_known_read_only(
            &json!({"cmd": "rg foo > out.txt"})
        ));
        assert!(!exec_command_is_known_read_only(
            &json!({"cmd": "ls && rm -rf /tmp/codex"})
        ));
    }

    #[test]
    fn exec_read_only_detection_matches_codex_unix_safelist_edges() {
        if cfg!(target_os = "linux") {
            assert!(exec_command_is_known_read_only(
                &json!({"cmd": "numfmt 1000"})
            ));
            assert!(exec_command_is_known_read_only(
                &json!({"cmd": "tac Cargo.toml"})
            ));
        }

        for cmd in [
            "base64 -o out.bin",
            "base64 --output=out.bin",
            "base64 -ob64.txt",
            "find . -exec rm {} ;",
            "find . -delete",
            "find . -fprintf out %p",
            "rg --pre cmd pattern",
            "rg --hostname-bin cmd pattern",
            "rg --search-zip pattern",
            "rg -z pattern",
            "git --paginate log -1",
            "git -p log -1",
            "git -C . status",
            "git --git-dir=.git status",
            "git --work-tree=. status",
        ] {
            assert!(
                !exec_command_is_known_read_only(&json!({"cmd": cmd})),
                "{cmd:?} should require normal command policy"
            );
        }

        for cmd in ["git log -p -1", "git diff -p", "git show -p HEAD"] {
            assert!(
                exec_command_is_known_read_only(&json!({"cmd": cmd})),
                "{cmd:?} should remain known read-only like Codex"
            );
        }
    }

    #[test]
    fn shell_wrapper_read_only_detection_rejects_codex_parser_edges() {
        for cmd in [
            "bash -lc 'git status --short && rg -n foo src | wc -l'",
            "zsh -lc 'ls'",
            "bash -lc 'ls\npwd'",
            "bash -lc 'rg -n \"foo\" -g\"*.rs\" src'",
        ] {
            assert!(
                exec_command_is_known_read_only(&json!({"cmd": cmd})),
                "{cmd:?} should be read-only"
            );
        }

        assert_eq!(
            rough_shell_word_commands(r#"echo "/usr"'/'"local"/bin"#),
            Some(vec![vec!["echo".to_string(), "/usr/local/bin".to_string()]])
        );

        for cmd in [
            "bash -lc 'git branch -d feature'",
            "bash -lc 'git --paginate log -1'",
            "bash -lc 'ls # comment'",
            "bash -lc 'ls &&'",
            "bash -lc 'ls ||'",
            "bash -lc '&& ls'",
            "bash -lc 'ls ;; pwd'",
            "bash -lc 'ls | | wc'",
            "bash -lc 'FOO=bar ls'",
            "bash -lc 'echo $(pwd)'",
            "bash -lc 'echo <(pwd)'",
            "bash -lc 'echo $HOME'",
            "bash -lc 'ls > out.txt'",
        ] {
            assert!(
                !exec_command_is_known_read_only(&json!({"cmd": cmd})),
                "{cmd:?} should be rejected by the Codex-style plain-command parser"
            );
        }
    }

    fn stop_for_test(process_id: i64) {
        if let Some(mut command) = commands()
            .lock()
            .expect("command registry poisoned")
            .remove(&process_id)
        {
            let _ = command.process.kill();
            let _ = command.process.wait();
            let _ = finish_readers_after_exit(&mut command);
        }
    }
}
