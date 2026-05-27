use anyhow::{anyhow, bail, Context, Result};
use browser_use_protocol::ToolSpec;
use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_MCP_STARTUP_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_MCP_TOOL_TIMEOUT_MS: u64 = 60_000;
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_MCP_STDERR_BUFFER_CHARS: usize = 8_000;

type SharedMcpConnection = Arc<Mutex<Option<PersistentMcpConnection>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct McpServerConfig {
    pub(crate) server_name: String,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) required: bool,
    pub(crate) supports_parallel_tool_calls: bool,
    pub(crate) startup_timeout_ms: u64,
    pub(crate) tool_timeout_ms: u64,
    pub(crate) enabled_tools: Option<BTreeSet<String>>,
    pub(crate) disabled_tools: BTreeSet<String>,
}

impl McpServerConfig {
    fn allows_tool(&self, name: &str) -> bool {
        if let Some(enabled_tools) = &self.enabled_tools {
            if !enabled_tools.contains(name) {
                return false;
            }
        }
        !self.disabled_tools.contains(name)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct McpToolDefinition {
    pub(crate) server: McpServerConfig,
    pub(crate) raw_tool_name: String,
    pub(crate) callable_namespace: String,
    pub(crate) callable_name: String,
    pub(crate) namespace_description: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
    pub(crate) output_schema: Option<Value>,
    pub(crate) read_only_hint: bool,
}

impl McpToolDefinition {
    pub(crate) fn supports_parallel_tool_calls(&self) -> bool {
        self.server.supports_parallel_tool_calls || self.read_only_hint
    }

    pub(crate) fn namespaced_tool_spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.callable_name.clone(),
            namespace: Some(self.callable_namespace.clone()),
            namespace_description: Some(self.namespace_description.clone()),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            output_schema: self.output_schema.clone(),
            freeform: None,
        }
    }

    pub(crate) fn flat_tool_spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.flat_tool_name(),
            namespace: None,
            namespace_description: None,
            description: format!(
                "{}\n\nMCP server: {}. Raw MCP tool: {}.",
                self.description, self.server.server_name, self.raw_tool_name
            ),
            input_schema: self.input_schema.clone(),
            output_schema: self.output_schema.clone(),
            freeform: None,
        }
    }

    pub(crate) fn flat_tool_name(&self) -> String {
        format!("{}{}", self.callable_namespace, self.callable_name)
    }

    pub(crate) fn matches_call(&self, namespace: Option<&str>, name: &str) -> bool {
        match namespace {
            Some(namespace) => namespace == self.callable_namespace && name == self.callable_name,
            None => name == self.flat_tool_name(),
        }
    }

    pub(crate) fn search_text(&self) -> String {
        let mut schema_properties = self
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|properties| properties.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        schema_properties.sort();
        let mut parts = vec![
            self.flat_tool_name(),
            self.callable_name.clone(),
            self.raw_tool_name.clone(),
            self.server.server_name.clone(),
            self.namespace_description.clone(),
            self.description.clone(),
        ];
        parts.extend(schema_properties);
        parts.join(" ")
    }
}

pub(crate) fn apply_mcp_servers_config_layer(
    servers: &mut BTreeMap<String, McpServerConfig>,
    value: &toml::Value,
    path: &Path,
    relative_base: &Path,
) -> Result<()> {
    let Some(raw_servers) = value.get("mcp_servers") else {
        return Ok(());
    };
    let Some(server_table) = raw_servers.as_table() else {
        bail!(
            "Invalid Browser Use Terminal config `mcp_servers` from `{}`: expected a table.",
            path.display()
        );
    };
    for (server_name, server_value) in server_table {
        let Some(server) = parse_mcp_server_config(server_name, server_value, path, relative_base)?
        else {
            servers.remove(server_name);
            continue;
        };
        servers.insert(server_name.clone(), server);
    }
    Ok(())
}

fn parse_mcp_server_config(
    server_name: &str,
    value: &toml::Value,
    path: &Path,
    relative_base: &Path,
) -> Result<Option<McpServerConfig>> {
    let Some(table) = value.as_table() else {
        bail!(
            "Invalid Browser Use Terminal config `mcp_servers.{server_name}` from `{}`: expected a table.",
            path.display()
        );
    };
    if table
        .get("enabled")
        .and_then(toml::Value::as_bool)
        .is_some_and(|enabled| !enabled)
    {
        return Ok(None);
    }
    let Some(command) = table.get("command").and_then(toml::Value::as_str) else {
        return Ok(None);
    };
    let command = command.trim();
    if command.is_empty() {
        return Ok(None);
    }
    let required = table
        .get("required")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    let supports_parallel_tool_calls = table
        .get("supports_parallel_tool_calls")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    let args = table
        .get("args")
        .map(|value| toml_string_array(value, path, &format!("mcp_servers.{server_name}.args")))
        .transpose()?
        .unwrap_or_default();
    let mut env = table
        .get("env_vars")
        .map(|value| toml_env_vars(value, path, &format!("mcp_servers.{server_name}.env_vars")))
        .transpose()?
        .unwrap_or_default();
    env.extend(
        table
            .get("env")
            .map(|value| toml_string_map(value, path, &format!("mcp_servers.{server_name}.env")))
            .transpose()?
            .unwrap_or_default(),
    );
    let cwd = table
        .get("cwd")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(|cwd| absolutize_config_path(relative_base, cwd));
    let startup_timeout_ms = timeout_ms_from_table(
        table,
        "startup_timeout_ms",
        "startup_timeout_sec",
        DEFAULT_MCP_STARTUP_TIMEOUT_MS,
        path,
        &format!("mcp_servers.{server_name}"),
    )?;
    let tool_timeout_ms = timeout_ms_from_table(
        table,
        "tool_timeout_ms",
        "tool_timeout_sec",
        DEFAULT_MCP_TOOL_TIMEOUT_MS,
        path,
        &format!("mcp_servers.{server_name}"),
    )?;
    let enabled_tools = table
        .get("enabled_tools")
        .map(|value| {
            toml_string_array(
                value,
                path,
                &format!("mcp_servers.{server_name}.enabled_tools"),
            )
        })
        .transpose()?
        .map(|tools| tools.into_iter().collect::<BTreeSet<_>>());
    let mut disabled_tools = table
        .get("disabled_tools")
        .map(|value| {
            toml_string_array(
                value,
                path,
                &format!("mcp_servers.{server_name}.disabled_tools"),
            )
        })
        .transpose()?
        .unwrap_or_default()
        .into_iter()
        .collect::<BTreeSet<_>>();
    if let Some(tools) = table.get("tools").and_then(toml::Value::as_table) {
        for (tool_name, tool_value) in tools {
            let Some(tool_table) = tool_value.as_table() else {
                bail!(
                    "Invalid Browser Use Terminal config `mcp_servers.{server_name}.tools.{tool_name}` from `{}`: expected a table.",
                    path.display()
                );
            };
            if tool_table
                .get("enabled")
                .and_then(toml::Value::as_bool)
                .is_some_and(|enabled| !enabled)
            {
                disabled_tools.insert(tool_name.clone());
            }
        }
    }
    Ok(Some(McpServerConfig {
        server_name: server_name.to_string(),
        command: command.to_string(),
        args,
        env,
        cwd,
        required,
        supports_parallel_tool_calls,
        startup_timeout_ms,
        tool_timeout_ms,
        enabled_tools,
        disabled_tools,
    }))
}

