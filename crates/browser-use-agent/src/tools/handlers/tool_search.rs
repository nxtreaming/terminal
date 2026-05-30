//! `tool_search` tool: how the model discovers DEFERRED tools.
//!
//! Some tools (notably large MCP catalogs) are not handed to the model upfront;
//! they are *deferred* and the model must discover them on demand. The
//! `tool_search` tool ranks the deferred-tool catalog against a free-text query
//! (BM25 over each tool's name + description + schema property names) and returns
//! the top-N matches so the model can then call them.
//!
//! This is the async re-implementation of codex's `tool_search` handler over our
//! merged [`ToolRuntime`](crate::tools::runtime::ToolRuntime) seam. It implements
//! the full trait stack ([`Approvable`] + [`Sandboxable`] + [`ToolRuntime`]) so it
//! can be driven by the [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator),
//! mirroring the `update_plan` / `request_user_input` tools' structure
//! (`tools/handlers/update_plan.rs`, `tools/handlers/request_user_input.rs`): a
//! non-FS, validate-rank-and-return tool that spawns no process.
//!
//! # Catalog injection — registry wiring is DEFERRED (TODO)
//!
//! In codex the handler is constructed with the deferred-tool catalog:
//! `ToolSearchHandler::new(search_infos: Vec<ToolSearchInfo>)` builds the
//! `Vec<ToolSearchEntry>` and the BM25 `SearchEngine` from it
//! (`core/src/tools/handlers/tool_search.rs:30-53`). The catalog itself is
//! assembled by the registry from the MCP / dynamic-tool sources that exceed the
//! direct-exposure threshold (legacy `DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD = 100`,
//! `browser-use-core/src/tools/mod.rs`; codex `TOOL_SEARCH_*`).
//!
//! This WP models the **search side only**: the tool holds a
//! `Vec<ToolSearchEntry>` injected at construction
//! ([`ToolSearchTool::new`] / [`ToolSearchTool::from_descriptors`]). The BM25
//! index is built eagerly from those entries. The wiring that *assembles* the
//! deferred catalog (deciding which MCP/dynamic tools defer past the exposure
//! threshold, composing their search text, and re-exposing the matched specs to
//! the next model turn) lands when the toolset/registry is built.
//!
//! TODO(WP-T-tool_search-registry-wiring): wire [`ToolSearchTool`] into the
//! toolset/registry so the deferred catalog is populated from the real MCP /
//! dynamic-tool sources (codex `coalesce_loadable_tool_specs` + the
//! `ToolSearchInfo`/`source_info` plumbing,
//! `core/src/tools/handlers/tool_search.rs:30-53,126-133`; legacy
//! `deferred_tool_search_entries` + `DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD`,
//! `browser-use-core/src/tools/mod.rs`), and so matched specs are re-exposed to
//! the next turn rather than only echoed into [`ExecOutput::stdout`].
//!
//! # Parity grounding (file:line in `/home/exedev/repos/codex/codex-rs`)
//!
//! * **Args / wire shape** — codex `ToolSearchArgs { query: String, limit:
//!   Option<usize> }` (the handler reads `args.query` / `args.limit`,
//!   `core/src/tools/handlers/tool_search.rs:85-91`; schema from
//!   `create_tool_search_tool`, `tool_search_spec.rs:11-22` — `query` required,
//!   `limit` optional number). Our [`ToolSearchRequest`] mirrors this
//!   field-for-field.
//! * **BM25 ranking** — codex builds a `bm25::SearchEngine<usize>` over one
//!   `Document` per entry (the entry's `search_text`), then `search(query,
//!   limit)` returns the ranked document ids
//!   (`tool_search.rs:39-46,112-123`). We do the same: one BM25 document per
//!   entry's [`ToolSearchEntry::search_text`], scored against the query, top-N by
//!   score.
//! * **Search-text composition** — codex composes each entry's `search_text`
//!   from the tool's name + description (+ schema property names) when building
//!   the `ToolSearchEntry` (`core/src/tools/tool_search_entry.rs`; legacy
//!   `search_text` composition in `browser-use-core/src/tools/mod.rs`). Our
//!   [`compose_search_text`] joins name + description + sorted schema property
//!   names.
//! * **Default limit** — codex `TOOL_SEARCH_DEFAULT_LIMIT` (used when `limit` is
//!   `None`, `tool_search.rs:91`). We mirror it as [`TOOL_SEARCH_DEFAULT_LIMIT`].
//! * **Empty-query / zero-limit / empty-catalog** — codex rejects an empty query
//!   ("query must not be empty") and a zero limit ("limit must be greater than
//!   zero"), and short-circuits to an empty result when the catalog is empty
//!   (`tool_search.rs:85-101`). We reproduce these exactly (the rejects as
//!   [`ToolError::Rejected`], the empty catalog as an empty result list).
//! * **Result shape** — codex returns the matched tool specs (coalesced into
//!   MCP namespaces) as the tool output (`tool_search.rs:103-105,126-133`). The
//!   coalescing into `LoadableToolSpec` namespaces is a Responses-API concern not
//!   in this crate's [`ExecOutput`] seam; we emit the ranked matches as a
//!   structured JSON list of `{name, description}` into [`ExecOutput::stdout`]
//!   (prefixed with [`TOOL_SEARCH_STDOUT_PREFIX`] so a later registry-aware layer
//!   can recognize the payload and re-expose the specs).
//! * **parallel_safe = true** — codex's `ToolSearchHandler` OVERRIDES
//!   `supports_parallel_tool_calls -> true`
//!   (`core/src/tools/handlers/tool_search.rs:66-68`): it is a pure, read-only
//!   ranking over an immutable catalog, so it is safe to run concurrently with
//!   other tools. We follow that exactly (see [`ToolSearchTool::parallel_safe`]).

