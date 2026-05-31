//! Tool registry: name -> handler dispatch for the async agent engine.
//!
//! This is the seam STATUS.md flagged as missing: turning a model-emitted
//! `ContentPart::ToolCall { name, input }` into a concrete, typed tool
//! execution. The typed [`ToolRuntime<Req, Out>`](crate::tools::ToolRuntime)
//! trait is generic and therefore not object-safe, so the registry stores
//! type-erased [`DynTool`] trait objects. A blanket [`ToolAdapter`] wraps any
//! typed handler whose `Req` is [`serde::de::DeserializeOwned`] and whose `Out`
//! is [`ExecOutput`], deserializing the call's `input` value into the handler's
//! `Req` and running it THROUGH the [`ToolOrchestrator`] so the uniform
//! approval/sandbox policy still applies.
//!
//! ## Why metadata is supplied at registration, not read from the handler
//!
//! The handlers (`tools/handlers/*.rs`) implement only `ToolRuntime`: they carry
//! NO model-facing `name`/`description`/`schema`, and their `parallel_safe`
//! takes a `&Req` (it is per-request, not a static property). The registry
//! therefore takes the advertised name, the [`ToolDefinition`], and a static
//! `parallel_safe` flag at registration time. This matches codex, where the
//! registry maps an advertised name to `(handler, spec)`: the spec is not
//! derived from the handler trait either.
//!
//! ## All ten handlers register (registry gap closed)
//!
//! [`register`](ToolRegistry::register) requires the handler's `Req` to be
//! [`DeserializeOwned`] so the registry can build it from the model-emitted JSON
//! `input`. All ten handlers now satisfy that, so ALL TEN register via the single
//! [`register`](ToolRegistry::register) path:
//!
//! * EIGHT have a `Req` that maps DIRECTLY to the model's argument object: the
//!   four originally-`Deserialize` tools (`update_plan`, `request_user_input`,
//!   `tool_search`, `web_search`) plus `shell` ([`ShellRequest`]),
//!   `apply_patch` ([`ApplyPatchRequest`]), `view_image` ([`ViewImageRequest`]),
//!   and `python` ([`PythonRequest`]) — each now derives `serde::Deserialize`
//!   with `#[serde(default)]` on the carried-but-optional plumbing fields so the
//!   MODEL's argument object deserializes cleanly.
//! * TWO carry a PARSED / namespaced `Req` that is not a direct match for the
//!   model's JSON: `browser` ([`BrowserRequest`]) and `mcp`
//!   ([`McpToolCallRequest`]). Each defines a small `Deserialize`-able wire-args
//!   struct ([`BrowserWireArgs`] / [`McpWireArgs`]) that matches the model object,
//!   plus an `impl From<Wire> for Req`, and the `Req` deserializes THROUGH it via
//!   `#[serde(from = "…WireArgs")]`. That makes the `Req` itself `Deserialize`, so
//!   it registers through the same `register` path (no separate adapter needed).
//!
//! See [`crate::tools::registry::definitions`] for the per-tool
//! [`ToolDefinition`] (name + description + input schema) supplied at
//! registration, and [`default_registry`] for a registry preloaded with all ten.
//!
//! [`ShellRequest`]: crate::tools::handlers::shell::ShellRequest
//! [`ApplyPatchRequest`]: crate::tools::handlers::apply_patch::ApplyPatchRequest
//! [`ViewImageRequest`]: crate::tools::handlers::view_image::ViewImageRequest
//! [`BrowserRequest`]: crate::tools::handlers::browser::BrowserRequest
//! [`BrowserWireArgs`]: crate::tools::handlers::browser::BrowserWireArgs
//! [`PythonRequest`]: crate::tools::handlers::python::PythonRequest
//! [`McpToolCallRequest`]: crate::tools::handlers::mcp::McpToolCallRequest
//! [`McpWireArgs`]: crate::tools::handlers::mcp::McpWireArgs
//!
//! ## Parity
//!
//! Mirrors codex-rs `core/src/tools/registry.rs` (the
//! `ToolRegistry { handlers: HashMap<String, Arc<dyn ToolHandler>> }` keyed by
//! the advertised tool name, with `handler(name)` lookup and a `specs()`
//! exposure of the model-visible tool list) and `core/src/tools/router.rs`
//! (`dispatch_tool_call`: look the handler up by name, error if unknown, then
//! run it through the orchestrator). The type-erased codex `trait ToolHandler`
//! with `async fn handle(...)` (codex `core/src/tools/handlers/mod.rs`) is the
//! direct analogue of our [`DynTool`]. We use trait objects rather than codex's
//! `ToolKind` enum-match — and rather than the legacy `browser-use-core`
//! `ToolHandlerKind` enum registry with its `model_visible_specs` /
//! `deferred_tool_search_entries`
//! (`browser-use-core/src/tools/mod.rs`) — because the trait-object form is the
//! design `docs/agent-design/DESIGN.md` prescribes for the dispatch loop
//! ("registry, name->handler routing, parallel/serial", DESIGN.md:34).