pub(crate) fn discover_tool_definitions_for_session(
    session_id: &str,
    servers: &BTreeMap<String, McpServerConfig>,
) -> Result<Vec<McpToolDefinition>> {
    discover_tool_definitions_for_scope(Some(session_id), servers)
}

fn discover_tool_definitions_for_scope(
    scope: Option<&str>,
    servers: &BTreeMap<String, McpServerConfig>,
) -> Result<Vec<McpToolDefinition>> {
    let mut definitions = Vec::new();
    let mut seen = BTreeSet::<(String, String)>::new();
    for server in servers.values() {
        let tools = match list_server_tools_for_scope(scope, server) {
            Ok(tools) => tools,
            Err(error) if server.required => {
                return Err(error).with_context(|| {
                    format!(
                        "required MCP server `{}` failed to initialize",
                        server.server_name
                    )
                });
            }
            Err(_) => continue,
        };
        for raw_tool in tools {
            let Some(raw_name) = raw_tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            if !server.allows_tool(raw_name) {
                continue;
            }
            let callable_namespace = format!(
                "mcp__{}__",
                sanitize_responses_api_identifier(&server.server_name, "server")
            );
            let callable_name = unique_callable_name(&callable_namespace, raw_name, &mut seen);
            let description = raw_tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            let input_schema = normalize_mcp_input_schema(
                raw_tool
                    .get("inputSchema")
                    .or_else(|| raw_tool.get("input_schema"))
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            );
            let output_schema = Some(mcp_call_tool_result_output_schema(
                raw_tool
                    .get("outputSchema")
                    .or_else(|| raw_tool.get("output_schema"))
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            ));
            let read_only_hint = mcp_tool_read_only_hint(&raw_tool);
            definitions.push(McpToolDefinition {
                server: server.clone(),
                raw_tool_name: raw_name.to_string(),
                callable_namespace,
                callable_name,
                namespace_description: format!("Tools from the {} MCP server.", server.server_name),
                description,
                input_schema,
                output_schema,
                read_only_hint,
            });
        }
    }
    Ok(definitions)
}

pub(crate) fn call_tool_for_session(
    session_id: &str,
    definition: &McpToolDefinition,
    arguments: &Value,
) -> Result<Value> {
    call_tool_for_scope(Some(session_id), definition, arguments)
}

fn call_tool_for_scope(
    scope: Option<&str>,
    definition: &McpToolDefinition,
    arguments: &Value,
) -> Result<Value> {
    run_mcp_operation(
        scope,
        &definition.server,
        McpOperation::CallTool {
            name: definition.raw_tool_name.clone(),
            arguments: arguments.clone(),
        },
        definition.server.tool_timeout_ms,
    )
}

pub(crate) fn list_resources_for_session(
    session_id: &str,
    server: &McpServerConfig,
    cursor: Option<&str>,
) -> Result<Value> {
    list_resource_page(
        Some(session_id),
        server,
        McpResourceListKind::Resources,
        cursor,
    )
}

pub(crate) fn list_resource_templates_for_session(
    session_id: &str,
    server: &McpServerConfig,
    cursor: Option<&str>,
) -> Result<Value> {
    list_resource_page(
        Some(session_id),
        server,
        McpResourceListKind::Templates,
        cursor,
    )
}

pub(crate) fn read_resource_for_session(
    session_id: &str,
    server: &McpServerConfig,
    uri: &str,
) -> Result<Value> {
    read_resource_for_scope(Some(session_id), server, uri)
}

fn read_resource_for_scope(
    scope: Option<&str>,
    server: &McpServerConfig,
    uri: &str,
) -> Result<Value> {
    run_mcp_operation(
        scope,
        server,
        McpOperation::ReadResource {
            uri: uri.to_string(),
        },
        server.tool_timeout_ms,
    )
}

pub(crate) fn list_all_resources_for_session(
    session_id: &str,
    servers: &BTreeMap<String, McpServerConfig>,
) -> BTreeMap<String, Vec<Value>> {
    list_all_resource_items(Some(session_id), servers, McpResourceListKind::Resources)
}

pub(crate) fn list_all_resource_templates_for_session(
    session_id: &str,
    servers: &BTreeMap<String, McpServerConfig>,
) -> BTreeMap<String, Vec<Value>> {
    list_all_resource_items(Some(session_id), servers, McpResourceListKind::Templates)
}

fn list_server_tools_for_scope(
    scope: Option<&str>,
    server: &McpServerConfig,
) -> Result<Vec<Value>> {
    let result = run_mcp_operation(
        scope,
        server,
        McpOperation::ListTools,
        server.startup_timeout_ms,
    )?;
    Ok(result
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

#[derive(Clone, Copy, Debug)]
enum McpResourceListKind {
    Resources,
    Templates,
}

impl McpResourceListKind {
    fn method(self) -> &'static str {
        match self {
            Self::Resources => "resources/list",
            Self::Templates => "resources/templates/list",
        }
    }

    fn result_key(self) -> &'static str {
        match self {
            Self::Resources => "resources",
            Self::Templates => "resourceTemplates",
        }
    }

    fn fallback_result_key(self) -> &'static str {
        match self {
            Self::Resources => "resources",
            Self::Templates => "resource_templates",
        }
    }
}