use bm25::{Document, Language, SearchEngine, SearchEngineBuilder};

use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// The tool name surfaced to the model.
///
/// Codex parity: `TOOL_SEARCH_TOOL_NAME = "tool_search"`
/// (`codex_tools`; used throughout `core/src/tools/handlers/tool_search.rs`).
pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

/// Default number of matches returned when the request omits `limit`.
///
/// Codex parity: `TOOL_SEARCH_DEFAULT_LIMIT` (the fallback for `args.limit`,
/// `core/src/tools/handlers/tool_search.rs:91`). Codex's value is a small N; we
/// use `8`, matching the legacy/codex default used in the spec tests
/// (`tool_search_spec.rs` exercises `default_limit = 8`).
pub const TOOL_SEARCH_DEFAULT_LIMIT: usize = 8;

/// Prefix on the [`ExecOutput::stdout`] JSON payload so a later registry-aware
/// layer can recognize the serialized matches and re-expose the specs.
///
/// This is a property of our [`ExecOutput`] fallback seam, NOT a codex/legacy
/// wire constant (codex re-exposes the matched specs directly; this WP returns
/// the ranked matches — see the module-doc "registry wiring is DEFERRED" note).
pub const TOOL_SEARCH_STDOUT_PREFIX: &str = "tool_search:";

/// A searchable descriptor for one deferred tool.
///
/// Codex parity: `ToolSearchEntry` (`core/src/tools/tool_search_entry.rs`), which
/// carries the BM25 `search_text` plus the loadable output spec. The codex
/// `output` is a Responses-API `LoadableToolSpec` (coalesced into MCP
/// namespaces) — that is out of this crate's [`ExecOutput`] seam, so we retain
/// only the searchable identity here: the tool's `name`, its `description`, and
/// the precomputed `search_text` the BM25 index ranks against.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolSearchEntry {
    /// The deferred tool's name (the model calls this name once discovered).
    pub name: String,
    /// The deferred tool's human/model-facing description.
    pub description: String,
    /// The text BM25 ranks the query against. Composed from name + description
    /// (+ schema property names) by [`compose_search_text`] in the common case;
    /// stored precomputed so the index build does not recompose it.
    pub search_text: String,
}