use std::collections::BTreeMap;
use std::marker::PhantomData;

use async_trait::async_trait;
use browser_use_llm::schema::ToolDefinition;
use serde::de::DeserializeOwned;

use crate::tools::approval::AskForApproval;
use crate::tools::handlers::tool_search::ToolSearchEntry;
use crate::tools::orchestrator::TurnEnv;
use crate::tools::runtime::{Approvable, AutoApprover, ToolRuntime};
use crate::tools::sandbox::{NoneSandboxProvider, SandboxProvider};
use crate::tools::{Approver, ExecOutput, ToolCtx, ToolError, ToolOrchestrator};

/// A type-erased tool handler the registry can dispatch to by name.
///
/// Erases each concrete handler's `Req` so a heterogeneous set of typed
/// [`ToolRuntime`] implementations (each with its own `ShellRequest`,
/// `ApplyPatchRequest`, …) can live behind one `Box<dyn DynTool>`. Generic over
/// the orchestrator's sandbox/approver seams `(S, A)` — defaulting to the
/// `None`/auto seams — so `call` can route through a concrete
/// [`ToolOrchestrator<S, A>`] while the trait object itself stays object-safe
/// (the type params are fixed per registry).
///
/// Analogous to codex's `trait ToolHandler` (codex
/// `core/src/tools/handlers/mod.rs`).
#[async_trait]
pub trait DynTool<S = NoneSandboxProvider, A = AutoApprover>: Send + Sync
where
    S: SandboxProvider,
    A: Approver,
{
    /// Stable snake_case tool name as advertised to the model.
    fn name(&self) -> &str;

    /// Provider-neutral definition (name + description + input schema) the
    /// engine exposes to the model.
    fn definition(&self) -> ToolDefinition;

    /// Whether this tool may run in parallel with other parallel-safe tools.
    fn parallel_safe(&self) -> bool;

    /// Run the tool from an erased JSON `input`, routing THROUGH the
    /// orchestrator so approval/sandbox policy still applies.
    ///
    /// Deserializes `input` into the concrete handler's `Req`; a deserialize
    /// failure surfaces as [`ToolError::Other`] naming the offending tool.
    async fn call(
        &self,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
    ) -> Result<ExecOutput, ToolError>;
}

/// Adapter that lifts a typed [`ToolRuntime<Req, ExecOutput>`] into a
/// [`DynTool`].
///
/// The orchestrator's `run` is generic over the handler's `Req`; this adapter
/// pins `Req` (and `Out = ExecOutput`) so the resulting `DynTool` is
/// object-safe. It also carries the model-facing metadata the handler trait
/// does not provide: the advertised `name`, the [`ToolDefinition`], and a static
/// `parallel_safe` flag.
pub struct ToolAdapter<T, Req> {
    tool: T,
    name: String,
    definition: ToolDefinition,
    parallel_safe: bool,
    _req: PhantomData<fn() -> Req>,
}

impl<T, Req> ToolAdapter<T, Req> {
    /// Wrap a typed handler with the metadata the registry advertises for it.
    pub fn new(
        tool: T,
        name: impl Into<String>,
        definition: ToolDefinition,
        parallel_safe: bool,
    ) -> Self {
        Self {
            tool,
            name: name.into(),
            definition,
            parallel_safe,
            _req: PhantomData,
        }
    }
}