fn list_resource_page(
    scope: Option<&str>,
    server: &McpServerConfig,
    kind: McpResourceListKind,
    cursor: Option<&str>,
) -> Result<Value> {
    run_mcp_operation(
        scope,
        server,
        McpOperation::ListResources {
            kind,
            cursor: cursor.map(str::to_string),
        },
        server.tool_timeout_ms,
    )
}

fn list_all_resource_items(
    scope: Option<&str>,
    servers: &BTreeMap<String, McpServerConfig>,
    kind: McpResourceListKind,
) -> BTreeMap<String, Vec<Value>> {
    let mut collected_by_server = BTreeMap::new();
    for server in servers.values() {
        let Ok(collected) = list_all_resource_items_for_server(scope, server, kind) else {
            continue;
        };
        collected_by_server.insert(server.server_name.clone(), collected);
    }
    collected_by_server
}

fn list_all_resource_items_for_server(
    scope: Option<&str>,
    server: &McpServerConfig,
    kind: McpResourceListKind,
) -> Result<Vec<Value>> {
    let mut collected = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = BTreeSet::new();
    loop {
        let result = list_resource_page(scope, server, kind, cursor.as_deref())?;
        collected.extend(mcp_resource_items(&result, kind));
        let Some(next_cursor) = mcp_next_cursor(&result) else {
            return Ok(collected);
        };
        if !seen_cursors.insert(next_cursor.clone()) {
            bail!("{} returned duplicate cursor", kind.method());
        }
        cursor = Some(next_cursor);
    }
}

fn mcp_resource_items(result: &Value, kind: McpResourceListKind) -> Vec<Value> {
    result
        .get(kind.result_key())
        .or_else(|| result.get(kind.fallback_result_key()))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn mcp_next_cursor(result: &Value) -> Option<String> {
    result
        .get("nextCursor")
        .or_else(|| result.get("next_cursor"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|cursor| !cursor.is_empty())
        .map(str::to_string)
}

#[derive(Clone, Debug)]
enum McpOperation {
    ListTools,
    CallTool {
        name: String,
        arguments: Value,
    },
    ListResources {
        kind: McpResourceListKind,
        cursor: Option<String>,
    },
    ReadResource {
        uri: String,
    },
}

impl McpOperation {
    fn into_json_rpc_request(self, id: i64) -> Value {
        match self {
            Self::ListTools => json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/list",
                "params": {},
            }),
            Self::CallTool { name, arguments } => json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": name,
                    "arguments": arguments,
                },
            }),
            Self::ListResources { kind, cursor } => {
                let params = cursor
                    .map(|cursor| json!({ "cursor": cursor }))
                    .unwrap_or_else(|| json!({}));
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": kind.method(),
                    "params": params,
                })
            }
            Self::ReadResource { uri } => json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "resources/read",
                "params": {
                    "uri": uri,
                },
            }),
        }
    }
}

fn run_mcp_operation(
    scope: Option<&str>,
    server: &McpServerConfig,
    operation: McpOperation,
    timeout_ms: u64,
) -> Result<Value> {
    let server = server.clone();
    let server_name = server.server_name.clone();
    let connection_key = persistent_mcp_connection_key(scope, &server);
    let connection = persistent_mcp_connection_slot(&connection_key);
    let canceled = Arc::new(AtomicBool::new(false));
    let (pid_tx, pid_rx) = mpsc::channel::<u32>();
    let (result_tx, result_rx) = mpsc::channel::<Result<Value>>();
    let worker_canceled = Arc::clone(&canceled);
    let handle = thread::spawn(move || {
        let result = run_persistent_mcp_operation_inner(
            &server,
            operation,
            connection,
            pid_tx,
            worker_canceled,
        );
        let _ = result_tx.send(result);
    });
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let started = Instant::now();
    let mut child_pid = None;
    loop {
        if child_pid.is_none() {
            if let Ok(pid) = pid_rx.try_recv() {
                child_pid = Some(pid);
            }
        }
        match result_rx.try_recv() {
            Ok(result) => {
                handle
                    .join()
                    .map_err(|_| anyhow!("MCP worker thread panicked"))?;
                return result;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                handle
                    .join()
                    .map_err(|_| anyhow!("MCP worker thread panicked"))?;
                bail!("MCP worker exited without returning a result");
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if started.elapsed() >= timeout {
            canceled.store(true, Ordering::SeqCst);
            if let Some(pid) = child_pid {
                terminate_process_id(pid);
            }
            remove_persistent_mcp_connection(&connection_key);
            bail!(
                "MCP server `{}` timed out after {} ms",
                server_name,
                timeout.as_millis()
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn run_persistent_mcp_operation_inner(
    server: &McpServerConfig,
    operation: McpOperation,
    connection: SharedMcpConnection,
    pid_tx: mpsc::Sender<u32>,
    canceled: Arc<AtomicBool>,
) -> Result<Value> {
    if canceled.load(Ordering::SeqCst) {
        bail!("MCP operation cancelled before start");
    }
    let mut guard = connection
        .lock()
        .map_err(|_| anyhow!("MCP connection cache lock poisoned"))?;
    if canceled.load(Ordering::SeqCst) {
        bail!("MCP operation cancelled before connection acquisition");
    }
    if guard.is_none() {
        *guard = Some(PersistentMcpConnection::start(server, &pid_tx)?);
    } else if let Some(connection) = guard.as_ref() {
        let _ = pid_tx.send(connection.pid());
    }
    if canceled.load(Ordering::SeqCst) {
        bail!("MCP operation cancelled before invocation");
    }
    let connection = guard
        .as_mut()
        .ok_or_else(|| anyhow!("MCP connection cache was empty after initialization"))?;
    let result = connection.invoke(operation);
    let reset_connection = result
        .as_ref()
        .err()
        .is_some_and(mcp_operation_error_resets_connection);
    if reset_connection {
        connection.shutdown();
    }
    let stderr_text = connection.stderr_snapshot();
    if reset_connection {
        *guard = None;
    }
    with_mcp_stderr_context(result, &server.server_name, &stderr_text)
}

struct PersistentMcpConnection {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_text: Arc<Mutex<String>>,
    stderr_handle: Option<thread::JoinHandle<()>>,
    next_request_id: i64,
}

impl PersistentMcpConnection {
    fn start(server: &McpServerConfig, pid_tx: &mpsc::Sender<u32>) -> Result<Self> {
        let mut command = ProcessCommand::new(&server.command);
        command.args(&server.args);
        if let Some(cwd) = &server.cwd {
            command.current_dir(cwd);
        }
        command.envs(&server.env);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start MCP server `{}`", server.server_name))?;
        let _ = pid_tx.send(child.id());
        let stdin = child
            .stdin
            .take()
            .context("MCP child stdin was not captured")?;
        let stdout = child
            .stdout
            .take()
            .context("MCP child stdout was not captured")?;
        let stderr = child
            .stderr
            .take()
            .context("MCP child stderr was not captured")?;
        let stderr_text = Arc::new(Mutex::new(String::new()));
        let stderr_handle = spawn_mcp_stderr_reader(stderr, Arc::clone(&stderr_text));
        let mut connection = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_text,
            stderr_handle: Some(stderr_handle),
            next_request_id: 1,
        };
        if let Err(error) = connection.initialize() {
            connection.shutdown();
            let stderr_text = connection.stderr_snapshot();
            return with_mcp_stderr_context(Err(error), &server.server_name, &stderr_text);
        }
        Ok(connection)
    }

    fn initialize(&mut self) -> Result<()> {
        let initialize_id = self.next_request_id();
        write_json_rpc(
            &mut self.stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": initialize_id,
                "method": "initialize",
                "params": {
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "browser-use-terminal",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                },
            }),
        )?;
        let _ = read_json_rpc_response(&mut self.stdout, initialize_id)?;
        write_json_rpc(
            &mut self.stdin,
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {},
            }),
        )?;
        Ok(())
    }

    fn invoke(&mut self, operation: McpOperation) -> Result<Value> {
        let id = self.next_request_id();
        write_json_rpc(&mut self.stdin, &operation.into_json_rpc_request(id))?;
        read_json_rpc_response(&mut self.stdout, id)
    }

    fn next_request_id(&mut self) -> i64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        id
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn stderr_snapshot(&self) -> String {
        self.stderr_text
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(stderr_handle) = self.stderr_handle.take() {
            let _ = stderr_handle.join();
        }
    }
}

impl Drop for PersistentMcpConnection {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn persistent_mcp_connection_slot(key: &str) -> SharedMcpConnection {
    let mut connections = persistent_mcp_connections()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    connections
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(None)))
        .clone()
}

