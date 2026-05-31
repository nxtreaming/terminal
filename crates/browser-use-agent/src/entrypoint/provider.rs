//! Provider resolution for the run-entrypoint facade.
//!
//! This turns a binary-facing [`ProviderRunConfig`] (its [`ProviderBackend`] +
//! model) into a concrete model [`Route`] and a ready-to-run
//! [`ModelSamplingDriver`] built via [`build_sampling_driver`] — the production
//! multi-provider path that `turn::model_path` ships.
//!
//! **This is the single place where [`build_sampling_driver`] is actually called
//! from a binary-facing entrypoint.** The real OpenAI / Anthropic / OpenAI-compatible
//! routes are *constructed* here (base-url + auth-header derivation, no network),
//! the transport is built over a [`ModelClient`], and the driver wraps it. No byte
//! goes on the wire until the returned driver's `run_sampling_request` awaits
//! `ModelClient::stream`. That is why a real driver can be CONSTRUCTED offline
//! (the facade tests assert exactly this) even though we have no API key to run it.
//!
//! ## Backend mapping (parity with the legacy provider-selection step)
//! The legacy `browser-use-core` run path picks a provider from the configured
//! backend + the standard env credentials. We mirror that:
//!   * [`ProviderBackend::Openai`]      → [`ProviderChoice::OpenAiResponses`]
//!     (key from `OPENAI_API_KEY` / `LLM_BROWSER_OPENAI_API_KEY`, optional
//!     `LLM_BROWSER_OPENAI_BASE_URL`),
//!   * [`ProviderBackend::Anthropic`]   → [`ProviderChoice::Anthropic`]
//!     (key from `ANTHROPIC_API_KEY` / `LLM_BROWSER_ANTHROPIC_API_KEY`),
//!   * [`ProviderBackend::Openrouter`]  → [`ProviderChoice::OpenAiCompatibleProvider`]
//!     id `"openrouter"` (key from `OPENROUTER_API_KEY`),
//!   * [`ProviderBackend::Deepseek`]    → [`ProviderChoice::OpenAiCompatibleProvider`]
//!     id `"deepseek"` (key from `DEEPSEEK_API_KEY`),
//!   * [`ProviderBackend::Fake`]        → [`ResolvedProvider::Fake`] (no real
//!     provider; the facade drives it with an offline scripted driver),
//!   * [`ProviderBackend::Codex`]       → a clear typed error: **codex is cut**.
//!     We do NOT wire `chatgpt.com`; the cut is deliberate (see module-level docs
//!     of [`crate::turn::model_path`]).
//!   * [`ProviderBackend::None`]        → a clear typed error (no provider chosen).
//!
//! Credentials are read from the process environment, exactly like
//! [`provider_choice_from_env`](crate::turn::model_path::provider_choice_from_env);
//! a missing key surfaces as [`ProviderResolveError::MissingCredentials`] (honest,
//! never a panic, never a silent default to codex).

use std::sync::Arc;

use browser_use_llm::route::ModelClient;

use crate::config_overrides::ProviderBackend;
use crate::config_overrides::ProviderRunConfig;
use crate::events::EventSink;
use crate::events::TurnCtx;
use crate::tools::approval::AskForApproval;
use crate::tools::handlers::python::PythonBackend;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::ToolCtx;
use crate::tools::sandbox::FileSystemSandboxPolicy;
use crate::turn::dispatch::{RegistryRunner, ToolDispatcher};
use crate::turn::model_path::build_route;
use crate::turn::model_path::build_sampling_driver;
use crate::turn::model_path::build_transport;
use crate::turn::model_path::ModelPathError;
use crate::turn::model_path::ProviderChoice;
use crate::turn::sampling::FusionRecorder;
use crate::turn::sampling::ModelClientTransport;
use crate::turn::sampling::ModelSamplingDriver;

/// The concrete real-backend sampling driver this facade builds.
///
/// [`build_sampling_driver`] returns the default-runner text-only sampler over a
/// live transport; the entrypoint then attaches FUSED tool dispatch via
/// [`ModelSamplingDriver::with_fusion`], which rebinds the runner type parameter to
/// the production [`RegistryRunner`]. This alias names the fused driver so the
/// entrypoint can hold it without spelling the generics each time. It still
/// implements [`SamplingDriver`](crate::turn::SamplingDriver) (the loop drives the
/// concrete type — the trait is not dyn-compatible, so the driver is held as the
/// concrete generic, not boxed as `dyn SamplingDriver`).
pub type RealSamplingDriver = ModelSamplingDriver<ModelClientTransport, RegistryRunner>;