#[async_trait]
impl<T, Req, S, A> DynTool<S, A> for ToolAdapter<T, Req>
where
    Req: DeserializeOwned + Send + Sync,
    T: ToolRuntime<Req, ExecOutput> + Send + Sync,
    // The orchestrator's `run` future holds a slice of the tool's approval keys
    // across the `.await`; it is only `Send` if the key type is `Send + Sync`.
    // Every handler's `ApprovalKey` is a plain owned value type, so this bound
    // holds for all 10 tools.
    <T as Approvable<Req>>::ApprovalKey: Send + Sync,
    S: SandboxProvider,
    A: Approver,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn parallel_safe(&self) -> bool {
        self.parallel_safe
    }

    async fn call(
        &self,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
    ) -> Result<ExecOutput, ToolError> {
        let req: Req = serde_json::from_value(input.clone()).map_err(|source| {
            ToolError::Other(anyhow::anyhow!(
                "tool `{}`: invalid arguments: {source}",
                self.name
            ))
        })?;
        // Route through the orchestrator so approval/sandbox/escalation policy
        // applies uniformly (parity with codex router.rs, where dispatch goes
        // through the handler under the orchestrator's policy wrapper).
        let result = orchestrator.run(&self.tool, &req, ctx, env, policy).await?;
        Ok(result.output)
    }
}

/// Adapter that lifts a typed [`ToolRuntime<Req, ExecOutput>`] into a [`DynTool`]
/// when the MODEL's wire arguments are a DIFFERENT type `Wire` than the handler's
/// `Req`, bridged by `Wire: Into<Req>`.
///
/// Two handlers (`browser`, `mcp`) take a PARSED / namespaced `Req` that is not a
/// direct match for the model's JSON argument object (the browser `Req` carries a
/// tagged [`BrowserAction`] enum; the mcp `Req` carries `server`/`tool` split out
/// of the namespaced function NAME). For those, we deserialize a small
/// `Wire`-args struct that matches the model JSON and convert it into the typed
/// `Req` via [`From`]. The orchestrator still runs the typed `Req`, so
/// approval/sandbox policy and behavior are unchanged.
///
/// [`BrowserAction`]: crate::tools::handlers::browser::BrowserAction
pub struct WireToolAdapter<T, Wire, Req> {
    tool: T,
    name: String,
    definition: ToolDefinition,
    parallel_safe: bool,
    _wire: PhantomData<fn() -> Wire>,
    _req: PhantomData<fn() -> Req>,
}

impl<T, Wire, Req> WireToolAdapter<T, Wire, Req> {
    /// Wrap a typed handler whose model wire args are `Wire` (convertible into
    /// the handler's `Req`).
    pub fn new(
        tool: T,
        name: impl Into<String>,
        definition: ToolDefinition,
        parallel_safe: bool,
    ) -> Self {
        Self {
            tool,
            name: name.into(),
            definition,
            parallel_safe,
            _wire: PhantomData,
            _req: PhantomData,
        }
    }
}

#[async_trait]
impl<T, Wire, Req, S, A> DynTool<S, A> for WireToolAdapter<T, Wire, Req>
where
    Wire: DeserializeOwned + Send + Sync + Into<Req>,
    Req: Send + Sync,
    T: ToolRuntime<Req, ExecOutput> + Send + Sync,
    <T as Approvable<Req>>::ApprovalKey: Send + Sync,
    S: SandboxProvider,
    A: Approver,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn parallel_safe(&self) -> bool {
        self.parallel_safe
    }

    async fn call(
        &self,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
    ) -> Result<ExecOutput, ToolError> {
        // Deserialize the MODEL's wire-args type, then convert into the handler's
        // parsed `Req`. A deserialize failure surfaces as `ToolError::Other`
        // naming the offending tool (same shape as `ToolAdapter::call`).
        let wire: Wire = serde_json::from_value(input.clone()).map_err(|source| {
            ToolError::Other(anyhow::anyhow!(
                "tool `{}`: invalid arguments: {source}",
                self.name
            ))
        })?;
        let req: Req = wire.into();
        let result = orchestrator.run(&self.tool, &req, ctx, env, policy).await?;
        Ok(result.output)
    }
}