fn remove_persistent_mcp_connection(key: &str) {
    let mut connections = persistent_mcp_connections()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    connections.remove(key);
}

pub(crate) fn cleanup_mcp_connections_for_session(session_id: &str) -> usize {
    cleanup_persistent_mcp_connections_for_scope(Some(session_id))
}

pub(crate) fn cleanup_all_mcp_connections() -> usize {
    let connections = {
        let mut connections = persistent_mcp_connections()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *connections)
    };
    cleanup_persistent_mcp_connection_slots(connections.into_values())
}

fn cleanup_persistent_mcp_connections_for_scope(scope: Option<&str>) -> usize {
    let prefix = persistent_mcp_scope_key_prefix(scope);
    let slots = {
        let mut connections = persistent_mcp_connections()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let keys = connections
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| connections.remove(&key))
            .collect::<Vec<_>>()
    };
    cleanup_persistent_mcp_connection_slots(slots)
}

fn cleanup_persistent_mcp_connection_slots(
    slots: impl IntoIterator<Item = SharedMcpConnection>,
) -> usize {
    let mut cleaned = 0;
    for slot in slots {
        let Ok(mut connection) = slot.lock() else {
            continue;
        };
        if let Some(mut connection) = connection.take() {
            connection.shutdown();
            cleaned += 1;
        }
    }
    cleaned
}

fn persistent_mcp_connections() -> &'static Mutex<BTreeMap<String, SharedMcpConnection>> {
    static CONNECTIONS: OnceLock<Mutex<BTreeMap<String, SharedMcpConnection>>> = OnceLock::new();
    CONNECTIONS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn persistent_mcp_scope_key_prefix(scope: Option<&str>) -> String {
    let mut hasher = Sha1::new();
    hasher.update(scope.unwrap_or("<global>").as_bytes());
    format!("scope:{:x}:", hasher.finalize())
}

fn persistent_mcp_connection_key(scope: Option<&str>, server: &McpServerConfig) -> String {
    let mut hasher = Sha1::new();
    hasher.update(server.server_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(server.command.as_bytes());
    hasher.update(b"\0");
    for arg in &server.args {
        hasher.update(arg.as_bytes());
        hasher.update(b"\0");
    }
    for (key, value) in &server.env {
        hasher.update(key.as_bytes());
        hasher.update(b"=");
        hasher.update(value.as_bytes());
        hasher.update(b"\0");
    }
    if let Some(cwd) = &server.cwd {
        hasher.update(cwd.display().to_string().as_bytes());
    }
    format!(
        "{}{}:{:x}",
        persistent_mcp_scope_key_prefix(scope),
        server.server_name,
        hasher.finalize()
    )
}

fn spawn_mcp_stderr_reader(
    stderr: ChildStderr,
    buffer: Arc<Mutex<String>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut chunk = [0u8; 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => append_mcp_stderr(&buffer, &String::from_utf8_lossy(&chunk[..count])),
                Err(_) => break,
            }
        }
    })
}

fn append_mcp_stderr(buffer: &Arc<Mutex<String>>, text: &str) {
    let Ok(mut buffer) = buffer.lock() else {
        return;
    };
    buffer.push_str(text);
    if buffer.chars().count() <= MAX_MCP_STDERR_BUFFER_CHARS {
        return;
    }
    let tail = buffer
        .chars()
        .rev()
        .take(MAX_MCP_STDERR_BUFFER_CHARS)
        .collect::<Vec<_>>();
    let mut truncated = tail.into_iter().rev().collect::<String>();
    truncated.insert_str(0, "[MCP stderr buffer truncated]\n");
    *buffer = truncated;
}

fn mcp_operation_error_resets_connection(error: &anyhow::Error) -> bool {
    !format!("{error:#}").contains("MCP server returned error for request")
}