/// Errors resolving a provider into a driver.
#[derive(Debug)]
pub enum ProviderResolveError {
    /// The configured backend has no real provider in this engine.
    ///
    /// Carries a human-readable reason. The `Codex` backend is cut (chatgpt.com
    /// is no longer wired); `None` means no backend was selected.
    UnsupportedBackend(String),
    /// No usable credential was found in the environment for the chosen backend.
    MissingCredentials(&'static str),
    /// The model route could not be built (e.g. an unknown OpenAI-compatible
    /// provider id). Wraps the real [`ModelPathError`].
    Route(ModelPathError),
    /// The Python worker subprocess could not be started for the run.
    ///
    /// The legacy run path starts ONE [`PythonWorker`] per run (eager, via
    /// `start_with_browser_mode_and_env`) and threads it through dispatch; if
    /// that spawn fails we surface it as a typed error rather than silently
    /// dropping the `python` tool (which would be a hidden regression). Carries
    /// the underlying error's message.
    ///
    /// [`PythonWorker`]: browser_use_python_worker::PythonWorker
    PythonWorker(String),
}

impl std::fmt::Display for ProviderResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderResolveError::UnsupportedBackend(why) => {
                write!(f, "unsupported provider backend: {why}")
            }
            ProviderResolveError::MissingCredentials(which) => {
                write!(f, "no provider credentials found in environment ({which})")
            }
            ProviderResolveError::Route(e) => write!(f, "{e}"),
            ProviderResolveError::PythonWorker(why) => {
                write!(f, "failed to start python worker: {why}")
            }
        }
    }
}

impl std::error::Error for ProviderResolveError {}

impl From<ModelPathError> for ProviderResolveError {
    fn from(e: ModelPathError) -> Self {
        ProviderResolveError::Route(e)
    }
}

/// What provider resolution produced for a config.
///
/// A real backend yields a built [`RealSamplingDriver`] (the live model path,
/// via [`build_sampling_driver`]); the `Fake` backend yields
/// [`ResolvedProvider::Fake`], the signal the entrypoint uses to drive the turn
/// with an offline scripted driver instead.
pub enum ResolvedProvider {
    /// A live model driver, built via [`build_sampling_driver`].
    Real(Box<RealSamplingDriver>),
    /// The fake/test backend: no real provider; caller drives offline.
    Fake,
}

// `ModelSamplingDriver` is not `Debug`, so derive is impossible. A by-hand impl
// lets callers `expect_err`/assert on a `Result<ResolvedProvider, _>` without
// leaking driver internals (it prints only the variant tag).
impl std::fmt::Debug for ResolvedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolvedProvider::Real(_) => f.write_str("ResolvedProvider::Real(..)"),
            ResolvedProvider::Fake => f.write_str("ResolvedProvider::Fake"),
        }
    }
}

/// First non-empty env var among `keys`.
fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
}

/// Map a [`ProviderBackend`] to a model-path [`ProviderChoice`], reading the
/// backend's standard credentials from the process environment.
///
/// Returns:
///   * `Ok(Some(choice))` for a real provider with credentials present,
///   * `Ok(None)` for [`ProviderBackend::Fake`] (no real provider),
///   * `Err(..)` for a cut/absent backend or missing credentials.
///
/// No network I/O — this only reads env and assembles a [`ProviderChoice`].
pub fn provider_choice_for_backend(
    backend: ProviderBackend,
) -> Result<Option<ProviderChoice>, ProviderResolveError> {
    match backend {
        ProviderBackend::Openai => {
            let api_key = env_first(&["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"]).ok_or(
                ProviderResolveError::MissingCredentials(
                    "set OPENAI_API_KEY for the openai backend",
                ),
            )?;
            Ok(Some(ProviderChoice::OpenAiResponses {
                api_key,
                base_url: env_first(&["LLM_BROWSER_OPENAI_BASE_URL"]),
            }))
        }
        ProviderBackend::Anthropic => {
            let api_key = env_first(&["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"])
                .ok_or(ProviderResolveError::MissingCredentials(
                    "set ANTHROPIC_API_KEY for the anthropic backend",
                ))?;
            Ok(Some(ProviderChoice::Anthropic {
                api_key,
                base_url: env_first(&["LLM_BROWSER_ANTHROPIC_BASE_URL"]),
            }))
        }
        ProviderBackend::Openrouter => {
            let api_key = env_first(&["OPENROUTER_API_KEY", "LLM_BROWSER_OPENAI_COMPAT_API_KEY"])
                .ok_or(ProviderResolveError::MissingCredentials(
                "set OPENROUTER_API_KEY for the openrouter backend",
            ))?;
            Ok(Some(ProviderChoice::OpenAiCompatibleProvider {
                provider_id: "openrouter".to_string(),
                api_key,
            }))
        }
        ProviderBackend::Deepseek => {
            let api_key = env_first(&["DEEPSEEK_API_KEY", "LLM_BROWSER_OPENAI_COMPAT_API_KEY"])
                .ok_or(ProviderResolveError::MissingCredentials(
                    "set DEEPSEEK_API_KEY for the deepseek backend",
                ))?;
            Ok(Some(ProviderChoice::OpenAiCompatibleProvider {
                provider_id: "deepseek".to_string(),
                api_key,
            }))
        }
        ProviderBackend::Fake => Ok(None),
        // Phase-E note: codex is being removed in the cutover. Do NOT wire
        // chatgpt.com here; surface a clear typed error instead.
        ProviderBackend::Codex => Err(ProviderResolveError::UnsupportedBackend(
            "codex backend is cut: chatgpt.com is no longer wired".to_string(),
        )),
        ProviderBackend::None => Err(ProviderResolveError::UnsupportedBackend(
            "no provider backend selected".to_string(),
        )),
    }
}