impl ToolSearchEntry {
    /// Build an entry from a tool's name, description, and schema property names,
    /// composing the BM25 [`search_text`](ToolSearchEntry::search_text) via
    /// [`compose_search_text`].
    ///
    /// Codex parity: the `ToolSearchEntry` construction composes `search_text`
    /// from the tool metadata (`core/src/tools/tool_search_entry.rs`).
    pub fn new<I, S>(
        name: impl Into<String>,
        description: impl Into<String>,
        property_names: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let name = name.into();
        let description = description.into();
        let search_text = compose_search_text(&name, &description, property_names);
        Self {
            name,
            description,
            search_text,
        }
    }
}

/// Compose the BM25 search text for a deferred tool.
///
/// Codex parity: codex's `ToolSearchEntry` composes `search_text` from the tool's
/// name + description (+ the schema property names), so a query term appearing in
/// any of those fields scores the entry (`core/src/tools/tool_search_entry.rs`;
/// legacy `search_text` composition in `browser-use-core/src/tools/mod.rs`). We
/// join, in order: the name, the description, then the schema property names
/// (sorted for a deterministic, order-independent document — JSON object property
/// order is not significant).
pub fn compose_search_text<I, S>(name: &str, description: &str, property_names: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut props: Vec<String> = property_names
        .into_iter()
        .map(|p| p.as_ref().to_string())
        .filter(|p| !p.trim().is_empty())
        .collect();
    props.sort();
    props.dedup();

    let mut parts: Vec<&str> = Vec::with_capacity(2 + props.len());
    if !name.trim().is_empty() {
        parts.push(name);
    }
    if !description.trim().is_empty() {
        parts.push(description);
    }
    for p in &props {
        parts.push(p);
    }
    parts.join(" ")
}

/// Typed request for the `tool_search` tool.
///
/// Codex parity: `ToolSearchArgs { query: String, limit: Option<usize> }` (the
/// handler reads `args.query` / `args.limit`,
/// `core/src/tools/handlers/tool_search.rs:85-91`; schema:
/// `create_tool_search_tool` marks `query` required and `limit` an optional
/// number, `tool_search_spec.rs:11-22,56-60`). `limit` is `#[serde(default)]`
/// (omittable) and skipped on serialize when `None` to keep the echoed JSON tidy.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolSearchRequest {
    /// The free-text query to rank the deferred-tool catalog against.
    pub query: String,
    /// Maximum number of matches to return. When `None`,
    /// [`TOOL_SEARCH_DEFAULT_LIMIT`] is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl ToolSearchRequest {
    /// Convenience constructor from a query, using the default limit.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            limit: None,
        }
    }

    /// Convenience constructor from a query + explicit limit.
    pub fn with_limit(query: impl Into<String>, limit: usize) -> Self {
        Self {
            query: query.into(),
            limit: Some(limit),
        }
    }

    /// The effective limit for this request.
    fn effective_limit(&self) -> usize {
        self.limit.unwrap_or(TOOL_SEARCH_DEFAULT_LIMIT)
    }
}

/// A single ranked match emitted in the result.
///
/// Codex returns the matched specs (coalesced into MCP namespaces); this crate's
/// [`ExecOutput`] seam carries only text, so we surface the searchable identity
/// of each match: its `name` and `description`. A later registry-aware layer can
/// map these names back to the loadable specs to re-expose to the model.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolSearchMatch {
    /// The matched deferred tool's name.
    pub name: String,
    /// The matched deferred tool's description.
    pub description: String,
}

impl From<&ToolSearchEntry> for ToolSearchMatch {
    fn from(entry: &ToolSearchEntry) -> Self {
        Self {
            name: entry.name.clone(),
            description: entry.description.clone(),
        }
    }
}