fn with_mcp_stderr_context<T>(
    result: Result<T>,
    server_name: &str,
    stderr_text: &str,
) -> Result<T> {
    if stderr_text.trim().is_empty() {
        return result;
    }
    result.with_context(|| {
        format!(
            "MCP server `{server_name}` stderr:\n{}",
            truncate_mcp_stderr(stderr_text)
        )
    })
}

fn truncate_mcp_stderr(stderr_text: &str) -> String {
    const MAX_MCP_STDERR_CHARS: usize = 4_000;
    let mut truncated = stderr_text
        .chars()
        .take(MAX_MCP_STDERR_CHARS)
        .collect::<String>();
    if stderr_text.chars().count() > MAX_MCP_STDERR_CHARS {
        truncated.push_str("\n[MCP stderr truncated]");
    }
    truncated
}

fn write_json_rpc(stdin: &mut impl Write, value: &Value) -> Result<()> {
    let text = serde_json::to_string(value)?;
    stdin.write_all(text.as_bytes())?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

fn read_json_rpc_response(stdout: &mut impl BufRead, id: i64) -> Result<Value> {
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = stdout.read_line(&mut line)?;
        if bytes == 0 {
            bail!("MCP server closed stdout before response id {id}");
        }
        let value: Value = serde_json::from_str(line.trim())
            .with_context(|| format!("MCP server returned invalid JSON: {}", line.trim()))?;
        if value.get("id").and_then(Value::as_i64) != Some(id) {
            continue;
        }
        if let Some(error) = value.get("error") {
            bail!("MCP server returned error for request {id}: {error}");
        }
        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
    }
}

fn normalize_mcp_input_schema(value: Value) -> Value {
    let mut object = match value {
        Value::Object(object) => object,
        _ => Map::new(),
    };
    object
        .entry("type".to_string())
        .or_insert_with(|| Value::String("object".to_string()));
    let needs_properties = object
        .get("properties")
        .is_none_or(|properties| properties.is_null());
    if needs_properties {
        object.insert("properties".to_string(), Value::Object(Map::new()));
    }
    Value::Object(object)
}

fn mcp_call_tool_result_output_schema(structured_content_schema: Value) -> Value {
    json!({
        "type": "object",
        "properties": {
            "content": {
                "type": "array",
                "items": {
                    "type": "object"
                }
            },
            "structuredContent": structured_content_schema,
            "isError": {
                "type": "boolean"
            },
            "_meta": {
                "type": "object"
            }
        },
        "required": ["content"],
        "additionalProperties": false
    })
}