/// Resolve a [`ProviderRunConfig`] into a ready sampling driver (or the Fake
/// signal). **This is the production call site for [`build_sampling_driver`].**
///
/// Steps (parity with the legacy provider-selection path):
/// 1. [`provider_choice_for_backend`] maps the backend → a credentialed
///    [`ProviderChoice`] (`Codex`/`None` → typed error, `Fake` → `None`).
/// 2. For a real choice: [`build_route`] derives the endpoint + auth (offline),
///    [`build_transport`] binds a fresh [`ModelClient`] + the per-turn request,
///    and [`build_sampling_driver`] wraps it into the live [`ModelSamplingDriver`].
/// 3. `Fake` short-circuits to [`ResolvedProvider::Fake`].
///
/// `sink` receives the driver's UI events; `ctx` carries the turn's model/provider
/// identity; `max_retries` is the codex-style stream retry budget; `recorder` is the
/// [`FusionRecorder`] the fused driver records the assistant message + dispatched
/// tool outputs through (it must point at the SAME conversation buffer the loop's
/// `TurnState` re-samples from). No network I/O happens here.
pub fn resolve_provider(
    config: &ProviderRunConfig,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    max_retries: u32,
    recorder: Arc<dyn FusionRecorder>,
) -> Result<ResolvedProvider, ProviderResolveError> {
    // The Fake short-circuit lives in the inner builder (so we never spawn a
    // Python worker for a fake/cut/missing-credential run). For a real backend we
    // start the run's single Python worker EAGERLY here, then thread its backend
    // through. `start_python_backend` only runs once we know the route builds.
    resolve_provider_with_python(config, sink, ctx, max_retries, recorder, None)
}

/// Start the run's single Python worker subprocess (eager, matching legacy
/// `run_existing_session_from_config`, which spawns one
/// `PythonWorker::start_with_browser_mode_and_env` per run and threads it through
/// dispatch).
///
/// `browser_mode` + `python_env` come from the run config's [`AgentRunOptions`],
/// forwarded verbatim. A spawn failure is a typed [`ProviderResolveError::PythonWorker`]
/// (no silent drop of the `python` tool — that would be a hidden regression).
///
/// LIFECYCLE: the returned backend owns the [`PythonWorker`], which is reaped on
/// drop — `PythonWorker`'s `Drop` (python-worker `lib.rs:475`) sends a `shutdown`
/// request then force-kills + waits the child. The backend is held by the
/// `python` handler inside the dispatcher; when the dispatcher (owned by the
/// fused driver) drops at run end, the worker process is reaped — no leak.
///
/// [`AgentRunOptions`]: crate::config_overrides::AgentRunOptions
/// [`PythonWorker`]: browser_use_python_worker::PythonWorker
fn start_python_backend(
    config: &ProviderRunConfig,
) -> Result<Arc<dyn PythonBackend>, ProviderResolveError> {
    let backend = crate::tools::handlers::python::RealBackend::start(
        config.options.browser_mode.as_deref(),
        &config.options.python_env,
    )
    .map_err(|e| ProviderResolveError::PythonWorker(e.to_string()))?;
    Ok(Arc::new(backend))
}

