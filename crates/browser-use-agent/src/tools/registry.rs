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
//! ## FOLLOW-UP: six handler `Req` types are not yet `Deserialize`
//!
//! [`register`](ToolRegistry::register) requires the handler's `Req` to be
//! [`DeserializeOwned`] so the registry can build it from the model-emitted JSON
//! `input`. Of the ten handlers, only FOUR derive `serde::Deserialize` today —
//! `update_plan`, `request_user_input`, `tool_search`, `web_search`. The other
//! SIX — `shell` ([`ShellRequest`]), `apply_patch` ([`ApplyPatchRequest`]),
//! `view_image` ([`ViewImageRequest`]), `browser` ([`BrowserRequest`]),
//! `python` ([`PythonRequest`]), and `mcp` ([`McpToolCallRequest`]) — derive
//! only `Clone, Debug, PartialEq, [Eq]` and therefore CANNOT be registered yet.
//! Per this WP's contract the handler files are read-only here, so adding
//! `#[derive(serde::Deserialize)]` to those six `Req` structs is a one-line
//! follow-up in each handler file (TODO(WP-I-registry-followup): derive
//! `serde::Deserialize` for `ShellRequest`/`ApplyPatchRequest`/`ViewImageRequest`/
//! `BrowserRequest`/`PythonRequest`/`McpToolCallRequest` — several use
//! non-string-keyed fields, e.g. `ShellRequest.env: HashMap`, which deserialize
//! fine; `BrowserRequest`/`McpToolCallRequest` need a `from`/adapter shape since
//! their `Req` is a parsed/namespaced form, not the raw model arg object).
//! Until those derives land, only the four Deserialize-able tools register.
//!
//! [`ShellRequest`]: crate::tools::handlers::shell::ShellRequest
//! [`ApplyPatchRequest`]: crate::tools::handlers::apply_patch::ApplyPatchRequest
//! [`ViewImageRequest`]: crate::tools::handlers::view_image::ViewImageRequest
//! [`BrowserRequest`]: crate::tools::handlers::browser::BrowserRequest
//! [`PythonRequest`]: crate::tools::handlers::python::PythonRequest
//! [`McpToolCallRequest`]: crate::tools::handlers::mcp::McpToolCallRequest
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

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;