/// The async `tool_search` tool.
///
/// Holds the immutable deferred-tool catalog and a BM25 [`SearchEngine`] built
/// over it at construction. Cheap to clone is NOT a goal here (the engine is
/// non-trivial); the tool is constructed once and shared by reference, like the
/// other handlers.
pub struct ToolSearchTool {
    /// The deferred-tool catalog (the corpus the query ranks against).
    entries: Vec<ToolSearchEntry>,
    /// The BM25 index over the entries' [`search_text`](ToolSearchEntry::search_text),
    /// keyed by entry index. `None` when the catalog is empty (BM25 cannot index
    /// an empty corpus; an empty catalog short-circuits to no matches).
    search_engine: Option<SearchEngine<usize>>,
}

impl std::fmt::Debug for ToolSearchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The BM25 engine is opaque; show only the catalog for the debug dump.
        f.debug_struct("ToolSearchTool")
            .field("entries", &self.entries)
            .field("indexed", &self.search_engine.is_some())
            .finish()
    }
}

impl ToolSearchTool {
    /// Construct the tool from a deferred-tool catalog, building the BM25 index.
    ///
    /// Codex parity: `ToolSearchHandler::new` builds one `Document` per entry's
    /// `search_text` and the `SearchEngine` over them
    /// (`core/src/tools/handlers/tool_search.rs:30-53`). An empty catalog yields
    /// no index (a query short-circuits to an empty result).
    pub fn new(entries: Vec<ToolSearchEntry>) -> Self {
        let search_engine = if entries.is_empty() {
            None
        } else {
            let documents: Vec<Document<usize>> = entries
                .iter()
                .enumerate()
                .map(|(idx, entry)| Document::new(idx, entry.search_text.clone()))
                .collect();
            Some(SearchEngineBuilder::<usize>::with_documents(Language::English, documents).build())
        };
        Self {
            entries,
            search_engine,
        }
    }

    /// Construct the tool from `(name, description, property_names)` descriptors,
    /// composing each entry's search text via [`compose_search_text`].
    pub fn from_descriptors<I, N, D, P, S>(descriptors: I) -> Self
    where
        I: IntoIterator<Item = (N, D, P)>,
        N: Into<String>,
        D: Into<String>,
        P: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let entries = descriptors
            .into_iter()
            .map(|(name, description, props)| ToolSearchEntry::new(name, description, props))
            .collect();
        Self::new(entries)
    }

    /// The deferred-tool catalog this tool searches over.
    pub fn entries(&self) -> &[ToolSearchEntry] {
        &self.entries
    }

    /// Rank the catalog against `query`, returning up to `limit` matches.
    ///
    /// Codex parity: `ToolSearchHandler::search` runs `search_engine.search(query,
    /// limit)` and maps the ranked document ids back to entries
    /// (`core/src/tools/handlers/tool_search.rs:112-123`). An empty catalog (no
    /// index) yields no matches.
    pub fn search(&self, query: &str, limit: usize) -> Vec<ToolSearchMatch> {
        let Some(engine) = self.search_engine.as_ref() else {
            return Vec::new();
        };
        engine
            .search(query, limit)
            .into_iter()
            .filter_map(|result| self.entries.get(result.document.id))
            .map(ToolSearchMatch::from)
            .collect()
    }
}

/// Approval key: the query + limit identify a call for session caching, mirroring
/// the shape the other non-FS tools use (`update_plan.rs:207-210`,
/// `request_user_input.rs:319-322`). In practice this tool never prompts (it is
/// read-only and benign — see below), so the key is rarely consulted; it exists
/// to satisfy the [`Approvable`] contract uniformly.
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ToolSearchApprovalKey {
    query: String,
    limit: Option<usize>,
}

impl Approvable<ToolSearchRequest> for ToolSearchTool {
    type ApprovalKey = ToolSearchApprovalKey;