/// Inner [`resolve_provider`] that accepts a pre-built Python backend.
///
/// `python_backend = None` means "start the real worker eagerly" (the production
/// path). Tests pass `Some(fake)` to exercise resolution WITHOUT spawning a real
/// Python process — the real-driver-constructs-offline assertion is about the
/// model transport, not the worker, so injecting a fake keeps it network/process
/// free while still wiring the `python` tool through the real dispatcher.
fn resolve_provider_with_python(
    config: &ProviderRunConfig,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    max_retries: u32,
    recorder: Arc<dyn FusionRecorder>,
    python_backend: Option<Arc<dyn PythonBackend>>,
) -> Result<ResolvedProvider, ProviderResolveError> {
    // (1) backend → credentialed provider choice (Codex/None → Err; Fake → None).
    let choice = match provider_choice_for_backend(config.backend)? {
        Some(choice) => choice,
        None => return Ok(ResolvedProvider::Fake),
    };

    // (2) build the real route (offline: URL + auth headers only).
    let route = build_route(&choice, &config.model)?;

    // (3) build the live transport over a fresh ModelClient, then the driver.
    //     The transport owns the per-turn request; we seed it with an empty
    //     message vec — the turn loop threads the real prompt through
    //     `run_sampling_request`, which rebuilds the request per attempt from
    //     `ctx` + the loop's input (the shape `build_transport` documents).
    let client = Arc::new(ModelClient::default());
    let transport = build_transport(client, route, &ctx, Vec::new());

    // (3a) Resolve the Python backend for the run's `python` tool. Real path:
    //      start the single worker eagerly (only reached AFTER the `Fake`/`Codex`/
    //      missing-credential exits above, so those never spawn Python). Tests
    //      inject a fake.
    let python_backend = match python_backend {
        Some(backend) => backend,
        None => start_python_backend(config)?,
    };

    // *** build_sampling_driver is actually CALLED here (production path). ***
    // It yields the text-only sampler; we then attach the FUSED dispatch path so a
    // model tool-call actually EXECUTES (through the registry + orchestrator) and
    // its output re-enters the prompt via `recorder`, and the loop re-samples.
    let driver = build_sampling_driver(transport, sink, ctx, max_retries)
        .with_fusion(build_tool_dispatcher(python_backend), recorder);
    Ok(ResolvedProvider::Real(Box::new(driver)))
}