/// Registry of tool handlers keyed by the name advertised to the model.
///
/// Parity: codex `ToolRegistry { handlers: HashMap<String, Arc<dyn ToolHandler>> }`.
/// We key on a [`BTreeMap`] for deterministic ordering of model-visible
/// definitions for free. Generic over the orchestrator seams `(S, A)`, defaulting
/// to the `None`/auto seams.
pub struct ToolRegistry<S = NoneSandboxProvider, A = AutoApprover>
where
    S: SandboxProvider,
    A: Approver,
{
    tools: BTreeMap<String, Box<dyn DynTool<S, A>>>,
    deferred: Vec<ToolSearchEntry>,
}

impl<S, A> Default for ToolRegistry<S, A>
where
    S: SandboxProvider,
    A: Approver,
{
    fn default() -> Self {
        Self {
            tools: BTreeMap::new(),
            deferred: Vec::new(),
        }
    }
}

impl<S, A> ToolRegistry<S, A>
where
    S: SandboxProvider,
    A: Approver,
{
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a typed handler under `name`, with its model-facing definition
    /// and static `parallel_safe` flag.
    ///
    /// The handler is wrapped in a [`ToolAdapter`] and stored type-erased. A
    /// later registration with the same name replaces the earlier one (last
    /// write wins), matching codex's `HashMap::insert` semantics.
    pub fn register<T, Req>(
        &mut self,
        name: impl Into<String>,
        definition: ToolDefinition,
        parallel_safe: bool,
        tool: T,
    ) where
        Req: DeserializeOwned + Send + Sync + 'static,
        T: ToolRuntime<Req, ExecOutput> + Send + Sync + 'static,
        <T as Approvable<Req>>::ApprovalKey: Send + Sync,
    {
        let name = name.into();
        let adapter = ToolAdapter::<T, Req>::new(tool, name.clone(), definition, parallel_safe);
        self.tools.insert(name, Box::new(adapter));
    }

    /// Register an already-erased [`DynTool`] under its own `name()`.
    pub fn register_dyn(&mut self, tool: Box<dyn DynTool<S, A>>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    /// Look up a handler by name.
    pub fn get(&self, name: &str) -> Option<&dyn DynTool<S, A>> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    /// Whether a handler is registered under `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Number of registered handlers.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry holds no handlers.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Set the deferred tool catalog that `tool_search` searches over.
    ///
    /// Parity: legacy `browser-use-core` exposed these via
    /// `deferred_tool_search_entries`; codex likewise keeps the deferred set out
    /// of the always-on spec list. The entries are the same
    /// [`ToolSearchEntry`] catalog the `tool_search` handler is constructed with.
    pub fn set_deferred_search_entries(&mut self, entries: Vec<ToolSearchEntry>) {
        self.deferred = entries;
    }

    /// The deferred tool catalog entries fed to `tool_search`.
    ///
    /// Parity: legacy `browser-use-core::deferred_tool_search_entries`.
    pub fn deferred_search_entries(&self) -> &[ToolSearchEntry] {
        &self.deferred
    }

    /// Model-visible tool definitions for every registered handler, in
    /// deterministic (name-sorted) order.
    ///
    /// Parity: codex `ToolRegistry::specs()` /
    /// legacy `browser-use-core::model_visible_specs`.
    pub fn model_visible_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    /// Whether a registered tool is parallel-safe; `None` if not registered.
    ///
    /// The dispatcher feeds this into its parallel/serial gate
    /// (`turn::decision`).
    pub fn parallel_safe(&self, name: &str) -> Option<bool> {
        self.tools.get(name).map(|t| t.parallel_safe())
    }

    /// Dispatch a tool call by name, routing through the orchestrator.
    ///
    /// Parity: codex `router.rs::dispatch_tool_call` (look up the handler by
    /// name, error if unknown, then run it under the orchestrator's policy).
    /// An unknown name is a [`ToolError::Other`] naming the missing tool.
    pub async fn dispatch(
        &self,
        name: &str,
        input: &serde_json::Value,
        ctx: &ToolCtx,
        env: &TurnEnv,
        policy: AskForApproval,
        orchestrator: &ToolOrchestrator<S, A>,
    ) -> Result<ExecOutput, ToolError> {
        let tool = self
            .get(name)
            .ok_or_else(|| ToolError::Other(anyhow::anyhow!("unknown tool `{name}`")))?;
        tool.call(input, ctx, env, policy, orchestrator).await
    }
}

impl<S, A> std::fmt::Debug for ToolRegistry<S, A>
where
    S: SandboxProvider,
    A: Approver,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("deferred", &self.deferred.len())
            .finish()
    }
}