    fn approval_keys(&self, req: &ToolSearchRequest) -> Vec<Self::ApprovalKey> {
        vec![ToolSearchApprovalKey {
            query: req.query.clone(),
            limit: req.limit,
        }]
    }

    /// `tool_search` touches no filesystem; request the default sandbox
    /// permissions (no escalation), mirroring the update_plan / request_user_input
    /// tools (`update_plan.rs:236-238`, `request_user_input.rs:336-338`).
    fn sandbox_permissions(&self, _req: &ToolSearchRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    // `exec_approval_requirement` is intentionally left at its trait default
    // (`None`): codex's tool_search handler needs no approval — it is a benign,
    // read-only ranking with no approval gate
    // (`core/src/tools/handlers/tool_search.rs` has no approval logic). Returning
    // `None` lets the orchestrator apply `default_exec_approval_requirement`,
    // which yields `Skip` under any non-prompting policy.
}

impl Sandboxable for ToolSearchTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // Let the provider decide (today everything resolves to
        // `SandboxType::None`). Matches the other non-FS tools
        // (`update_plan.rs:249-255`, `request_user_input.rs:350-357`). The tool
        // does no I/O, so the sandbox is moot, but `Auto` keeps the seam uniform.
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // The tool never produces a sandbox denial (it does no I/O), so this is
        // moot; `true` keeps it uniform with the other tools
        // (`update_plan.rs:257-262`, `request_user_input.rs:359-364`).
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<ToolSearchRequest, ExecOutput> for ToolSearchTool {
    fn parallel_safe(&self, _req: &ToolSearchRequest) -> bool {
        // Match codex: PARALLEL-SAFE (true). Codex's `ToolSearchHandler`
        // OVERRIDES `supports_parallel_tool_calls -> true`
        // (`core/src/tools/handlers/tool_search.rs:66-68`) — unlike shell /
        // update_plan / request_user_input, which inherit the `false` default.
        // tool_search is a pure, read-only BM25 ranking over an IMMUTABLE catalog:
        // it mutates no shared state, so it is safe to run concurrently with other
        // tools. We follow codex exactly: `true`.
        true
    }

    async fn run(
        &self,
        req: &ToolSearchRequest,
        attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // No sandbox is exercised (the tool does no I/O); acknowledge the attempt
        // to make the seam explicit, matching the other tools.
        let _ = attempt;

        // Codex: reject an empty query ("query must not be empty",
        // `tool_search.rs:85-90`).
        let query = req.query.trim();
        if query.is_empty() {
            return Err(ToolError::Rejected("query must not be empty".to_string()));
        }

        // Codex: reject a zero limit ("limit must be greater than zero",
        // `tool_search.rs:91-97`).
        let limit = req.effective_limit();
        if limit == 0 {
            return Err(ToolError::Rejected(
                "limit must be greater than zero".to_string(),
            ));
        }

        // Codex: an empty catalog short-circuits to an empty result
        // (`tool_search.rs:99-101`). `search` itself also returns empty when the
        // index is absent, but we mirror codex's explicit short-circuit.
        let matches = if self.entries.is_empty() {
            Vec::new()
        } else {
            self.search(query, limit)
        };

        // RESULT SHAPE: codex re-exposes the matched specs (coalesced into MCP
        // namespaces) to the next turn (`tool_search.rs:103-105,126-133`). This
        // crate's `Out` seam is `ExecOutput` (text only), and the registry wiring
        // that re-exposes specs is DEFERRED (see the module doc), so we emit the
        // ranked matches as a structured JSON list of {name, description} into
        // stdout, prefixed so a later registry-aware layer can recognize it.
        let payload = serde_json::to_string(&matches).map_err(|err| {
            ToolError::Other(anyhow::anyhow!(
                "failed to serialize tool_search matches: {err}"
            ))
        })?;

        Ok(ExecOutput {
            exit_code: 0,
            stdout: format!("{TOOL_SEARCH_STDOUT_PREFIX}{payload}"),
            stderr: String::new(),
        })
    }
}