fn mcp_tool_read_only_hint(raw_tool: &Value) -> bool {
    let Some(annotations) = raw_tool.get("annotations").and_then(Value::as_object) else {
        return false;
    };
    annotations
        .get("readOnlyHint")
        .or_else(|| annotations.get("read_only_hint"))
        .or_else(|| annotations.get("readOnly"))
        .or_else(|| annotations.get("read_only"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn unique_callable_name(
    callable_namespace: &str,
    raw_tool_name: &str,
    seen: &mut BTreeSet<(String, String)>,
) -> String {
    let base = sanitize_responses_api_identifier(raw_tool_name, "tool");
    if seen.insert((callable_namespace.to_string(), base.clone())) {
        return base;
    }

    let mut hasher = Sha1::new();
    hasher.update(callable_namespace.as_bytes());
    hasher.update(b"\0");
    hasher.update(raw_tool_name.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let prefix_len = 55.min(base.len());
    let mut candidate = format!("{}_{}", &base[..prefix_len], &hash[..8]);
    let mut counter = 2;
    while !seen.insert((callable_namespace.to_string(), candidate.clone())) {
        let suffix = format!("_{}_{counter}", &hash[..8]);
        let prefix_len = 64usize.saturating_sub(suffix.len()).min(base.len());
        candidate = format!("{}{}", &base[..prefix_len], suffix);
        counter += 1;
    }
    candidate
}

fn sanitize_responses_api_identifier(value: &str, fallback: &str) -> String {
    let mut sanitized = value
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    while sanitized.contains("__") {
        sanitized = sanitized.replace("__", "_");
    }
    sanitized = sanitized.trim_matches('_').to_string();
    if sanitized.is_empty() {
        sanitized = fallback.to_string();
    }
    if sanitized.len() > 64 {
        let mut hasher = Sha1::new();
        hasher.update(value.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        let prefix_len = 55.min(sanitized.len());
        sanitized = format!("{}_{}", &sanitized[..prefix_len], &hash[..8]);
    }
    sanitized
}

fn timeout_ms_from_table(
    table: &toml::map::Map<String, toml::Value>,
    ms_key: &str,
    sec_key: &str,
    default_ms: u64,
    path: &Path,
    label_prefix: &str,
) -> Result<u64> {
    if let Some(ms) = table.get(ms_key) {
        return toml_nonnegative_u64(ms, path, &format!("{label_prefix}.{ms_key}"));
    }
    if let Some(sec) = table.get(sec_key) {
        return toml_nonnegative_u64(sec, path, &format!("{label_prefix}.{sec_key}"))
            .map(|sec| sec.saturating_mul(1000));
    }
    Ok(default_ms)
}

fn toml_nonnegative_u64(value: &toml::Value, path: &Path, label: &str) -> Result<u64> {
    let Some(number) = value.as_integer() else {
        bail!(
            "Invalid Browser Use Terminal config `{label}` from `{}`: expected a non-negative integer.",
            path.display()
        );
    };
    if number < 0 {
        bail!(
            "Invalid Browser Use Terminal config `{label}` from `{}`: expected a non-negative integer.",
            path.display()
        );
    }
    Ok(number as u64)
}

fn toml_string_array(value: &toml::Value, path: &Path, label: &str) -> Result<Vec<String>> {
    let Some(array) = value.as_array() else {
        bail!(
            "Invalid Browser Use Terminal config `{label}` from `{}`: expected an array of strings.",
            path.display()
        );
    };
    array
        .iter()
        .map(|item| {
            item.as_str().map(str::to_string).with_context(|| {
                format!(
                    "Invalid Browser Use Terminal config `{label}` from `{}`: expected an array of strings.",
                    path.display()
                )
            })
        })
        .collect()
}

fn toml_env_vars(
    value: &toml::Value,
    path: &Path,
    label: &str,
) -> Result<BTreeMap<String, String>> {
    let Some(array) = value.as_array() else {
        bail!(
            "Invalid Browser Use Terminal config `{label}` from `{}`: expected an array.",
            path.display()
        );
    };
    let mut env = BTreeMap::new();
    for (idx, item) in array.iter().enumerate() {
        let name = if let Some(name) = item.as_str() {
            name
        } else if let Some(table) = item.as_table() {
            match table.get("source").and_then(toml::Value::as_str) {
                None | Some("local") => {}
                Some("remote") => continue,
                Some(source) => bail!(
                    "Invalid Browser Use Terminal config `{label}[{idx}].source` from `{}`: expected `local` or `remote`, got `{source}`.",
                    path.display()
                ),
            }
            table
                .get("name")
                .and_then(toml::Value::as_str)
                .with_context(|| {
                    format!(
                        "Invalid Browser Use Terminal config `{label}[{idx}]` from `{}`: expected a `name` string.",
                        path.display()
                    )
                })?
        } else {
            bail!(
                "Invalid Browser Use Terminal config `{label}[{idx}]` from `{}`: expected a string or table.",
                path.display()
            );
        };
        if let Ok(value) = std::env::var(name) {
            env.insert(name.to_string(), value);
        }
    }
    Ok(env)
}

fn toml_string_map(
    value: &toml::Value,
    path: &Path,
    label: &str,
) -> Result<BTreeMap<String, String>> {
    let Some(table) = value.as_table() else {
        bail!(
            "Invalid Browser Use Terminal config `{label}` from `{}`: expected a table of strings.",
            path.display()
        );
    };
    table
        .iter()
        .map(|(key, value)| {
            let value = value.as_str().with_context(|| {
                format!(
                    "Invalid Browser Use Terminal config `{label}.{key}` from `{}`: expected a string.",
                    path.display()
                )
            })?;
            Ok((key.clone(), value.to_string()))
        })
        .collect()
}

fn absolutize_config_path(base: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

#[cfg(unix)]
fn terminate_process_id(pid: u32) {
    let _ = ProcessCommand::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
    thread::sleep(Duration::from_millis(20));
    let _ = ProcessCommand::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
}

#[cfg(not(unix))]
fn terminate_process_id(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    struct McpSessionCleanup(&'static str);

    impl Drop for McpSessionCleanup {
        fn drop(&mut self) {
            cleanup_mcp_connections_for_session(self.0);
        }
    }

    #[test]
    fn parses_stdio_mcp_server_config_like_codex() -> Result<()> {
        let value: toml::Value = r#"
            [mcp_servers.docs]
            command = "python3"
            args = ["server.py"]
            cwd = "tools"
            required = true
            supports_parallel_tool_calls = true
            startup_timeout_ms = 50
            tool_timeout_sec = 2
            env_vars = ["PATH", { name = "REMOTE_ONLY", source = "remote" }]
            disabled_tools = ["write"]

            [mcp_servers.docs.env]
            TOKEN = "abc"

            [mcp_servers.docs.tools.delete]
            enabled = false
        "#
        .parse()?;
        let mut servers = BTreeMap::new();
        apply_mcp_servers_config_layer(
            &mut servers,
            &value,
            Path::new("/repo/.codex/config.toml"),
            Path::new("/repo/.codex"),
        )?;
        let server = servers.get("docs").expect("server");
        assert_eq!(server.command, "python3");
        assert_eq!(server.args, vec!["server.py"]);
        assert_eq!(server.cwd.as_deref(), Some(Path::new("/repo/.codex/tools")));
        assert!(server.required);
        assert!(server.supports_parallel_tool_calls);
        assert_eq!(server.startup_timeout_ms, 50);
        assert_eq!(server.tool_timeout_ms, 2000);
        assert!(server.disabled_tools.contains("write"));
        assert!(server.disabled_tools.contains("delete"));
        assert_eq!(server.env.get("TOKEN").map(String::as_str), Some("abc"));
        assert_eq!(
            server.env.get("PATH").map(String::as_str),
            std::env::var("PATH").ok().as_deref()
        );
        assert!(!server.env.contains_key("REMOTE_ONLY"));
        Ok(())
    }

    #[test]
    fn mcp_tool_names_are_sanitized_and_preserve_raw_identity() {
        let server = McpServerConfig {
            server_name: "server.one".to_string(),
            command: "cmd".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            required: false,
            supports_parallel_tool_calls: false,
            startup_timeout_ms: 1,
            tool_timeout_ms: 1,
            enabled_tools: None,
            disabled_tools: BTreeSet::new(),
        };
        let definition = McpToolDefinition {
            server,
            raw_tool_name: "tool.two-three".to_string(),
            callable_namespace: format!(
                "mcp__{}__",
                sanitize_responses_api_identifier("server.one", "server")
            ),
            callable_name: sanitize_responses_api_identifier("tool.two-three", "tool"),
            namespace_description: "Tools from the server.one MCP server.".to_string(),
            description: String::new(),
            input_schema: normalize_mcp_input_schema(json!({})),
            output_schema: None,
            read_only_hint: false,
        };
        assert_eq!(definition.callable_namespace, "mcp__server_one__");
        assert_eq!(definition.callable_name, "tool_two_three");
        assert_eq!(
            definition.flat_tool_name(),
            "mcp__server_one__tool_two_three"
        );
        assert_eq!(definition.raw_tool_name, "tool.two-three");
    }

    #[test]
    fn discovers_and_calls_stdio_mcp_tool() -> Result<()> {
        let _cleanup = McpSessionCleanup("test-discovers-and-calls-stdio-mcp-tool");
        let temp = tempfile::tempdir()?;
        let script = temp.path().join("server.py");
        std::fs::write(
            &script,
            r#"
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "test", "version": "1.0"},
            },
        }
    elif method == "tools/list":
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "tools": [{
                    "name": "echo.tool",
                    "description": "Echo text.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"text": {"type": "string"}},
                        "required": ["text"],
                    },
                }]
            },
        }
    elif method == "tools/call":
        text = request.get("params", {}).get("arguments", {}).get("text", "")
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "content": [{"type": "text", "text": "echo:" + text}],
                "structuredContent": {"text": text},
                "isError": False,
            },
        }
    else:
        continue
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
"#,
        )?;
        let server = McpServerConfig {
            server_name: "docs.server".to_string(),
            command: "python3".to_string(),
            args: vec![script.display().to_string()],
            env: BTreeMap::new(),
            cwd: None,
            required: false,
            supports_parallel_tool_calls: false,
            startup_timeout_ms: 2_000,
            tool_timeout_ms: 2_000,
            enabled_tools: None,
            disabled_tools: BTreeSet::new(),
        };
        let servers = BTreeMap::from([("docs.server".to_string(), server)]);
        let tools = discover_tool_definitions_for_session(
            "test-discovers-and-calls-stdio-mcp-tool",
            &servers,
        )?;
        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        assert_eq!(tool.callable_namespace, "mcp__docs_server__");
        assert_eq!(tool.callable_name, "echo_tool");
        assert_eq!(tool.raw_tool_name, "echo.tool");
        assert_eq!(tool.input_schema["properties"]["text"]["type"], "string");

        let result = call_tool_for_session(
            "test-discovers-and-calls-stdio-mcp-tool",
            tool,
            &json!({"text": "hello"}),
        )?;
        assert_eq!(result["content"][0]["text"], "echo:hello");
        assert_eq!(result["structuredContent"]["text"], "hello");
        assert_eq!(result["isError"], false);
        Ok(())
    }

    #[test]
    fn stdio_mcp_connection_persists_across_tool_calls_like_codex() -> Result<()> {
        let _cleanup_a = McpSessionCleanup("test-stateful-mcp-session-a");
        let _cleanup_b = McpSessionCleanup("test-stateful-mcp-session-b");
        let temp = tempfile::tempdir()?;
        let script = temp.path().join("stateful_server.py");
        std::fs::write(
            &script,
            r#"
import json
import sys

calls = 0

for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "stateful", "version": "1.0"},
            },
        }
    elif method == "tools/list":
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "tools": [{
                    "name": "count",
                    "description": "Return call count.",
                    "inputSchema": {"type": "object", "properties": {}},
                }]
            },
        }
    elif method == "tools/call":
        calls += 1
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "content": [{"type": "text", "text": "count:" + str(calls)}],
                "structuredContent": {"calls": calls},
                "isError": False,
            },
        }
    else:
        continue
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
"#,
        )?;
        let server = McpServerConfig {
            server_name: "stateful.server".to_string(),
            command: "python3".to_string(),
            args: vec![script.display().to_string()],
            env: BTreeMap::new(),
            cwd: None,
            required: false,
            supports_parallel_tool_calls: false,
            startup_timeout_ms: 2_000,
            tool_timeout_ms: 2_000,
            enabled_tools: None,
            disabled_tools: BTreeSet::new(),
        };
        let servers = BTreeMap::from([("stateful.server".to_string(), server)]);
        let tools_a =
            discover_tool_definitions_for_session("test-stateful-mcp-session-a", &servers)?;
        let tools_b =
            discover_tool_definitions_for_session("test-stateful-mcp-session-b", &servers)?;
        assert_eq!(tools_a.len(), 1);
        assert_eq!(tools_b.len(), 1);

        let first = call_tool_for_session("test-stateful-mcp-session-a", &tools_a[0], &json!({}))?;
        let second = call_tool_for_session("test-stateful-mcp-session-a", &tools_a[0], &json!({}))?;
        let other_session_first =
            call_tool_for_session("test-stateful-mcp-session-b", &tools_b[0], &json!({}))?;

        assert_eq!(first["structuredContent"]["calls"], 1);
        assert_eq!(first["content"][0]["text"], "count:1");
        assert_eq!(second["structuredContent"]["calls"], 2);
        assert_eq!(second["content"][0]["text"], "count:2");
        assert_eq!(other_session_first["structuredContent"]["calls"], 1);
        assert_eq!(other_session_first["content"][0]["text"], "count:1");
        assert_eq!(
            cleanup_mcp_connections_for_session("test-stateful-mcp-session-a"),
            1
        );
        let after_cleanup =
            call_tool_for_session("test-stateful-mcp-session-a", &tools_a[0], &json!({}))?;
        assert_eq!(after_cleanup["structuredContent"]["calls"], 1);
        Ok(())
    }

    #[test]
    fn stdio_mcp_connection_recovers_after_transport_failure() -> Result<()> {
        let _cleanup = McpSessionCleanup("test-mcp-transport-recovery");
        let temp = tempfile::tempdir()?;
        let script = temp.path().join("flaky_server.py");
        let marker = temp.path().join("already_failed");
        std::fs::write(
            &script,
            r#"
import json
import os
import sys

marker = os.environ["MCP_FAIL_MARKER"]
failed_once = os.path.exists(marker)

for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "flaky", "version": "1.0"},
            },
        }
    elif method == "tools/list":
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "tools": [{
                    "name": "flaky",
                    "description": "Fail once, then recover.",
                    "inputSchema": {"type": "object", "properties": {}},
                }]
            },
        }
    elif method == "tools/call":
        if not failed_once:
            open(marker, "w").close()
            sys.exit(0)
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "content": [{"type": "text", "text": "recovered"}],
                "structuredContent": {"ok": True},
                "isError": False,
            },
        }
    else:
        continue
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
"#,
        )?;
        let server = McpServerConfig {
            server_name: "flaky.server".to_string(),
            command: "python3".to_string(),
            args: vec![script.display().to_string()],
            env: BTreeMap::from([("MCP_FAIL_MARKER".to_string(), marker.display().to_string())]),
            cwd: None,
            required: false,
            supports_parallel_tool_calls: false,
            startup_timeout_ms: 2_000,
            tool_timeout_ms: 2_000,
            enabled_tools: None,
            disabled_tools: BTreeSet::new(),
        };
        let servers = BTreeMap::from([("flaky.server".to_string(), server)]);
        let tools = discover_tool_definitions_for_session("test-mcp-transport-recovery", &servers)?;
        let first = call_tool_for_session("test-mcp-transport-recovery", &tools[0], &json!({}));
        assert!(
            format!("{:#}", first.expect_err("first call should close stdout"))
                .contains("MCP server closed stdout")
        );

        let second = call_tool_for_session("test-mcp-transport-recovery", &tools[0], &json!({}))?;
        assert_eq!(second["content"][0]["text"], "recovered");
        assert_eq!(second["structuredContent"]["ok"], true);
        Ok(())
    }

    #[test]
    fn lists_and_reads_stdio_mcp_resources_like_codex() -> Result<()> {
        let _cleanup = McpSessionCleanup("test-lists-and-reads-stdio-mcp-resources");
        let temp = tempfile::tempdir()?;
        let script = temp.path().join("resource_server.py");
        std::fs::write(
            &script,
            r#"
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    params = request.get("params", {})
    if method == "initialize":
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"resources": {}},
                "serverInfo": {"name": "test", "version": "1.0"},
            },
        }
    elif method == "resources/list":
        if params.get("cursor") == "page-2":
            response = {
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {
                    "resources": [{"uri": "memo://two", "name": "two"}],
                },
            }
        else:
            response = {
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {
                    "resources": [{"uri": "memo://one", "name": "one"}],
                    "nextCursor": "page-2",
                },
            }
    elif method == "resources/templates/list":
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "resourceTemplates": [{"uriTemplate": "memo://{id}", "name": "memo"}],
            },
        }
    elif method == "resources/read":
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "contents": [{
                    "uri": params.get("uri"),
                    "mimeType": "text/plain",
                    "text": "resource body",
                }],
            },
        }
    else:
        continue
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
"#,
        )?;
        let server = McpServerConfig {
            server_name: "docs".to_string(),
            command: "python3".to_string(),
            args: vec![script.display().to_string()],
            env: BTreeMap::new(),
            cwd: None,
            required: false,
            supports_parallel_tool_calls: false,
            startup_timeout_ms: 2_000,
            tool_timeout_ms: 2_000,
            enabled_tools: None,
            disabled_tools: BTreeSet::new(),
        };

        let first_page =
            list_resources_for_session("test-lists-and-reads-stdio-mcp-resources", &server, None)?;
        assert_eq!(first_page["resources"][0]["uri"], "memo://one");
        assert_eq!(first_page["nextCursor"], "page-2");
        let templates = list_resource_templates_for_session(
            "test-lists-and-reads-stdio-mcp-resources",
            &server,
            None,
        )?;
        assert_eq!(
            templates["resourceTemplates"][0]["uriTemplate"],
            "memo://{id}"
        );
        let read = read_resource_for_session(
            "test-lists-and-reads-stdio-mcp-resources",
            &server,
            "memo://one",
        )?;
        assert_eq!(read["contents"][0]["text"], "resource body");

        let all = list_all_resources_for_session(
            "test-lists-and-reads-stdio-mcp-resources",
            &BTreeMap::from([("docs".to_string(), server)]),
        );
        assert_eq!(all["docs"].len(), 2);
        assert_eq!(all["docs"][1]["uri"], "memo://two");
        Ok(())
    }

    #[test]
    fn required_mcp_server_discovery_failure_is_not_silent() -> Result<()> {
        let server = McpServerConfig {
            server_name: "missing".to_string(),
            command: "definitely-missing-mcp-binary".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            required: true,
            supports_parallel_tool_calls: false,
            startup_timeout_ms: 100,
            tool_timeout_ms: 100,
            enabled_tools: None,
            disabled_tools: BTreeSet::new(),
        };
        let servers = BTreeMap::from([("missing".to_string(), server)]);
        let error = discover_tool_definitions_for_session("test-required-mcp-failure", &servers)
            .expect_err("required server should fail");
        assert!(format!("{error:#}").contains("required MCP server `missing` failed"));
        Ok(())
    }

    #[test]
    fn mcp_tool_name_collisions_are_disambiguated_and_read_only_is_detected() -> Result<()> {
        let _cleanup = McpSessionCleanup("test-mcp-tool-name-collisions");
        let temp = tempfile::tempdir()?;
        let script = temp.path().join("server.py");
        std::fs::write(
            &script,
            r#"
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "test", "version": "1.0"},
            },
        }
    elif method == "tools/list":
        response = {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "tools": [
                    {"name": "read.file", "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": True}},
                    {"name": "read-file", "inputSchema": {"type": "object"}}
                ]
            },
        }
    else:
        continue
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
"#,
        )?;
        let server = McpServerConfig {
            server_name: "docs".to_string(),
            command: "python3".to_string(),
            args: vec![script.display().to_string()],
            env: BTreeMap::new(),
            cwd: None,
            required: false,
            supports_parallel_tool_calls: false,
            startup_timeout_ms: 2_000,
            tool_timeout_ms: 2_000,
            enabled_tools: None,
            disabled_tools: BTreeSet::new(),
        };
        let servers = BTreeMap::from([("docs".to_string(), server)]);
        let tools =
            discover_tool_definitions_for_session("test-mcp-tool-name-collisions", &servers)?;
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].callable_name, "read_file");
        assert!(tools[0].read_only_hint);
        assert!(tools[0].supports_parallel_tool_calls());
        assert_ne!(tools[1].callable_name, "read_file");
        assert!(tools[1].callable_name.starts_with("read_file_"));
        assert!(!tools[1].read_only_hint);
        assert!(!tools[1].supports_parallel_tool_calls());
        Ok(())
    }

    #[test]
    fn mcp_operation_errors_include_server_stderr() -> Result<()> {
        let _cleanup = McpSessionCleanup("test-mcp-errors-include-stderr");
        let temp = tempfile::tempdir()?;
        let script = temp.path().join("bad_server.py");
        std::fs::write(
            &script,
            r#"
import sys

sys.stderr.write("startup failed with useful detail\n")
sys.stderr.flush()
"#,
        )?;
        let server = McpServerConfig {
            server_name: "bad".to_string(),
            command: "python3".to_string(),
            args: vec![script.display().to_string()],
            env: BTreeMap::new(),
            cwd: None,
            required: false,
            supports_parallel_tool_calls: false,
            startup_timeout_ms: 2_000,
            tool_timeout_ms: 2_000,
            enabled_tools: None,
            disabled_tools: BTreeSet::new(),
        };
        let error = list_server_tools_for_scope(Some("test-mcp-errors-include-stderr"), &server)
            .expect_err("server should fail");
        let message = format!("{error:#}");
        assert!(message.contains("MCP server `bad` stderr"));
        assert!(message.contains("startup failed with useful detail"));
        Ok(())
    }
}