/// Model-facing [`ToolDefinition`] builders for each of the ten handlers.
///
/// The registry takes a tool's name + description + input schema at registration
/// (it does NOT derive them from the handler trait — see the module docs). These
/// builders centralize the parity-grounded schema shape for each tool so the
/// dispatch loop (and tests) register a consistent definition. Each schema is a
/// JSON-Schema object mirroring the codex/legacy tool spec the field names come
/// from. Field names match the handlers' `Req` / wire-args structs.
pub mod definitions {
    use browser_use_llm::schema::ToolDefinition;
    use serde_json::json;

    /// `shell`: argv command + optional cwd/timeout/env. Parity: codex
    /// `ExecParams` (core/src/exec.rs:83-96) / legacy shell spec
    /// (`browser-use-core/src/tools/mod.rs`).
    pub fn shell() -> ToolDefinition {
        ToolDefinition {
            name: "shell".to_string(),
            description: "Run a shell command (argv-style) and capture its output.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Command and arguments, argv-style (first element is the program)."
                    },
                    "cwd": { "type": "string", "description": "Working directory (defaults to the session cwd)." },
                    "timeout_ms": { "type": "integer", "description": "Per-command timeout in milliseconds." },
                    "env": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "Extra environment variables for the child process."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    /// `apply_patch`: a V4A patch envelope. Parity: codex apply-patch
    /// (`apply-patch/src/parser.rs`) / legacy `browser-use-core/src/tools/files.rs`.
    pub fn apply_patch() -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".to_string(),
            description: "Apply a V4A patch envelope to files under the workspace root."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "The full V4A patch text (*** Begin Patch ... *** End Patch)."
                    }
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
        }
    }

    /// `view_image`: a local image path. Parity: codex `ViewImageArgs { path }`
    /// (core/src/tools/handlers/view_image.rs:53-58) / legacy `view_image`
    /// (`files.rs`).
    pub fn view_image() -> ToolDefinition {
        ToolDefinition {
            name: "view_image".to_string(),
            description: "Read a local image file and return it as model-visible image content."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to a local image (png/jpeg/gif/webp)." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    /// `browser`: a tagged browser action. Parity: browser-use's browser tool
    /// (the hidden `browser <cmd>` command path + the `browser_execute` /
    /// `observe` / `cancel` script paths; legacy
    /// `browser-use-core/src/tools/mod.rs`).
    pub fn browser() -> ToolDefinition {
        ToolDefinition {
            name: "browser".to_string(),
            description:
                "Drive the browser: run a command, execute a script, or observe/cancel a run."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["command", "execute", "observe", "cancel"],
                        "description": "Which browser operation to perform."
                    },
                    "session_id": { "type": "string", "description": "Browser session id." },
                    "command": { "type": "string", "description": "Command string for the `command` action." },
                    "script": { "type": "string", "description": "Script body for the `execute` action." },
                    "background": { "type": "boolean", "description": "Run an `execute` in the background." },
                    "run_id": { "type": "string", "description": "Run id for `observe`/`cancel`." },
                    "timeout_secs": { "type": "integer", "description": "Script timeout in seconds." },
                    "observe_timeout_ms": { "type": "integer", "description": "Observe poll window in ms." }
                },
                "required": ["action", "session_id"],
                "additionalProperties": false
            }),
        }
    }

    /// `python`: a Python snippet + optional timeout. Parity: legacy
    /// `dispatch_python_tool` (`browser-use-core/src/lib.rs`).
    pub fn python() -> ToolDefinition {
        ToolDefinition {
            name: "python".to_string(),
            description: "Run a Python snippet in a persistent worker and return its output."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "The Python source to execute." },
                    "session_id": { "type": "string", "description": "Worker session id (persistent namespace)." },
                    "timeout_secs": { "type": "number", "description": "Optional timeout in seconds." }
                },
                "required": ["code"],
                "additionalProperties": false
            }),
        }
    }

    /// `mcp`: a resolved MCP `tools/call`. Parity: legacy
    /// `dispatch_mcp_tool(server, tool, arguments, ..)`
    /// (`browser-use-core/src/lib.rs:13398-13403`) / codex
    /// `core/src/mcp_tool_call.rs`.
    pub fn mcp() -> ToolDefinition {
        ToolDefinition {
            name: "mcp".to_string(),
            description: "Call a tool on a connected MCP server.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "description": "The MCP server name." },
                    "tool": { "type": "string", "description": "The tool name on that server." },
                    "arguments": { "type": "object", "description": "JSON arguments for the call." },
                    "read_only": { "type": "boolean", "description": "Whether the tool is read-only (parallel-safe)." }
                },
                "required": ["server", "tool"],
                "additionalProperties": false
            }),
        }
    }

    /// `update_plan`: the structured plan. Parity: codex `UpdatePlanArgs`
    /// (core/src/tools/handlers/plan.rs) / legacy `UpdatePlanArgs`.
    pub fn update_plan() -> ToolDefinition {
        ToolDefinition {
            name: "update_plan".to_string(),
            description: "Record a structured task plan the model is tracking.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": { "type": "string" },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["step", "status"]
                        }
                    }
                },
                "required": ["plan"],
                "additionalProperties": true
            }),
        }
    }

    /// `request_user_input`: ask the user one to three short questions, each with
    /// mutually-exclusive options, and pause until they respond.
    ///
    /// Parity: codex `RequestUserInputArgs { questions:
    /// Vec<RequestUserInputQuestion> }` (`protocol/src/request_user_input.rs`),
    /// where each question carries `id`, `header`, `question`, the camelCase
    /// `isOther` / `isSecret` flags, and an `options` array of `{ label,
    /// description }`. The schema MUST match what the handler actually accepts —
    /// [`RequestUserInputRequest`](crate::tools::handlers::request_user_input::RequestUserInputRequest),
    /// which deserializes `{ "questions": [...] }`, NOT a flat `{ "prompt": ... }`
    /// (the old schema advertised a shape the handler rejects — a real
    /// correctness bug).
    pub fn request_user_input() -> ToolDefinition {
        ToolDefinition {
            name: "request_user_input".to_string(),
            description: "Ask the user one to three short questions (each with \
                          mutually-exclusive options) and pause until they respond."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "The questions to show the user (prefer 1, do not exceed 3).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "Stable snake_case identifier for mapping the answer."
                                },
                                "header": {
                                    "type": "string",
                                    "description": "Short header label shown in the UI (12 or fewer chars)."
                                },
                                "question": {
                                    "type": "string",
                                    "description": "Single-sentence prompt shown to the user."
                                },
                                "isOther": {
                                    "type": "boolean",
                                    "description": "Whether to add a free-form \"Other\" option (forced true on normalize)."
                                },
                                "isSecret": {
                                    "type": "boolean",
                                    "description": "Whether the answer is a secret (masked input)."
                                },
                                "options": {
                                    "type": "array",
                                    "description": "The mutually-exclusive choices (required, non-empty).",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {
                                                "type": "string",
                                                "description": "User-facing label (1-5 words)."
                                            },
                                            "description": {
                                                "type": "string",
                                                "description": "One short sentence explaining the impact if selected."
                                            }
                                        },
                                        "required": ["label", "description"]
                                    }
                                }
                            },
                            "required": ["id", "header", "question", "options"]
                        }
                    }
                },
                "required": ["questions"],
                "additionalProperties": false
            }),
        }
    }

    /// `tool_search`: BM25 over the deferred catalog. Parity: legacy
    /// `tool_search` args `{ query, limit? }`
    /// (`browser-use-core/src/tools/mod.rs`).
    pub fn tool_search() -> ToolDefinition {
        ToolDefinition {
            name: "tool_search".to_string(),
            description: "Search the deferred tool catalog by free-text query.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The free-text query." },
                    "limit": { "type": "integer", "description": "Max ranked entries to return." }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    /// `done`: the completion tool the model calls to declare the task finished,
    /// carrying its final summary. Parity: codex/legacy completion (`done`) tool
    /// (`{ "text"?: string }`). The handler's
    /// [`DoneRequest`](crate::tools::handlers::done::DoneRequest) accepts an
    /// optional `text` summary.
    pub fn done() -> ToolDefinition {
        ToolDefinition {
            name: "done".to_string(),
            description:
                "Signal that the task is finished, with an optional final summary message."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The final summary message describing what was accomplished."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    /// `web_search`: a hosted/passthrough web search. Parity: codex
    /// `WebSearchArgs { query }` / legacy web_search args.
    pub fn web_search() -> ToolDefinition {
        ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web for a free-text query.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The free-text search query." }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    /// `spawn_agent`: delegate a task to a child sub-agent. Parity: codex
    /// `create_spawn_agent_tool_v2` (`multi_agents_spec.rs:75-109`) — required
    /// `["task_name", "message"]`; matches
    /// [`SpawnAgentArgs`](crate::subagents::SpawnAgentArgs) (the request type the
    /// handler deserializes), which is `deny_unknown_fields`.
    pub fn spawn_agent() -> ToolDefinition {
        ToolDefinition {
            name: "spawn_agent".to_string(),
            description: "Spawn a sub-agent to work on a delegated task.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The task/message for the new agent."
                    },
                    "task_name": {
                        "type": "string",
                        "description": "Short canonical name for the task (lowercase letters, digits, underscores)."
                    },
                    "agent_type": {
                        "type": "string",
                        "description": "Optional role for the new agent. If omitted, `default` is used."
                    },
                    "fork_turns": {
                        "type": "string",
                        "description": "`none`, `all`, or a positive integer. Defaults to `all`."
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional model override for the new agent."
                    },
                    "reasoning_effort": {
                        "type": "string",
                        "description": "Optional reasoning-effort override for the new agent."
                    },
                    "service_tier": {
                        "type": "string",
                        "description": "Optional service-tier override for the new agent."
                    }
                },
                "required": ["task_name", "message"],
                "additionalProperties": false
            }),
        }
    }

    /// `wait_agent`: EVENT-NOTIFY wait for a child to report news. Parity: codex
    /// `multi_agents_v2/wait.rs` (the parent blocks on the mailbox, then reads the
    /// child's status).
    pub fn wait_agent() -> ToolDefinition {
        ToolDefinition {
            name: "wait_agent".to_string(),
            description: "Wait for a spawned sub-agent to report progress or completion."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent_path": {
                        "type": "string",
                        "description": "Canonical path of the child agent to wait on (from spawn_agent)."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Optional wait budget in seconds (default 300)."
                    }
                },
                "required": ["agent_path"],
                "additionalProperties": false
            }),
        }
    }

    /// `send_input`: deliver a message to a running child agent (codex
    /// `enqueue_mailbox_communication`).
    pub fn send_input() -> ToolDefinition {
        ToolDefinition {
            name: "send_input".to_string(),
            description: "Send a message to a running sub-agent.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent_path": {
                        "type": "string",
                        "description": "Canonical path of the child agent to deliver input to."
                    },
                    "message": {
                        "type": "string",
                        "description": "The message/prompt body delivered to the child agent."
                    }
                },
                "required": ["agent_path", "message"],
                "additionalProperties": false
            }),
        }
    }

    /// `list_agents`: a read-only snapshot of the live sub-agent registry (codex
    /// `live_agents`).
    pub fn list_agents() -> ToolDefinition {
        ToolDefinition {
            name: "list_agents".to_string(),
            description: "List the currently live sub-agents and their statuses.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }
}

/// Build a [`ToolRegistry`] preloaded with all eleven handlers, each carrying its
/// parity-grounded [`ToolDefinition`] and static `parallel_safe` flag.
///
/// This is the single place the dispatch loop wires the full tool set. Nine
/// tools register directly; `browser` and `mcp` register via
/// [`register_with_wire`](ToolRegistry::register_with_wire) over their
/// `WireArgs` types. The browser/python/mcp handlers need an injected backend
/// (they would otherwise reach the OS), so those are supplied by the caller.
///
/// `parallel_safe` per tool: `tool_search` / `web_search` = `true` (pure /
/// read-only); `shell` / `apply_patch` / `view_image` / `browser` / `python` /
/// `update_plan` / `request_user_input` = `false` (serial). `mcp` is registered
/// `false` here (a serial default); its per-request read-only hint still drives
/// the handler's own [`ToolRuntime::parallel_safe`](crate::tools::ToolRuntime::parallel_safe).
#[allow(clippy::too_many_arguments)]
pub fn default_registry<S, A>(
    shell: crate::tools::handlers::shell::ShellTool,
    apply_patch: crate::tools::handlers::apply_patch::ApplyPatchTool,
    view_image: crate::tools::handlers::view_image::ViewImageTool,
    browser: crate::tools::handlers::browser::BrowserTool,
    python: crate::tools::handlers::python::PythonTool,
    mcp: crate::tools::handlers::mcp::McpTool,
    update_plan: crate::tools::handlers::update_plan::UpdatePlanTool,
    request_user_input: crate::tools::handlers::request_user_input::RequestUserInputTool,
    tool_search: crate::tools::handlers::tool_search::ToolSearchTool,
    web_search: crate::tools::handlers::web_search::WebSearchTool,
    done: crate::tools::handlers::done::DoneTool,
) -> ToolRegistry<S, A>
where
    S: SandboxProvider,
    A: Approver,
{
    use crate::tools::handlers::apply_patch::ApplyPatchRequest;
    use crate::tools::handlers::browser::BrowserRequest;
    use crate::tools::handlers::done::DoneRequest;
    use crate::tools::handlers::mcp::McpToolCallRequest;
    use crate::tools::handlers::python::PythonRequest;
    use crate::tools::handlers::request_user_input::RequestUserInputRequest;
    use crate::tools::handlers::shell::ShellRequest;
    use crate::tools::handlers::tool_search::ToolSearchRequest;
    use crate::tools::handlers::update_plan::UpdatePlanRequest;
    use crate::tools::handlers::view_image::ViewImageRequest;
    use crate::tools::handlers::web_search::WebSearchRequest;

    let mut reg = ToolRegistry::new();

    reg.register::<_, ShellRequest>("shell", definitions::shell(), false, shell);
    reg.register::<_, ApplyPatchRequest>(
        "apply_patch",
        definitions::apply_patch(),
        false,
        apply_patch,
    );
    reg.register::<_, ViewImageRequest>("view_image", definitions::view_image(), false, view_image);
    reg.register::<_, PythonRequest>("python", definitions::python(), false, python);
    // `browser` / `mcp` carry a parsed / namespaced `Req`; each deserializes
    // THROUGH its `WireArgs` via `#[serde(from = "…WireArgs")]`, so the plain
    // `register` path works (the registry deserializes the model object straight
    // into the `Req`).
    reg.register::<_, BrowserRequest>("browser", definitions::browser(), false, browser);
    reg.register::<_, McpToolCallRequest>("mcp", definitions::mcp(), false, mcp);
    reg.register::<_, UpdatePlanRequest>(
        "update_plan",
        definitions::update_plan(),
        false,
        update_plan,
    );
    reg.register::<_, RequestUserInputRequest>(
        "request_user_input",
        definitions::request_user_input(),
        false,
        request_user_input,
    );
    reg.register::<_, ToolSearchRequest>(
        "tool_search",
        definitions::tool_search(),
        true,
        tool_search,
    );
    reg.register::<_, WebSearchRequest>("web_search", definitions::web_search(), true, web_search);
    // `done`: the completion tool. Serial (terminal; must not be reordered).
    reg.register::<_, DoneRequest>("done", definitions::done(), false, done);

    reg
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;