/// Build the production fused tool dispatcher: a [`ToolRegistry`] behind the REAL
/// [`RegistryRunner`], over a [`ToolOrchestrator`] stub (sandbox = `None`,
/// auto-approve), wrapped in a [`ToolDispatcher`].
///
/// This is the dispatcher the fused [`ModelSamplingDriver`] runs every model
/// tool-call through (codex `try_run_turn` -> router -> orchestrator). The runner
/// dispatches BY NAME through the registry, deserializing the call's `input` into
/// the matching handler's typed `Req` and running it under the orchestrator's
/// approval/sandbox policy, then renders the [`ExecOutput`](crate::tools::ExecOutput)
/// into the recorded tool-result message.
///
/// ## Which tools are wired here
/// The registry registers the backend-free handlers — `shell`, `apply_patch`,
/// `view_image`, `update_plan`, `request_user_input`, `tool_search` (empty
/// catalog), `web_search` (disabled) — plus the two product-surface tools that
/// drive real subsystems:
///   * `browser` ([`BrowserTool::new`]): standalone — the production
///     [`RealBackend`](crate::tools::handlers::browser::RealBackend) wraps the
///     `browser-use-browser` crate and manages CDP sessions internally (keyed by
///     `session_id`), so no external handle is threaded in. Registered
///     `parallel_safe = false` (a single CDP connection).
///   * `python` ([`PythonTool::with_backend`]): backed by the `python_backend`
///     this builder receives — a [`RealBackend`](crate::tools::handlers::python::RealBackend)
///     wrapping the ONE [`PythonWorker`] [`resolve_provider`] started for the run
///     (eager, matching legacy). Registered `parallel_safe = false` (a single
///     interpreter process).
///
/// `mcp` ([`McpTool::new`] takes an
/// [`McpClient`](crate::tools::handlers::mcp::McpClient)) is still NOT registered
/// here — it is handled separately and needs the MCP client manager threaded
/// through. A model call to `mcp` returns the registry's "unknown tool"
/// tool-result rather than reaching a default backend.
///
/// [`BrowserTool::new`]: crate::tools::handlers::browser::BrowserTool::new
/// [`PythonTool::with_backend`]: crate::tools::handlers::python::PythonTool::with_backend
/// [`McpTool::new`]: crate::tools::handlers::mcp::McpTool::new
/// [`PythonWorker`]: browser_use_python_worker::PythonWorker
///
/// ## Phase-E seams (honest defaults)
/// - **Approval policy = `Never`** + **`ToolOrchestrator::stub()`** (auto-approve,
///   `NoneSandboxProvider`): tools run unsandboxed and un-prompted. The richer
///   policy (real `AskForApproval` from config, a live `Approver`, the real sandbox
///   provider) is threaded by a later Phase-E WP — the seam is exactly this builder.
/// - **`TurnEnv`** carries an unrestricted filesystem policy and no managed network
///   / guardian, matching the `sandbox = None` initial scope.
/// - **`ToolCtx.cwd`** is the process cwd (best-effort); the per-call id/name are
///   placeholders the orchestrator does not key behavior on for the `Never`/stub path.
/// - **`supports_parallel_tool_calls = true`**: lets the registry's own per-tool
///   `parallel_safe` flag drive the parallel/serial gate (the conservative tools
///   are registered serial, so this is safe).
fn build_tool_dispatcher(
    python_backend: Arc<dyn PythonBackend>,
) -> Arc<ToolDispatcher<RegistryRunner>> {
    use crate::tools::handlers::apply_patch::{ApplyPatchRequest, ApplyPatchTool};
    use crate::tools::handlers::browser::{BrowserRequest, BrowserTool};
    use crate::tools::handlers::python::{PythonRequest, PythonTool};
    use crate::tools::handlers::request_user_input::{
        RequestUserInputRequest, RequestUserInputTool,
    };
    use crate::tools::handlers::shell::{ShellRequest, ShellTool};
    use crate::tools::handlers::tool_search::{ToolSearchRequest, ToolSearchTool};
    use crate::tools::handlers::update_plan::{UpdatePlanRequest, UpdatePlanTool};
    use crate::tools::handlers::view_image::{ViewImageRequest, ViewImageTool};
    use crate::tools::handlers::web_search::{WebSearchConfig, WebSearchRequest, WebSearchTool};
    use crate::tools::registry::{definitions, ToolRegistry};

    // The backend-free handlers, each with its parity-grounded definition + static
    // parallel_safe flag (matching `default_registry`'s presets), plus the
    // browser/python product tools. `mcp` is still absent (handled separately).
    let mut reg = ToolRegistry::new();
    reg.register::<_, ShellRequest>("shell", definitions::shell(), false, ShellTool::new());
    reg.register::<_, ApplyPatchRequest>(
        "apply_patch",
        definitions::apply_patch(),
        false,
        ApplyPatchTool::new(),
    );
    reg.register::<_, ViewImageRequest>(
        "view_image",
        definitions::view_image(),
        false,
        ViewImageTool::new(),
    );
    reg.register::<_, UpdatePlanRequest>(
        "update_plan",
        definitions::update_plan(),
        false,
        UpdatePlanTool::new(),
    );
    reg.register::<_, RequestUserInputRequest>(
        "request_user_input",
        definitions::request_user_input(),
        false,
        RequestUserInputTool::new(),
    );
    reg.register::<_, ToolSearchRequest>(
        "tool_search",
        definitions::tool_search(),
        true,
        ToolSearchTool::new(Vec::new()),
    );
    reg.register::<_, WebSearchRequest>(
        "web_search",
        definitions::web_search(),
        true,
        WebSearchTool::new(WebSearchConfig::disabled()),
    );
    // `browser`: standalone production backend (`browser-use-browser`, internal
    // session management). parallel_safe = false (single CDP connection).
    reg.register::<_, BrowserRequest>("browser", definitions::browser(), false, BrowserTool::new());
    // `python`: backed by the run's single PythonWorker (started eagerly by
    // `resolve_provider`). parallel_safe = false (single interpreter process).
    reg.register::<_, PythonRequest>(
        "python",
        definitions::python(),
        false,
        PythonTool::with_backend(python_backend),
    );

    // Capture the model-visible tool definitions BEFORE `reg` is moved into the
    // runner's Arc, so the dispatcher can carry them to the sampling driver (which
    // sets `LlmRequest::tools` from them). Same Vec the registry advertises
    // (name-sorted, order-stable) — without this the model receives no tool
    // definitions and can never emit browser/python/shell tool calls.
    let specs = reg.model_visible_definitions();

    let runner = RegistryRunner::new(
        Arc::new(reg),
        Arc::new(ToolOrchestrator::stub()),
        // Phase-E seam: placeholder per-turn ctx/env; the stub orchestrator +
        // `Never` policy do not key behavior on these (no approval prompt, no
        // sandbox). Wave-E threads the real ToolCtx/TurnEnv/policy through.
        ToolCtx {
            call_id: String::new(),
            tool_name: String::new(),
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        },
        TurnEnv {
            file_system_sandbox_policy: FileSystemSandboxPolicy {
                restricted: false,
                denied_read: false,
            },
            managed_network_active: false,
            strict_auto_review: false,
            use_guardian: false,
        },
        AskForApproval::Never,
    );

    Arc::new(ToolDispatcher::with_runner_and_specs(
        runner, /* supports_parallel_tool_calls */ true, specs,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_overrides::ProviderRunConfig;
    use crate::events::PendingEvent;

    struct NullSink;
    impl EventSink for NullSink {
        fn emit(&self, _ev: PendingEvent) {}
    }

    /// A no-op [`FusionRecorder`] for resolution tests (they assert the driver
    /// CONSTRUCTS offline — they never sample, so nothing is ever recorded).
    struct NullRecorder;
    #[async_trait::async_trait]
    impl FusionRecorder for NullRecorder {
        async fn record(&self, _messages: &[browser_use_llm::schema::Message]) {}
    }

    fn recorder() -> Arc<dyn FusionRecorder> {
        Arc::new(NullRecorder)
    }

    /// A fake Python backend so resolution tests never spawn a real worker
    /// (network/process free). It records nothing and is never `run` — these
    /// tests only assert the driver CONSTRUCTS, they do not dispatch.
    struct FakePythonBackend;
    impl crate::tools::handlers::python::PythonBackend for FakePythonBackend {
        fn run(
            &self,
            _session_id: &str,
            _cwd: &std::path::Path,
            _artifact_dir: &std::path::Path,
            _code: &str,
            _timeout_secs: Option<f64>,
        ) -> anyhow::Result<browser_use_python_worker::RunPythonResponse> {
            anyhow::bail!("fake python backend: run() not used in resolution tests")
        }
    }

    fn fake_python() -> Arc<dyn PythonBackend> {
        Arc::new(FakePythonBackend)
    }

    fn ctx() -> TurnCtx {
        TurnCtx {
            session_id: "prov-test".to_string(),
            model: "m".to_string(),
            provider: "p".to_string(),
            turn_idx: 0,
            attempt: 0,
        }
    }

    /// A real OpenAI backend CONSTRUCTS the live driver offline (no network). We
    /// inject the key via env for the duration of the test, then assert
    /// resolution yields a Real driver. The key is removed afterwards.
    ///
    /// We go through `resolve_provider_with_python` with an injected FAKE Python
    /// backend so the test never spawns a real Python worker subprocess (the
    /// public `resolve_provider` starts the real worker eagerly; that is exercised
    /// in production, not here). The offline-construction assertion is about the
    /// model transport, not the worker.
    #[test]
    fn resolves_real_openai_driver_offline() {
        // SAFETY: single-threaded test; we set + clear the var around the call.
        std::env::set_var("OPENAI_API_KEY", "sk-test-entrypoint");
        let config = ProviderRunConfig::new(ProviderBackend::Openai, "gpt-x");
        let resolved = resolve_provider_with_python(
            &config,
            Arc::new(NullSink),
            ctx(),
            3,
            recorder(),
            Some(fake_python()),
        )
        .expect("real openai driver must construct offline");
        std::env::remove_var("OPENAI_API_KEY");
        assert!(matches!(resolved, ResolvedProvider::Real(_)));
    }

    /// A real Anthropic backend also constructs offline given its key.
    #[test]
    fn resolves_real_anthropic_driver_offline() {
        std::env::set_var("ANTHROPIC_API_KEY", "ak-test-entrypoint");
        let config = ProviderRunConfig::new(ProviderBackend::Anthropic, "claude-x");
        let resolved = resolve_provider_with_python(
            &config,
            Arc::new(NullSink),
            ctx(),
            3,
            recorder(),
            Some(fake_python()),
        )
        .expect("real anthropic driver must construct offline");
        std::env::remove_var("ANTHROPIC_API_KEY");
        assert!(matches!(resolved, ResolvedProvider::Real(_)));
    }

    /// The fake backend resolves to the Fake signal (no real provider, no key).
    #[test]
    fn fake_backend_resolves_to_fake_signal() {
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let resolved = resolve_provider(&config, Arc::new(NullSink), ctx(), 3, recorder())
            .expect("fake must resolve");
        assert!(matches!(resolved, ResolvedProvider::Fake));
    }

    /// The cut codex backend surfaces a clear typed error (chatgpt.com stays cut).
    #[test]
    fn codex_backend_is_cut_with_typed_error() {
        let config = ProviderRunConfig::new(ProviderBackend::Codex, "codex-model");
        let err = resolve_provider(&config, Arc::new(NullSink), ctx(), 3, recorder())
            .expect_err("codex backend must be rejected");
        match err {
            ProviderResolveError::UnsupportedBackend(msg) => {
                assert!(msg.contains("codex"), "message should mention codex: {msg}");
            }
            other => panic!("expected UnsupportedBackend, got {other:?}"),
        }
    }

    /// A real backend with NO credentials in the env is an honest typed error,
    /// not a panic and never a silent default to codex.
    #[test]
    fn missing_credentials_is_typed_error() {
        // Ensure the relevant keys are unset for this backend.
        std::env::remove_var("OPENROUTER_API_KEY");
        std::env::remove_var("LLM_BROWSER_OPENAI_COMPAT_API_KEY");
        let config = ProviderRunConfig::new(ProviderBackend::Openrouter, "x");
        let err = resolve_provider(&config, Arc::new(NullSink), ctx(), 3, recorder())
            .expect_err("missing credentials must error");
        assert!(matches!(err, ProviderResolveError::MissingCredentials(_)));
    }

    // ---- browser/python tool wiring (network/process free) --------------------
    //
    // These prove the `browser` and `python` handlers are REGISTERED in the
    // production registry the dispatcher runs over, and REACHABLE through the real
    // dispatch path: a call deserializes into the typed Req, the runner looks the
    // handler up BY NAME (not "unknown tool"), and the FAKE backend's distinctive
    // marker flows out the rendered tool-result. We never start a real
    // PythonWorker or a real browser: browser uses a FAKE [`BrowserBackend`],
    // python a FAKE [`PythonBackend`]. Mirrors `browser_tests.rs` /
    // `python_tests.rs` (same orchestrator-driven seam) plus a registry
    // membership assertion.

    use crate::tools::handlers::browser::{BrowserBackend, BrowserRequest, BrowserTool};
    use crate::tools::handlers::python::{PythonRequest, PythonTool};
    use crate::tools::orchestrator::ToolOrchestrator;
    use crate::tools::registry::{definitions, ToolRegistry};
    use crate::tools::runtime::{AutoApprover, ToolCtx};
    use crate::tools::sandbox::{FileSystemSandboxPolicy, NoneSandboxProvider};
    use browser_use_browser::BrowserCommandOutput;
    use browser_use_python_worker::RunPythonResponse;

    /// A fake browser backend returning a marker on the `command` path (no real
    /// CDP/browser). Only `command` is exercised; the other methods are
    /// unreachable in these tests.
    struct MarkerBrowserBackend;
    impl BrowserBackend for MarkerBrowserBackend {
        fn command(
            &self,
            _session_id: &str,
            _cwd: &std::path::Path,
            _artifact_dir: &std::path::Path,
            _command: &str,
        ) -> anyhow::Result<BrowserCommandOutput> {
            Ok(BrowserCommandOutput {
                content: serde_json::json!({ "marker": "BROWSER_MARKER" }),
                events: vec![],
            })
        }
        fn run_script(
            &self,
            _session_id: &str,
            _cwd: &std::path::Path,
            _artifact_dir: &std::path::Path,
            _code: &str,
            _timeout_secs: u64,
        ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
            anyhow::bail!("run_script not used")
        }
        fn start_script(
            &self,
            _session_id: &str,
            _cwd: &std::path::Path,
            _artifact_dir: &std::path::Path,
            _code: &str,
            _timeout_secs: u64,
        ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
            anyhow::bail!("start_script not used")
        }
        fn observe_script(
            &self,
            _session_id: &str,
            _run_id: &str,
            _observe_timeout_ms: u64,
        ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
            anyhow::bail!("observe_script not used")
        }
        fn cancel_script(
            &self,
            _session_id: &str,
            _run_id: &str,
        ) -> anyhow::Result<browser_use_browser::BrowserScriptOutput> {
            anyhow::bail!("cancel_script not used")
        }
    }

    /// A fake python backend returning a marker output (no real worker/process).
    struct MarkerPythonBackend;
    impl crate::tools::handlers::python::PythonBackend for MarkerPythonBackend {
        fn run(
            &self,
            _session_id: &str,
            _cwd: &std::path::Path,
            _artifact_dir: &std::path::Path,
            _code: &str,
            _timeout_secs: Option<f64>,
        ) -> anyhow::Result<RunPythonResponse> {
            Ok(RunPythonResponse {
                id: "py-marker".to_string(),
                ok: true,
                text: "PYTHON_MARKER".to_string(),
                error: None,
                data: serde_json::Value::Null,
                outputs: vec![],
                artifacts: vec![],
                images: vec![],
                browser_events: vec![],
                browser_harness_available: false,
                browser_harness_error: None,
            })
        }
    }

    fn turn_env() -> TurnEnv {
        TurnEnv {
            file_system_sandbox_policy: FileSystemSandboxPolicy {
                restricted: false,
                denied_read: false,
            },
            managed_network_active: false,
            strict_auto_review: false,
            use_guardian: false,
        }
    }

    fn tool_ctx(name: &str) -> ToolCtx {
        ToolCtx {
            call_id: format!("call-{name}"),
            tool_name: name.to_string(),
            cwd: std::env::temp_dir(),
        }
    }

    /// `browser` and `python` are REGISTERED in the production registry (exactly
    /// as `build_tool_dispatcher` registers them) — proving they are no longer the
    /// Phase-E "unknown tool" seam.
    #[test]
    fn browser_and_python_are_registered() {
        // Default seams (`NoneSandboxProvider`/`AutoApprover`) — this registry is
        // only inspected for membership, never handed to a runner that would infer
        // the seams, so annotate them explicitly.
        let mut reg: ToolRegistry<NoneSandboxProvider, AutoApprover> = ToolRegistry::new();
        reg.register::<_, BrowserRequest>(
            "browser",
            definitions::browser(),
            false,
            BrowserTool::with_backend(Arc::new(MarkerBrowserBackend)),
        );
        reg.register::<_, PythonRequest>(
            "python",
            definitions::python(),
            false,
            PythonTool::with_backend(Arc::new(MarkerPythonBackend)),
        );
        assert!(reg.contains("browser"), "browser must be registered");
        assert!(reg.contains("python"), "python must be registered");
    }

    /// A `browser` call REACHES the injected backend through the orchestrator
    /// (same seam the dispatcher's runner uses) and the backend's marker flows
    /// into the rendered output — not a stub, not "unknown tool".
    #[tokio::test]
    async fn browser_dispatch_reaches_injected_backend() {
        let tool = BrowserTool::with_backend(Arc::new(MarkerBrowserBackend));
        let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
        let req = BrowserRequest::command("sess-1", "click");
        let result = orch
            .run(
                &tool,
                &req,
                &tool_ctx("browser"),
                &turn_env(),
                AskForApproval::Never,
            )
            .await
            .expect("browser orchestration ok");
        assert!(
            result.output.stdout.contains("BROWSER_MARKER"),
            "browser must reach the backend, got: {:?}",
            result.output
        );
    }

    /// A `python` call REACHES the injected backend through the orchestrator and
    /// the backend's marker flows into the rendered output.
    #[tokio::test]
    async fn python_dispatch_reaches_injected_backend() {
        let tool = PythonTool::with_backend(Arc::new(MarkerPythonBackend));
        let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
        let req = PythonRequest::new("print('x')");
        let result = orch
            .run(
                &tool,
                &req,
                &tool_ctx("python"),
                &turn_env(),
                AskForApproval::Never,
            )
            .await
            .expect("python orchestration ok");
        assert!(
            result.output.stdout.contains("PYTHON_MARKER"),
            "python must reach the backend, got: {:?}",
            result.output
        );
    }

    /// The PRODUCTION builder `build_tool_dispatcher` accepts the injected python
    /// backend and constructs a real dispatcher (proving the signature wiring) —
    /// exercised with a FAKE backend so no real worker is started.
    #[test]
    fn production_builder_accepts_injected_python_backend() {
        let _dispatcher: Arc<ToolDispatcher<RegistryRunner>> =
            build_tool_dispatcher(Arc::new(MarkerPythonBackend));
    }
}
