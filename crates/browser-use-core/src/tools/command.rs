use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child as StdChild, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
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
const DEFAULT_WRITE_STDIN_YIELD_TIME_MS: u64 = 250;
const MIN_YIELD_TIME_MS: u64 = 250;
const MIN_EMPTY_POLL_YIELD_TIME_MS: u64 = 5_000;
const MAX_YIELD_TIME_MS: u64 = 30_000;
const MAX_EMPTY_POLL_YIELD_TIME_MS: u64 = 300_000;
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
const UNIFIED_EXEC_OUTPUT_MAX_BYTES: usize = 1024 * 1024;
const MAX_UNIFIED_EXEC_PROCESSES: usize = 64;
const TOKEN_TO_CHAR_APPROX: usize = 4;
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
        .unwrap_or_else(default_shell);
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
            "shell": shell,
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
        finish_readers(&mut managed);
        let text = managed.read_recent_output();
        let aggregated_output = managed.read_transcript_output();
        emit_command_output(store, &session.id, process_id, &text)?;
        store.append_event(
            &session.id,
            "command.finished",
            json!({
                "tool_call_id": call.id,
                "session_id": process_id,
                "exit_code": status.exit_code,
                "success": status.success,
                "duration_ms": managed.started_at.elapsed().as_millis() as u64,
                "stdout": aggregated_output,
                "stderr": "",
                "aggregated_output": aggregated_output,
                "timed_out": false,
            }),
        )?;
        let payload = CommandOutputPayload {
            chunk_id: chunk_id_for_call(&call.id),
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
    emit_command_output(store, &session.id, process_id, &text)?;
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
        chunk_id: chunk_id_for_call(&call.id),
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
    let write_error = if !chars.is_empty() {
        match command.process.write_all(chars.as_bytes()) {
            Ok(()) => None,
            Err(error) => {
                let message = format!("{error:#}");
                store.append_event(
                    &session.id,
                    "command.write_error",
                    json!({
                        "tool_call_id": call.id,
                        "session_id": process_id,
                        "error": message,
                    }),
                )?;
                Some(message)
            }
        }
    } else {
        None
    };
    wait_for_output(yield_time, || command.process.try_wait())?;
    let status = command.process.try_wait()?;
    if status.is_some() {
        finish_readers(&mut command);
    }
    let text = command.read_recent_output();
    emit_command_output(store, &session.id, process_id, &text)?;

    let running = status.is_none();
    let tty_allocated = command.process.tty_allocated();
    let payload = CommandOutputPayload {
        chunk_id: chunk_id_for_call(&call.id),
        session_id: Some(process_id),
        running,
        output: &text,
        max_chars,
        exit_code: status.as_ref().and_then(|status| status.exit_code),
        duration: command.started_at.elapsed(),
        tty_requested: tty_allocated,
        tty_allocated,
        write_error: write_error.as_deref(),
    };
    let content = command_output(&payload);
    let model_text = command_model_text(&payload);
    if let Some(status) = status {
        if !command.background_finished {
            let aggregated_output = command.read_transcript_output();
            store.append_event(
                &session.id,
                "command.finished",
                json!({
                    "tool_call_id": command.tool_call_id,
                    "session_id": process_id,
                    "exit_code": status.exit_code,
                    "success": status.success,
                    "duration_ms": command.started_at.elapsed().as_millis() as u64,
                    "stdout": aggregated_output,
                    "stderr": "",
                    "aggregated_output": aggregated_output,
                    "timed_out": false,
                }),
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
        finish_readers(&mut command);
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
        let event = {
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
            finish_readers(command);
            let aggregated_output = command.read_transcript_output();
            command.background_finished = true;
            Some((
                command.session_id.clone(),
                command.tool_call_id.clone(),
                status,
                command.started_at.elapsed(),
                aggregated_output,
            ))
        };
        if let Some((session_id, tool_call_id, status, duration, aggregated_output)) = event {
            if let Ok(store) = Store::open_with_optional_notifier(&state_dir, notifier.clone()) {
                let _ = store.append_event(
                    &session_id,
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
    let mut pending = Vec::new();
    {
        let mut commands = commands().lock().expect("command registry poisoned");
        let process_ids = commands
            .iter()
            .filter_map(|(process_id, command)| {
                (command.session_id == session_id).then_some(*process_id)
            })
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
        finish_readers(&mut command);
    }
    count
}

pub(crate) fn exec_command_is_known_read_only(arguments: &Value) -> bool {
    let cmd = arguments
        .get("cmd")
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
    shell: &str,
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
    shell: &str,
    login: bool,
    cmd: &str,
    workdir: &Path,
    output: Arc<Mutex<HeadTailBuffer>>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    thread_id: &str,
) -> Result<(ManagedProcess, Vec<JoinHandle<()>>, bool)> {
    let mut command = Command::new(shell);
    command
        .args(shell_args(shell, login, cmd))
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_unified_exec_env_to_command(&mut command, thread_id);
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn command via shell {} in {}", shell, workdir.display()))?;
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
    shell: &str,
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
    let mut command = CommandBuilder::new(shell);
    command.args(shell_args(shell, login, cmd));
    command.cwd(workdir.as_os_str());
    apply_unified_exec_env_to_pty_command(&mut command, thread_id);
    let child = pair.slave.spawn_command(command).with_context(|| {
        format!(
            "spawn pty command via shell {} in {}",
            shell,
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

fn finish_readers(command: &mut ManagedCommand) {
    for reader in command.readers.drain(..) {
        let _ = reader.join();
    }
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

fn emit_command_output(store: &Store, session_id: &str, process_id: i64, text: &str) -> Result<()> {
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

fn chunk_id_for_call(call_id: &str) -> String {
    format!("chunk_{}", call_id.replace('-', "_"))
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

fn resolve_model_shell(shell: &str) -> String {
    let path = Path::new(shell.trim());
    if path.is_file() {
        return path.to_string_lossy().to_string();
    }
    match shell_name(path).as_deref() {
        Some("bash") => first_available_shell("bash", &["/bin/bash"]),
        Some("zsh") => first_available_shell("zsh", &["/bin/zsh"]),
        Some("sh") => first_available_shell("sh", &["/bin/sh"]),
        _ => ultimate_fallback_shell(),
    }
}

fn first_available_shell(binary_name: &str, fallback_paths: &[&str]) -> String {
    if command_exists_on_path(binary_name) {
        return binary_name.to_string();
    }
    fallback_paths
        .iter()
        .find(|path| Path::new(path).is_file())
        .copied()
        .unwrap_or_else(|| fallback_paths.first().copied().unwrap_or("/bin/sh"))
        .to_string()
}

fn command_exists_on_path(binary_name: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(binary_name).is_file())
}

fn ultimate_fallback_shell() -> String {
    if cfg!(windows) {
        "cmd.exe".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

fn shell_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let name = name.strip_suffix(".exe").unwrap_or(name);
    Some(name.to_ascii_lowercase())
}

fn shell_args(shell: &str, login: bool, cmd: &str) -> Vec<String> {
    let name = shell_name(Path::new(shell)).unwrap_or_else(|| shell.to_ascii_lowercase());
    if login && matches!(name.as_str(), "bash" | "zsh" | "sh") {
        vec!["-lc".to_string(), cmd.to_string()]
    } else {
        vec!["-c".to_string(), cmd.to_string()]
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

fn base64_command_is_read_only(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        matches!(arg.as_str(), "-o" | "--output")
            || arg.starts_with("--output=")
            || (arg.starts_with("-o") && arg != "-o")
    })
}

fn rg_command_is_read_only(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--pre" | "--hostname-bin" | "--search-zip" | "-z"
        ) || arg.starts_with("--pre=")
            || arg.starts_with("--hostname-bin=")
    })
}

fn sed_command_is_read_only(words: &[String]) -> bool {
    words.len() <= 4
        && words.get(1).map(String::as_str) == Some("-n")
        && is_valid_sed_n_arg(words.get(2).map(String::as_str))
}

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
enum GitOptionPattern {
    Exact(&'static str),
    ShortWithInlineValue(&'static str),
    Prefix(&'static str),
}

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

const UNSAFE_GIT_SUBCOMMAND_OPTIONS: &[GitOptionPattern] = &[
    GitOptionPattern::Exact("--output"),
    GitOptionPattern::Prefix("--output="),
    GitOptionPattern::Exact("--ext-diff"),
    GitOptionPattern::Exact("--textconv"),
    GitOptionPattern::Exact("--exec"),
    GitOptionPattern::Prefix("--exec="),
];

fn git_has_unsafe_global_option(global_args: &[String]) -> bool {
    global_args
        .iter()
        .map(String::as_str)
        .any(|arg| git_matches_option_pattern(arg, UNSAFE_GIT_GLOBAL_OPTIONS))
}

fn git_subcommand_args_are_read_only(args: &[String]) -> bool {
    !args
        .iter()
        .map(String::as_str)
        .any(|arg| git_matches_option_pattern(arg, UNSAFE_GIT_SUBCOMMAND_OPTIONS))
}

fn git_matches_option_pattern(arg: &str, patterns: &[GitOptionPattern]) -> bool {
    patterns.iter().any(|pattern| pattern.matches(arg))
}

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

fn find_command_is_read_only(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fls" | "-fprint" | "-fprint0"
        ) || arg == "-fprintf"
    })
}

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
        assert!(result
            .model_text
            .contains("Chunk ID: chunk_call_exec_completed"));
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
        assert!(started.payload["session_id"].as_i64().is_some());
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
        assert_eq!(started.payload["shell"], json!(ultimate_fallback_shell()));
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
        assert!(polled.content["output"]
            .as_str()
            .expect("output")
            .contains("done"));

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
            finish_readers(&mut command);
        }
    }
}
