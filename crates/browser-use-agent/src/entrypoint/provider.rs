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
//!   * [`ProviderBackend::Codex`]       → [`ProviderChoice::Codex`]: the codex
//!     (chatgpt.com) backend, resolved from the Codex CLI OAuth login. The access
//!     token + account id come from env (`CODEX_ACCESS_TOKEN`/`CODEX_ACCOUNT_ID`),
//!     else the credential store (`auth.codex.*`), else `~/.codex/auth.json` (via
//!     [`browser_use_llm::auth::load_codex_auth`]).
//!   * [`ProviderBackend::None`]        → a clear typed error (no provider chosen).
//!
//! ## Credential resolution (env, then store)
//! API keys are resolved env-first, then from the [`Store`] settings the legacy
//! `auth login <provider> --api-key` command writes (`auth.<provider>.api_key`),
//! matching the legacy `stored_or_env` precedence. A missing credential surfaces
//! as [`ProviderResolveError::MissingCredentials`] (honest, never a panic).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use browser_use_llm::auth::{load_codex_auth, CodexAuth};
use browser_use_llm::route::ModelClient;
use browser_use_store::Store;

use crate::config_overrides::ProviderBackend;
use crate::config_overrides::ProviderRunConfig;
use crate::config_overrides::{
    ChildAgentCompletionHandler, ChildAgentRunCompletion, ChildAgentRunRequest, ChildAgentRunner,
};
use crate::events::EventSink;
use crate::events::PendingEvent;
use crate::events::TurnCtx;
use crate::guardian::approval::GuardianApprover;
use crate::guardian::reviewer::{GuardianReviewer, StaticReviewer};
use crate::guardian::Guardian;
use crate::mcp::McpConnectionManager;
use crate::session::{SessionId, SharedStore};
use crate::subagents::mailbox::Mailbox;
use crate::subagents::manager::{
    ChildHandle, ChildSpawner, ChildSpec, ParentContext, SubagentError, SubagentManager,
};
use crate::subagents::parent_link::{update_parent_from_child_run, ChildRunOutcome};
use crate::subagents::registry::AgentRegistry;
use crate::subagents::role::AgentConfigLayer;
use crate::tools::approval::AskForApproval;
use crate::tools::handlers::mcp::{McpClient, McpTool};
use crate::tools::handlers::python::PythonBackend;
use crate::tools::handlers::request_user_input::StoreRoundTripResponder;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::ToolCtx;
use crate::tools::sandbox::{FileSystemSandboxPolicy, NoneSandboxProvider};
use crate::tools::UnifiedExecManager;
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
///
/// The runner's approver is the REAL [`GuardianApprover`] (over a permissive
/// [`NoneSandboxProvider`] — OS sandboxing is intentionally NOT enforced here).
/// The active [`AskForApproval`] policy (from the run config) decides whether the
/// approver is consulted at all: `Never` auto-approves without a prompt, any other
/// policy routes each gated call through the guardian review.
pub type RealSamplingDriver = ModelSamplingDriver<
    ModelClientTransport,
    RegistryRunner<NoneSandboxProvider, GuardianApprover>,
>;

/// The production tool dispatcher type: a [`RegistryRunner`] whose approver is the
/// REAL [`GuardianApprover`] (permissive sandbox seam). Named so the builder + the
/// fused driver agree on the runner's generic arguments.
pub type RealToolDispatcher = ToolDispatcher<RegistryRunner<NoneSandboxProvider, GuardianApprover>>;

static UNIFIED_EXEC_MANAGERS: OnceLock<Mutex<HashMap<String, UnifiedExecManager>>> =
    OnceLock::new();

fn unified_exec_managers() -> &'static Mutex<HashMap<String, UnifiedExecManager>> {
    UNIFIED_EXEC_MANAGERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn unified_exec_manager_for_session(session_id: Option<&SessionId>) -> UnifiedExecManager {
    let Some(session_id) = session_id else {
        return UnifiedExecManager::default();
    };
    let key = session_id.as_str().to_string();
    let Ok(mut managers) = unified_exec_managers().lock() else {
        return UnifiedExecManager::default();
    };
    managers.entry(key).or_default().clone()
}

pub fn cleanup_unified_exec_manager_for_session_id(session_id: &str) -> usize {
    let manager = unified_exec_managers()
        .lock()
        .ok()
        .and_then(|mut managers| managers.remove(session_id));
    manager
        .map(|manager| manager.terminate_all_best_effort())
        .unwrap_or(0)
}

pub fn cleanup_all_unified_exec_managers() -> usize {
    let managers = unified_exec_managers()
        .lock()
        .ok()
        .map(|mut managers| {
            managers
                .drain()
                .map(|(_, manager)| manager)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    managers
        .into_iter()
        .map(|manager| manager.terminate_all_best_effort())
        .sum()
}

#[cfg(test)]
struct NoopEventSink;

#[cfg(test)]
impl EventSink for NoopEventSink {
    fn emit(&self, _ev: PendingEvent) {}
}

/// Errors resolving a provider into a driver.
#[derive(Debug)]
pub enum ProviderResolveError {
    /// The configured backend has no real provider in this engine.
    ///
    /// Carries a human-readable reason. `None` means no backend was selected.
    UnsupportedBackend(String),
    /// No usable credential was found (env or store) for the chosen backend.
    MissingCredentials(&'static str),
    /// The model route could not be built (e.g. an unknown OpenAI-compatible
    /// provider id). Wraps the real [`ModelPathError`].
    Route(ModelPathError),
    /// The codex (chatgpt.com) login state exists but could not be resolved
    /// (e.g. a malformed `~/.codex/auth.json`). Carries the underlying message
    /// (never the token/file contents — the codex reader keeps secrets out of its
    /// error strings).
    Codex(String),
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
            ProviderResolveError::Codex(why) => {
                write!(f, "failed to resolve codex login: {why}")
            }
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

/// Read a non-empty value from the [`Store`] settings, ignoring read errors
/// (a store read failure should not block an otherwise-resolvable env credential;
/// it degrades to "no stored value").
fn store_first(store: Option<&Store>, key: &str) -> Option<String> {
    store?
        .get_setting(key)
        .ok()
        .flatten()
        .filter(|v| !v.trim().is_empty())
}

/// Resolve a provider API key env-first, then from the [`Store`] settings the
/// legacy `auth login <provider> --api-key` command writes (`auth.<provider>.api_key`).
///
/// Precedence matches the legacy `stored_or_env`: env wins, store is the fallback.
fn key_env_then_store(
    env_keys: &[&str],
    store: Option<&Store>,
    store_provider: &str,
) -> Option<String> {
    env_first(env_keys).or_else(|| store_first(store, &format!("auth.{store_provider}.api_key")))
}

/// Map a [`ProviderBackend`] to a model-path [`ProviderChoice`], resolving the
/// backend's credentials env-first, then from the [`Store`] settings.
///
/// `store` is the (optional) credential store: when an env key is absent, the
/// stored `auth.<provider>.api_key` (and codex tokens) are consulted, matching the
/// legacy `auth login` write path. Pass `None` to resolve from env only.
///
/// Returns:
///   * `Ok(Some(choice))` for a real provider with credentials present,
///   * `Ok(None)` for [`ProviderBackend::Fake`] (no real provider),
///   * `Err(..)` for an absent backend or missing credentials.
///
/// Only file I/O is the codex `~/.codex/auth.json` fallback (and the store read);
/// no network I/O — this only assembles a [`ProviderChoice`].
pub fn provider_choice_for_backend(
    backend: ProviderBackend,
    store: Option<&Store>,
) -> Result<Option<ProviderChoice>, ProviderResolveError> {
    match backend {
        ProviderBackend::Openai => {
            let api_key = key_env_then_store(
                &["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"],
                store,
                "openai",
            )
            .ok_or(ProviderResolveError::MissingCredentials(
                "set OPENAI_API_KEY (or run `auth login openai`) for the openai backend",
            ))?;
            Ok(Some(ProviderChoice::OpenAiResponses {
                api_key,
                base_url: env_first(&["LLM_BROWSER_OPENAI_BASE_URL"]),
            }))
        }
        ProviderBackend::Anthropic => {
            let api_key = key_env_then_store(
                &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
                store,
                "anthropic",
            )
            .ok_or(ProviderResolveError::MissingCredentials(
                "set ANTHROPIC_API_KEY (or run `auth login anthropic`) for the anthropic backend",
            ))?;
            Ok(Some(ProviderChoice::Anthropic {
                api_key,
                base_url: env_first(&["LLM_BROWSER_ANTHROPIC_BASE_URL"]),
            }))
        }
        ProviderBackend::Openrouter => {
            let api_key = key_env_then_store(
                &["OPENROUTER_API_KEY", "LLM_BROWSER_OPENAI_COMPAT_API_KEY"],
                store,
                "openrouter",
            )
            .ok_or(ProviderResolveError::MissingCredentials(
                "set OPENROUTER_API_KEY (or run `auth login openrouter`) for the openrouter backend",
            ))?;
            Ok(Some(ProviderChoice::OpenAiCompatibleProvider {
                provider_id: "openrouter".to_string(),
                api_key,
            }))
        }
        ProviderBackend::Deepseek => {
            let api_key = key_env_then_store(
                &["DEEPSEEK_API_KEY", "LLM_BROWSER_OPENAI_COMPAT_API_KEY"],
                store,
                "deepseek",
            )
            .ok_or(ProviderResolveError::MissingCredentials(
                "set DEEPSEEK_API_KEY (or run `auth login deepseek`) for the deepseek backend",
            ))?;
            Ok(Some(ProviderChoice::OpenAiCompatibleProvider {
                provider_id: "deepseek".to_string(),
                api_key,
            }))
        }
        ProviderBackend::Fake => Ok(None),
        // Codex (chatgpt.com) login: resolve the OAuth access token + account id
        // from env, then the store, then `~/.codex/auth.json`.
        ProviderBackend::Codex => Ok(Some(resolve_codex_choice(store)?)),
        ProviderBackend::None => Err(ProviderResolveError::UnsupportedBackend(
            "no provider backend selected".to_string(),
        )),
    }
}

/// Resolve the codex (chatgpt.com) [`ProviderChoice`] from, in precedence order:
///   1. env `CODEX_ACCESS_TOKEN` + `CODEX_ACCOUNT_ID`,
///   2. the credential store (`auth.codex.access_token` + `auth.codex.account_id`),
///   3. the on-disk Codex CLI login `~/.codex/auth.json` (via [`load_codex_auth`]).
///
/// The base url honours `CODEX_BASE_URL` (env), else the stored `auth.codex.base_url`,
/// else the chatgpt.com default baked into the route builder.
fn resolve_codex_choice(store: Option<&Store>) -> Result<ProviderChoice, ProviderResolveError> {
    let base_url =
        env_first(&["CODEX_BASE_URL"]).or_else(|| store_first(store, "auth.codex.base_url"));

    // (1) explicit env token + account.
    if let (Some(access_token), Some(account_id)) = (
        env_first(&["CODEX_ACCESS_TOKEN"]),
        env_first(&["CODEX_ACCOUNT_ID"]),
    ) {
        return Ok(ProviderChoice::Codex {
            access_token,
            account_id,
            base_url,
        });
    }

    // (2) store-resolved token + account.
    if let (Some(access_token), Some(account_id)) = (
        store_first(store, "auth.codex.access_token"),
        store_first(store, "auth.codex.account_id"),
    ) {
        return Ok(ProviderChoice::Codex {
            access_token,
            account_id,
            base_url,
        });
    }

    // (3) on-disk Codex CLI login (`~/.codex/auth.json`).
    match load_codex_auth() {
        Ok(Some(CodexAuth {
            access_token,
            account_id,
        })) => Ok(ProviderChoice::Codex {
            access_token,
            account_id,
            base_url,
        }),
        // No login present anywhere → honest missing-credentials error.
        Ok(None) => Err(ProviderResolveError::MissingCredentials(
            "no codex login found: run `auth import-codex` or log in with the Codex CLI \
             (~/.codex/auth.json), or set CODEX_ACCESS_TOKEN + CODEX_ACCOUNT_ID",
        )),
        // A present-but-malformed auth.json is a typed route/resolution error.
        Err(e) => Err(ProviderResolveError::Codex(e.to_string())),
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
/// `store` is the (optional) credential store threaded through for env-then-store
/// key resolution (the caller's `SharedStore`); pass `None` to resolve from env
/// only. `sink` receives the driver's UI events; `ctx` carries the turn's
/// model/provider identity; `max_retries` is the codex-style stream retry budget;
/// `recorder` is the [`FusionRecorder`] the fused driver records the assistant
/// message + dispatched tool outputs through (it must point at the SAME
/// conversation buffer the loop's `TurnState` re-samples from). No network I/O
/// happens here.
pub fn resolve_provider(
    config: &ProviderRunConfig,
    store: Option<&Store>,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    max_retries: u32,
    recorder: Arc<dyn FusionRecorder>,
    user_input: Option<(SharedStore, SessionId)>,
) -> Result<ResolvedProvider, ProviderResolveError> {
    let tool_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    resolve_provider_with_tool_cwd(
        config,
        store,
        sink,
        ctx,
        max_retries,
        recorder,
        user_input,
        tool_cwd,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_provider_with_tool_cwd(
    config: &ProviderRunConfig,
    store: Option<&Store>,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    max_retries: u32,
    recorder: Arc<dyn FusionRecorder>,
    user_input: Option<(SharedStore, SessionId)>,
    tool_cwd: std::path::PathBuf,
) -> Result<ResolvedProvider, ProviderResolveError> {
    resolve_provider_with_tool_paths(
        config,
        store,
        sink,
        ctx,
        max_retries,
        recorder,
        user_input,
        tool_cwd.clone(),
        tool_cwd,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_provider_with_tool_paths(
    config: &ProviderRunConfig,
    store: Option<&Store>,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    max_retries: u32,
    recorder: Arc<dyn FusionRecorder>,
    user_input: Option<(SharedStore, SessionId)>,
    tool_cwd: std::path::PathBuf,
    tool_artifact_root: std::path::PathBuf,
) -> Result<ResolvedProvider, ProviderResolveError> {
    // The Fake short-circuit lives in the inner builder (so we never spawn a
    // Python worker for a fake/cut/missing-credential run). For a real backend we
    // start the run's single Python worker EAGERLY here, then thread its backend
    // through. `start_python_backend` only runs once we know the route builds.
    //
    // `user_input` is the (SharedStore, SessionId) the production `request_user_input`
    // responder round-trips through (Some on the live run path, None for tests /
    // headless callers — which fall back to the Echo auto-responder).
    resolve_provider_with_python(
        config,
        store,
        sink,
        ctx,
        max_retries,
        recorder,
        None,
        user_input,
        tool_cwd,
        tool_artifact_root,
    )
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
#[allow(clippy::too_many_arguments)]
fn resolve_provider_with_python(
    config: &ProviderRunConfig,
    store: Option<&Store>,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    max_retries: u32,
    recorder: Arc<dyn FusionRecorder>,
    python_backend: Option<Arc<dyn PythonBackend>>,
    user_input: Option<(SharedStore, SessionId)>,
    tool_cwd: std::path::PathBuf,
    tool_artifact_root: std::path::PathBuf,
) -> Result<ResolvedProvider, ProviderResolveError> {
    // (1) backend → credentialed provider choice (env-then-store creds; codex from
    //     env/store/~/.codex; None → Err; Fake → None).
    let choice = match provider_choice_for_backend(config.backend, store)? {
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
    let driver = build_sampling_driver(transport, Arc::clone(&sink), ctx, max_retries).with_fusion(
        build_tool_dispatcher_with_cwd(
            python_backend,
            config,
            user_input,
            tool_cwd,
            tool_artifact_root,
            sink,
        ),
        recorder,
    );
    Ok(ResolvedProvider::Real(Box::new(driver)))
}

/// Build the production fused tool dispatcher: a [`ToolRegistry`] behind the REAL
/// [`RegistryRunner`], over a REAL [`ToolOrchestrator`] (permissive
/// [`NoneSandboxProvider`] + a live [`GuardianApprover`]), wrapped in a
/// [`ToolDispatcher`].
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
/// `view_image`, `update_plan`, `request_user_input` (default
/// [`EchoAutoResponder`](crate::tools::handlers::request_user_input::EchoAutoResponder)),
/// `done`, `tool_search` (catalog populated from the registered tools' defs),
/// `web_search` (ENABLED; the Responses builder encodes it as the hosted
/// `web_search_preview` tool) — plus the two product-surface tools that drive
/// real subsystems:
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
/// [`McpClient`](crate::tools::handlers::mcp::McpClient)) is registered ONLY when
/// the run config supplies one or more `mcp_servers`: this builder connects them
/// via [`McpConnectionManager::connect_all`] (per-server failure isolation) and
/// registers the `mcp` tool over the resulting manager. An EMPTY `mcp_servers`
/// map (the default) registers nothing, preserving prior behavior — a model call
/// to `mcp` then returns the registry's "unknown tool" tool-result.
///
/// [`BrowserTool::new`]: crate::tools::handlers::browser::BrowserTool::new
/// [`PythonTool::with_backend`]: crate::tools::handlers::python::PythonTool::with_backend
/// [`McpTool::new`]: crate::tools::handlers::mcp::McpTool::new
/// [`PythonWorker`]: browser_use_python_worker::PythonWorker
///
/// ## Approval / guardian wiring (LIVE)
/// - **Approval policy** is sourced from `config.options.approval_policy` (default
///   [`AskForApproval::Never`], preserving prior non-interactive behavior) and
///   threaded into the [`RegistryRunner`]. `Never` auto-approves (no prompt); any
///   other policy routes each gated call through the REAL [`GuardianApprover`],
///   which can deny.
/// - **Orchestrator** is [`build_real_orchestrator`] over a permissive
///   [`NoneSandboxProvider`] (OS-level sandbox enforcement is intentionally
///   SKIPPED — a permissive seam) + a live [`GuardianApprover`]. The guardian's
///   reviewer is selected by `config.options.use_guardian` (deny vs. allow).
/// - **`ToolCtx.cwd`** is the process cwd (best-effort); the per-call id/name are
///   placeholders for this headless dispatch path.
/// - **`supports_parallel_tool_calls = true`**: lets the registry's own per-tool
///   `parallel_safe` flag drive the parallel/serial gate (the conservative tools
///   are registered serial, so this is safe).
#[cfg(test)]
fn build_tool_dispatcher(
    python_backend: Arc<dyn PythonBackend>,
    config: &ProviderRunConfig,
    user_input: Option<(SharedStore, SessionId)>,
) -> Arc<RealToolDispatcher> {
    let tool_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    build_tool_dispatcher_with_cwd(
        python_backend,
        config,
        user_input,
        tool_cwd.clone(),
        tool_cwd,
        Arc::new(NoopEventSink),
    )
}

fn build_tool_dispatcher_with_cwd(
    python_backend: Arc<dyn PythonBackend>,
    config: &ProviderRunConfig,
    user_input: Option<(SharedStore, SessionId)>,
    tool_cwd: std::path::PathBuf,
    tool_artifact_root: std::path::PathBuf,
    event_sink: Arc<dyn EventSink>,
) -> Arc<RealToolDispatcher> {
    use crate::tools::handlers::apply_patch::{ApplyPatchRequest, ApplyPatchTool};
    use crate::tools::handlers::browser::{BrowserRequest, BrowserTool};
    use crate::tools::handlers::capture::{CaptureCurationRequest, CaptureCurationTool};
    use crate::tools::handlers::done::{DoneRequest, DoneTool};
    use crate::tools::handlers::mcp::McpToolCallRequest;
    use crate::tools::handlers::python::{PythonRequest, PythonTool};
    use crate::tools::handlers::request_user_input::{
        RequestUserInputRequest, RequestUserInputTool,
    };
    use crate::tools::handlers::shell::{
        ExecCommandRequest, ExecCommandTool, ShellRequest, ShellTool, WriteStdinRequest,
        WriteStdinTool,
    };
    use crate::tools::handlers::tool_search::{ToolSearchEntry, ToolSearchRequest, ToolSearchTool};
    use crate::tools::handlers::update_plan::{UpdatePlanRequest, UpdatePlanTool};
    use crate::tools::handlers::view_image::{ViewImageRequest, ViewImageTool};
    use crate::tools::handlers::web_search::{WebSearchConfig, WebSearchRequest, WebSearchTool};
    use crate::tools::registry::{definitions, ToolRegistry};

    // The backend-free handlers, each with its parity-grounded definition + static
    // parallel_safe flag (matching `default_registry`'s presets), plus the
    // browser/python product tools. `mcp` is still absent (handled separately).
    // Typed with the production approver (`GuardianApprover`) so the registry, the
    // orchestrator, and the runner all agree on the `(S, A)` seams.
    let mut reg: ToolRegistry<NoneSandboxProvider, GuardianApprover> = ToolRegistry::new();
    let unified_exec =
        unified_exec_manager_for_session(user_input.as_ref().map(|(_, session_id)| session_id));
    let unified_exec_emitter = user_input.as_ref().map(|(_, session_id)| {
        Arc::new(crate::tools::UnifiedExecEventEmitter::new(
            Arc::clone(&event_sink),
            session_id.as_str().to_string(),
        ))
    });
    let shell_tool = match &unified_exec_emitter {
        Some(emitter) => {
            ShellTool::with_manager(unified_exec.clone()).with_event_emitter(Arc::clone(emitter))
        }
        None => ShellTool::with_manager(unified_exec.clone()),
    };
    let exec_command_tool = match &unified_exec_emitter {
        Some(emitter) => {
            ExecCommandTool::new(unified_exec.clone()).with_event_emitter(Arc::clone(emitter))
        }
        None => ExecCommandTool::new(unified_exec.clone()),
    };
    let write_stdin_tool = match &unified_exec_emitter {
        Some(emitter) => {
            WriteStdinTool::new(unified_exec.clone()).with_event_emitter(Arc::clone(emitter))
        }
        None => WriteStdinTool::new(unified_exec.clone()),
    };
    reg.register::<_, ShellRequest>("shell", definitions::shell(), false, shell_tool);
    reg.register::<_, ExecCommandRequest>(
        "exec_command",
        definitions::exec_command(),
        true,
        exec_command_tool,
    );
    reg.register::<_, WriteStdinRequest>(
        "write_stdin",
        definitions::write_stdin(),
        false,
        write_stdin_tool,
    );
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
    // `request_user_input`: production path round-trips the question/answer
    // through the session store so the TUI (or any consumer of the
    // `request_user_input.requested` / `.response` control-channel events) can
    // surface it to the operator and deliver the answer back. With no store /
    // session available (tests, headless), fall back to the Echo auto-responder.
    let request_user_input_tool = match &user_input {
        Some((store, session_id)) => RequestUserInputTool::with_responder(Arc::new(
            StoreRoundTripResponder::new(store.clone(), session_id.clone()),
        )),
        None => RequestUserInputTool::new(),
    };
    reg.register::<_, RequestUserInputRequest>(
        "request_user_input",
        definitions::request_user_input(),
        false,
        request_user_input_tool,
    );
    // `web_search` is ENABLED (hosted/provider-side). The OpenAI Responses
    // request builder encodes it as the hosted `{"type":"web_search_preview"}`
    // tool (see `browser-use-llm` `openai_responses.rs::lower_tool`).
    reg.register::<_, WebSearchRequest>(
        "web_search",
        definitions::web_search(),
        true,
        WebSearchTool::new(WebSearchConfig::enabled()),
    );
    let browser_tool = match &user_input {
        Some((store, session_id)) => {
            BrowserTool::with_browser_mode(config.options.browser_mode.clone())
                .with_session_id(session_id.as_str().to_string())
                .with_persistence(store.clone(), session_id.as_str().to_string())
        }
        None => BrowserTool::with_browser_mode(config.options.browser_mode.clone()),
    };
    // `browser`: standalone production backend (`browser-use-browser`, internal
    // session management). parallel_safe = false (single CDP connection).
    reg.register::<_, BrowserRequest>(
        "browser",
        definitions::browser(),
        false,
        browser_tool.clone(),
    );
    // `browser_script`: browser-use's page/data-plane surface. It routes through
    // the same handler, but the schema omits the internal session id and matches
    // the prompt contract used by current-main browser tasks.
    reg.register::<_, BrowserRequest>(
        "browser_script",
        definitions::browser_script(),
        false,
        browser_tool,
    );
    if let Some((store, session_id)) = &user_input {
        reg.register::<_, CaptureCurationRequest>(
            "submit_capture_curation",
            definitions::submit_capture_curation(),
            false,
            CaptureCurationTool::with_store(store.clone(), session_id.as_str().to_string()),
        );
    }
    // `python`: backed by the run's single PythonWorker (started eagerly by
    // `resolve_provider`). parallel_safe = false (single interpreter process).
    reg.register::<_, PythonRequest>(
        "python",
        definitions::python(),
        false,
        PythonTool::with_backend(python_backend),
    );
    // `done`: the completion tool the model calls to declare it has finished, with
    // its final summary. Serial (terminal; must not be reordered).
    reg.register::<_, DoneRequest>("done", definitions::done(), false, DoneTool::new());

    // Subagent orchestration tools (`spawn_agent` / `wait_agent` / `send_input` /
    // `list_agents`). All four share ONE `SubagentManager` (live-agent registry +
    // EVENT-NOTIFY mailbox + depth enforcement + the `ChildSpawner` seam). The
    // child spawner is bridged from the run config's `child_agent_runner` so
    // spawned children inherit the parent's provider/model; when none is configured
    // the spawner returns an honest "subagents not configured" error rather than
    // silently dropping the tools (the model still SEES the tools).
    register_subagent_tools(&mut reg, config, &user_input);

    // Goal tools (`get_goal` / `create_goal` / `update_goal`). All three share ONE
    // `GoalStore` (the event-sourced `GoalManager` + its durable `goal.*` event
    // sink), registered behind the same registry seam so a `create_goal` is
    // visible to a later `get_goal`/`update_goal`. The model always SEES the tools
    // (no config gate), matching how the subagent tools are always advertised.
    register_goal_tools(&mut reg, &user_input);

    // `mcp`: register the MCP bridge ONLY when servers are configured. An empty
    // `mcp_servers` map (the default) registers nothing, preserving prior
    // behavior. Non-empty => connect all servers (per-server failure isolation
    // inside `connect_all`) and register the single `mcp` tool over the resulting
    // manager (which implements `McpClient`). Registered `parallel_safe = false`;
    // the handler's per-request read-only hint still drives its own gate.
    if !config.options.mcp_servers.is_empty() {
        match McpConnectionManager::connect_all(config.options.mcp_servers.clone()) {
            Ok((manager, errors)) => {
                for (server, err) in &errors {
                    eprintln!("warning: MCP server '{server}' failed to connect: {err}");
                }
                let client: Arc<dyn McpClient> = Arc::new(manager);
                reg.register::<_, McpToolCallRequest>(
                    "mcp",
                    definitions::mcp(),
                    false,
                    McpTool::new(client),
                );
            }
            // A runtime-build failure (rare) drops the `mcp` tool rather than
            // aborting the whole run; a model call to `mcp` then returns "unknown
            // tool". The other tools are unaffected.
            Err(e) => eprintln!("warning: failed to start MCP runtime, mcp tool disabled: {e}"),
        }
    }

    // `tool_search` catalog: populate it from the registry's model-visible
    // definitions so the model can discover the registered tools by free-text
    // query (legacy `deferred_tool_search_entries`; codex's `ToolSearchInfo`
    // catalog). We use the registered tools' definitions (name + description +
    // schema property names) as the searchable catalog — the obvious in-crate
    // source. (When a deferred MCP / dynamic-tool source lands, it extends this
    // catalog; for now the registered tools are the catalog. See REPORT.) We
    // register tool_search LAST so the catalog reflects every other tool, then
    // mirror the same entries as the registry's deferred set.
    let catalog: Vec<ToolSearchEntry> = reg
        .model_visible_definitions()
        .iter()
        .map(|def| {
            let props: Vec<String> = def
                .input_schema
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            ToolSearchEntry::new(def.name.clone(), def.description.clone(), props)
        })
        .collect();
    reg.set_deferred_search_entries(catalog.clone());
    reg.register::<_, ToolSearchRequest>(
        "tool_search",
        definitions::tool_search(),
        true,
        ToolSearchTool::new(catalog),
    );

    // Capture the model-visible tool definitions BEFORE `reg` is moved into the
    // runner's Arc, so the dispatcher can carry them to the sampling driver (which
    // sets `LlmRequest::tools` from them). Same Vec the registry advertises
    // (name-sorted, order-stable) — without this the model receives no tool
    // definitions and can never emit browser/python/shell tool calls.
    let specs = reg.model_visible_definitions();

    // The REAL approval policy, sourced from the run config (default `Never`,
    // which preserves prior non-interactive behavior).
    let policy: AskForApproval = config.options.approval_policy;
    // The REAL orchestrator: permissive `NoneSandboxProvider` (OS sandboxing is
    // intentionally skipped) + a live `GuardianApprover`. The approver is only
    // consulted when the policy routes to it (any non-`Never` policy on a gated
    // call); under `Never` the orchestrator bypasses the prompt entirely.
    let orchestrator = Arc::new(build_real_orchestrator(config.options.use_guardian));

    let runner = RegistryRunner::new(
        Arc::new(reg),
        orchestrator,
        // Per-turn ctx/env. The cwd is the durable session cwd supplied by the
        // caller; falling back to the process cwd only in test/headless helper paths.
        ToolCtx {
            call_id: user_input
                .as_ref()
                .map(|(_, session_id)| session_id.as_str().to_string())
                .unwrap_or_default(),
            tool_name: String::new(),
            cwd: tool_cwd,
            artifact_root: tool_artifact_root,
        },
        TurnEnv {
            file_system_sandbox_policy: FileSystemSandboxPolicy {
                restricted: false,
                denied_read: false,
            },
            managed_network_active: false,
            strict_auto_review: false,
            // Mirror the run config's guardian toggle for parity.
            use_guardian: config.options.use_guardian,
        },
        policy,
    );

    Arc::new(ToolDispatcher::with_runner_and_specs(
        runner, /* supports_parallel_tool_calls */ true, specs,
    ))
}

/// Build the production tool orchestrator: a permissive [`NoneSandboxProvider`]
/// (OS-level sandbox enforcement is intentionally SKIPPED — this is a permissive
/// seam) paired with a live [`GuardianApprover`].
///
/// `use_guardian` selects the guardian's reviewer:
///   * `false` → [`StaticReviewer::allow`] — the permissive default. Under a
///     non-`Never` policy a gated call is reviewed and ALLOWED (the approver IS
///     consulted — the routing is live — it just permits).
///   * `true`  → [`StaticReviewer::deny`] — fail-closed. Under a non-`Never`
///     policy a gated call is reviewed and DENIED, proving the policy routes to a
///     real approver that can block.
///
/// Under [`AskForApproval::Never`] the orchestrator's pure decision table bypasses
/// the approval gate entirely, so the reviewer is never consulted regardless of
/// this flag — preserving the prior auto-approve behavior.
fn build_real_orchestrator(
    use_guardian: bool,
) -> ToolOrchestrator<NoneSandboxProvider, GuardianApprover> {
    let reviewer: Arc<dyn GuardianReviewer> = if use_guardian {
        Arc::new(StaticReviewer::deny(
            "guardian denied: non-interactive run with the guardian gate enabled",
        ))
    } else {
        Arc::new(StaticReviewer::allow())
    };
    let approver = GuardianApprover::new(Guardian::new(reviewer));
    ToolOrchestrator::new(NoneSandboxProvider, approver)
}

/// Bridges the run config's [`ChildAgentRunner`] (a `Fn(ChildAgentRunRequest) ->
/// Result<()>` that *launches* a child) into the subagents [`ChildSpawner`] seam.
///
/// The legacy `child_agent_runner` is fire-and-forget: it spawns the child run
/// (inheriting the parent's provider/model via the request fields) and returns.
/// The subagents [`SubagentManager`] tracks the child's lifecycle through the
/// registry + mailbox, so this adapter maps the [`ChildSpec`] onto a
/// [`ChildAgentRunRequest`], invokes the runner, and returns a [`ChildHandle`]
/// for the just-launched child. A runner error becomes a [`SubagentError`].
struct ChildAgentRunnerSpawner {
    runner: ChildAgentRunner,
    parent_session_id: String,
    parent_link: Arc<Mutex<Option<ChildAgentRunnerParentLink>>>,
}

#[derive(Clone)]
struct ChildAgentRunnerParentLink {
    registry: Arc<AgentRegistry>,
    mailbox: Arc<Mailbox>,
}

#[async_trait::async_trait]
impl ChildSpawner for ChildAgentRunnerSpawner {
    async fn spawn_child(&self, spec: ChildSpec) -> Result<ChildHandle, SubagentError> {
        let completion_handler = self
            .parent_link
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .map(|parent_link| {
                let child_path = spec.agent_path.clone();
                ChildAgentCompletionHandler::new(move |completion: ChildAgentRunCompletion| {
                    let outcome = if completion.success {
                        ChildRunOutcome::success(completion.summary)
                    } else {
                        ChildRunOutcome::failure(
                            completion
                                .summary
                                .unwrap_or_else(|| "child agent failed".to_string()),
                        )
                    };
                    let _ = update_parent_from_child_run(
                        &parent_link.registry,
                        &parent_link.mailbox,
                        &child_path,
                        &outcome,
                    );
                    Ok(())
                })
            });
        let request = ChildAgentRunRequest {
            parent_session_id: self.parent_session_id.clone(),
            // The child's session id is its canonical agent id (unique per spawn).
            child_session_id: spec.agent_id.clone(),
            message: spec.message.clone(),
            agent_path: Some(spec.agent_path.clone()),
            nickname: spec.nickname.clone(),
            role: spec.role.clone(),
            fork_turns: spec.fork_turns.clone(),
            // Child inherits the resolved config (provider/tier folded into the
            // role layer); surface the model + reasoning/tier overrides the
            // legacy runner consumes.
            model: Some(spec.config.model.clone()),
            reasoning_effort: spec.config.reasoning_effort.clone(),
            service_tier: spec.config.service_tier.clone(),
            config_overrides: Vec::new(),
            completion_handler,
        };
        self.runner
            .run(request)
            .map_err(|e| SubagentError(format!("child_agent_runner failed: {e}")))?;
        Ok(ChildHandle {
            agent_path: spec.agent_path,
            agent_id: spec.agent_id,
        })
    }
}

/// A [`ChildSpawner`] that always errors: the fallback when the run config
/// supplies no `child_agent_runner`. Spawning then returns an honest "subagents
/// not configured" error rather than the tools being silently absent — the model
/// still SEES `spawn_agent` (so it can attempt delegation) but gets a clear
/// failure when no runner is wired (e.g. headless/test runs).
struct UnconfiguredChildSpawner;

#[async_trait::async_trait]
impl ChildSpawner for UnconfiguredChildSpawner {
    async fn spawn_child(&self, _spec: ChildSpec) -> Result<ChildHandle, SubagentError> {
        Err(SubagentError(
            "subagents are not configured for this run (no child_agent_runner)".to_string(),
        ))
    }
}

/// A durable [`EventSink`] over a [`SharedStore`]: appends each `subagent.*`
/// lifecycle event under the shared lock so the TUI's subagent render sees the
/// transition. Best-effort (append errors are swallowed, matching
/// [`EventSink::emit`]'s infallible contract).
struct SubagentStoreSink {
    store: SharedStore,
}

impl EventSink for SubagentStoreSink {
    fn emit(&self, ev: PendingEvent) {
        if let Ok(store) = self.store.lock() {
            let _ = store.append_event(&ev.session_id, &ev.event_type, ev.payload);
        }
    }
}

/// A no-op [`EventSink`] for runs without a session store (tests / headless):
/// lifecycle events are dropped, but spawn/wait/send still function.
struct NoopSubagentSink;

impl EventSink for NoopSubagentSink {
    fn emit(&self, _ev: PendingEvent) {}
}

/// Register the four subagent orchestration tools (`spawn_agent`, `wait_agent`,
/// `send_input`, `list_agents`) into `reg`, all sharing ONE [`SubagentManager`].
///
/// The manager's [`ChildSpawner`] is bridged from
/// `config.options.child_agent_runner` via [`ChildAgentRunnerSpawner`]; spawned
/// children therefore inherit the parent's provider/model from whatever the
/// entrypoint wired into that runner. When the run config carries no runner,
/// [`UnconfiguredChildSpawner`] is used so a spawn attempt fails honestly.
/// Lifecycle events are persisted through a store-backed [`SubagentStoreSink`]
/// when a session store is available (the live run path), else dropped via
/// [`NoopSubagentSink`] (tests/headless).
fn register_subagent_tools<S, A>(
    reg: &mut crate::tools::registry::ToolRegistry<S, A>,
    config: &ProviderRunConfig,
    user_input: &Option<(SharedStore, SessionId)>,
) where
    S: crate::tools::sandbox::SandboxProvider,
    A: crate::tools::runtime::Approver,
{
    use crate::subagents::spawn::SpawnAgentArgs;
    use crate::tools::handlers::subagent::{
        ListAgentsRequest, ListAgentsTool, SendInputRequest, SendInputTool, SpawnAgentTool,
        SubagentToolDeps, WaitAgentRequest, WaitAgentTool,
    };
    use crate::tools::registry::definitions;

    let parent_session_id = user_input
        .as_ref()
        .map(|(_, sid)| sid.as_str().to_string())
        .unwrap_or_default();

    // The child-runner seam (parent's provider/model inheritance) or an honest
    // error fallback when no runner is configured.
    let parent_link = Arc::new(Mutex::new(None));
    let spawner: Arc<dyn ChildSpawner> = match &config.options.child_agent_runner {
        Some(runner) => Arc::new(ChildAgentRunnerSpawner {
            runner: runner.clone(),
            parent_session_id,
            parent_link: Arc::clone(&parent_link),
        }),
        None => Arc::new(UnconfiguredChildSpawner),
    };
    let manager = Arc::new(SubagentManager::new(spawner));
    *parent_link
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ChildAgentRunnerParentLink {
        registry: manager.registry(),
        mailbox: manager.mailbox(),
    });

    // The parent context the children hang off: `/root`, depth 0, with the run's
    // model + provider as the base config so role layering preserves them.
    let parent = ParentContext {
        agent_path: "/root".to_string(),
        depth: 0,
        base_config: AgentConfigLayer::base(config.model.clone(), provider_label(config)),
    };

    // Durable lifecycle sink + session scope: the store-backed sink on the live
    // run path, a no-op when no session store is wired.
    let (sink, session_id): (Arc<dyn EventSink>, String) = match user_input {
        Some((store, sid)) => (
            Arc::new(SubagentStoreSink {
                store: store.clone(),
            }),
            sid.as_str().to_string(),
        ),
        None => (Arc::new(NoopSubagentSink), String::new()),
    };

    let deps = SubagentToolDeps {
        manager,
        parent,
        sink,
        session_id,
    };

    // spawn_agent: parallel-safe (each spawn mints a handle + hands off; the child
    // runs on its own task).
    reg.register::<_, SpawnAgentArgs>(
        "spawn_agent",
        definitions::spawn_agent(),
        true,
        SpawnAgentTool::new(deps.clone()),
    );
    // wait_agent: parallel-safe (blocks on the shared mailbox + reads the registry).
    reg.register::<_, WaitAgentRequest>(
        "wait_agent",
        definitions::wait_agent(),
        true,
        WaitAgentTool::new(deps.clone()),
    );
    // send_input: parallel-safe (enqueues onto the mailbox).
    reg.register::<_, SendInputRequest>(
        "send_input",
        definitions::send_input(),
        true,
        SendInputTool::new(deps.clone()),
    );
    // list_agents: parallel-safe (read-only registry snapshot).
    reg.register::<_, ListAgentsRequest>(
        "list_agents",
        definitions::list_agents(),
        true,
        ListAgentsTool::new(deps),
    );
}

/// Register the goal tool family (`get_goal`, `create_goal`, `update_goal`) into
/// `reg`, all sharing ONE [`GoalStore`].
///
/// The store wraps a [`GoalManager`](crate::goals::GoalManager) whose injected
/// [`EventSink`] persists durable `goal.*` events: on the live run path it is the
/// store-backed [`SubagentStoreSink`] (the same `crate::events::EventSink` the
/// subagent tools use — it appends each event to the session's durable log so the
/// TUI render / resume-by-replay observe `goal.created` / `goal.updated`), and in
/// tests/headless it is the no-op [`NoopSubagentSink`]. `create_goal` (and
/// budget-threshold crossings) emit through the manager's sink automatically;
/// `update_goal` emits `goal.updated` from its handler.
///
/// Mirrors [`register_subagent_tools`]: a shared store + a store-backed event sink
/// + the session id from the threaded `(SharedStore, SessionId)`.
fn register_goal_tools<S, A>(
    reg: &mut crate::tools::registry::ToolRegistry<S, A>,
    user_input: &Option<(SharedStore, SessionId)>,
) where
    S: crate::tools::sandbox::SandboxProvider,
    A: crate::tools::runtime::Approver,
{
    use crate::tools::handlers::goal::{
        CreateGoalRequest, CreateGoalTool, GetGoalRequest, GetGoalTool, GoalStore,
        UpdateGoalRequest, UpdateGoalTool,
    };
    use crate::tools::registry::definitions;

    // Durable event sink + session scope: the store-backed sink on the live run
    // path (durable `goal.*` events appended to the session log), a no-op when no
    // session store is wired (tests/headless). Reuses the subagent tools' sinks
    // (both are `crate::events::EventSink`).
    let (sink, session_id): (Arc<dyn EventSink>, String) = match user_input {
        Some((store, sid)) => (
            Arc::new(SubagentStoreSink {
                store: store.clone(),
            }),
            sid.as_str().to_string(),
        ),
        None => (Arc::new(NoopSubagentSink), String::new()),
    };

    // One shared store so create_goal is visible to a later get/update_goal.
    let store = Arc::new(GoalStore::new(session_id, sink));

    // get_goal: read-only snapshot (parallel-safe).
    reg.register::<_, GetGoalRequest>(
        "get_goal",
        definitions::get_goal(),
        true,
        GetGoalTool::new(store.clone()),
    );
    // create_goal: mutates the shared state (serial).
    reg.register::<_, CreateGoalRequest>(
        "create_goal",
        definitions::create_goal(),
        false,
        CreateGoalTool::new(store.clone()),
    );
    // update_goal: mutates the shared state (serial).
    reg.register::<_, UpdateGoalRequest>(
        "update_goal",
        definitions::update_goal(),
        false,
        UpdateGoalTool::new(store),
    );
}

/// Best-effort wire-provider label for the child's base config (mirrors the
/// entrypoint's backend->label derivation).
fn provider_label(config: &ProviderRunConfig) -> String {
    format!("{:?}", config.backend).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_overrides::ProviderRunConfig;
    use crate::events::PendingEvent;
    use std::sync::Mutex;

    /// Serializes tests that mutate process env (`set_var`/`remove_var`). Cargo
    /// runs tests in a binary in parallel, so unsynchronized env mutation across
    /// these credential tests would race; this lock keeps them serial.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("OPENAI_API_KEY", "sk-test-entrypoint");
        let config = ProviderRunConfig::new(ProviderBackend::Openai, "gpt-x");
        let resolved = resolve_provider_with_python(
            &config,
            None,
            Arc::new(NullSink),
            ctx(),
            3,
            recorder(),
            Some(fake_python()),
            None,
            std::env::temp_dir(),
            std::env::temp_dir().join("artifacts"),
        )
        .expect("real openai driver must construct offline");
        std::env::remove_var("OPENAI_API_KEY");
        assert!(matches!(resolved, ResolvedProvider::Real(_)));
    }

    /// A real Anthropic backend also constructs offline given its key.
    #[test]
    fn resolves_real_anthropic_driver_offline() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("ANTHROPIC_API_KEY", "ak-test-entrypoint");
        let config = ProviderRunConfig::new(ProviderBackend::Anthropic, "claude-x");
        let resolved = resolve_provider_with_python(
            &config,
            None,
            Arc::new(NullSink),
            ctx(),
            3,
            recorder(),
            Some(fake_python()),
            None,
            std::env::temp_dir(),
            std::env::temp_dir().join("artifacts"),
        )
        .expect("real anthropic driver must construct offline");
        std::env::remove_var("ANTHROPIC_API_KEY");
        assert!(matches!(resolved, ResolvedProvider::Real(_)));
    }

    /// The fake backend resolves to the Fake signal (no real provider, no key).
    #[test]
    fn fake_backend_resolves_to_fake_signal() {
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let resolved = resolve_provider(
            &config,
            None,
            Arc::new(NullSink),
            ctx(),
            3,
            recorder(),
            None,
        )
        .expect("fake must resolve");
        assert!(matches!(resolved, ResolvedProvider::Fake));
    }

    /// The codex backend is a REAL provider again: with env codex creds present it
    /// resolves a live driver offline (no network), targeting chatgpt.com.
    #[test]
    fn codex_backend_resolves_real_driver_from_env() {
        // Serialize with the other env-mutating tests: this sets CODEX_* vars,
        // and `codex_backend_resolves_choice_from_store` clears them, so without a
        // shared lock the two race (a flake surfaced under parallel test runs).
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("CODEX_ACCESS_TOKEN", "codex-access-test");
        std::env::set_var("CODEX_ACCOUNT_ID", "codex-acct-test");
        let config = ProviderRunConfig::new(ProviderBackend::Codex, "gpt-5.1-codex");
        let resolved = resolve_provider_with_python(
            &config,
            None,
            Arc::new(NullSink),
            ctx(),
            3,
            recorder(),
            Some(fake_python()),
            None,
            std::env::temp_dir(),
            std::env::temp_dir().join("artifacts"),
        );
        std::env::remove_var("CODEX_ACCESS_TOKEN");
        std::env::remove_var("CODEX_ACCOUNT_ID");
        assert!(matches!(
            resolved.expect("codex driver must construct offline"),
            ResolvedProvider::Real(_)
        ));
    }

    /// The codex backend also resolves its OAuth creds from the Store
    /// (`auth.codex.access_token` / `auth.codex.account_id`) when env is absent —
    /// proving the store-fallback path for codex.
    #[test]
    fn codex_backend_resolves_choice_from_store() {
        // Serialize with the env-setting codex test (see ENV_LOCK note there):
        // both touch CODEX_* process env, so they must not run concurrently.
        let _guard = ENV_LOCK.lock().unwrap();
        // Env codex creds must be absent for this to exercise the store path.
        std::env::remove_var("CODEX_ACCESS_TOKEN");
        std::env::remove_var("CODEX_ACCOUNT_ID");
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("store");
        store
            .set_setting("auth.codex.access_token", "stored-codex-access")
            .unwrap();
        store
            .set_setting("auth.codex.account_id", "stored-codex-acct")
            .unwrap();
        let choice = provider_choice_for_backend(ProviderBackend::Codex, Some(&store))
            .expect("codex resolves")
            .expect("codex is a real provider");
        match choice {
            ProviderChoice::Codex {
                access_token,
                account_id,
                ..
            } => {
                assert_eq!(access_token, "stored-codex-access");
                assert_eq!(account_id, "stored-codex-acct");
            }
            other => panic!("expected codex choice, got {other:?}"),
        }
    }

    /// A real backend with NO credentials in env AND none in the store is an honest
    /// typed error, not a panic.
    #[test]
    fn missing_credentials_is_typed_error() {
        // Ensure the relevant keys are unset for this backend.
        std::env::remove_var("OPENROUTER_API_KEY");
        std::env::remove_var("LLM_BROWSER_OPENAI_COMPAT_API_KEY");
        let config = ProviderRunConfig::new(ProviderBackend::Openrouter, "x");
        let err = resolve_provider(
            &config,
            None,
            Arc::new(NullSink),
            ctx(),
            3,
            recorder(),
            None,
        )
        .expect_err("missing credentials must error");
        assert!(matches!(err, ProviderResolveError::MissingCredentials(_)));
    }

    /// Env wins over the store: a provider key present in BOTH resolves to the env
    /// value (legacy `stored_or_env` precedence).
    #[test]
    fn env_key_wins_over_store() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("OPENAI_API_KEY", "env-openai-key");
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("store");
        store
            .set_setting("auth.openai.api_key", "stored-openai-key")
            .unwrap();
        let choice = provider_choice_for_backend(ProviderBackend::Openai, Some(&store))
            .expect("resolves")
            .expect("real provider");
        std::env::remove_var("OPENAI_API_KEY");
        match choice {
            ProviderChoice::OpenAiResponses { api_key, .. } => {
                assert_eq!(api_key, "env-openai-key", "env must win over store");
            }
            other => panic!("expected openai choice, got {other:?}"),
        }
    }

    /// Store is the fallback: with the env key absent, the stored
    /// `auth.<provider>.api_key` resolves the provider (fixes the env-only regression).
    #[test]
    fn store_key_is_fallback_when_env_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("LLM_BROWSER_ANTHROPIC_API_KEY");
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("store");
        store
            .set_setting("auth.anthropic.api_key", "stored-anthropic-key")
            .unwrap();
        let choice = provider_choice_for_backend(ProviderBackend::Anthropic, Some(&store))
            .expect("resolves")
            .expect("real provider");
        match choice {
            ProviderChoice::Anthropic { api_key, .. } => {
                assert_eq!(api_key, "stored-anthropic-key");
            }
            other => panic!("expected anthropic choice, got {other:?}"),
        }
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
            artifact_root: std::env::temp_dir().join("artifacts"),
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
        // No MCP servers, no user-input store -> mcp tool absent, Echo responder.
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let _dispatcher: Arc<RealToolDispatcher> =
            build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
    }

    /// An empty `mcp_servers` map registers NO `mcp` tool (prior behavior).
    #[test]
    fn empty_mcp_servers_registers_no_mcp_tool() {
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !names.contains(&"mcp"),
            "no mcp tool without configured servers"
        );
        // The other core tools are still present.
        assert!(names.contains(&"browser"));
        assert!(names.contains(&"done"));
    }

    /// A non-empty `mcp_servers` map registers the `mcp` tool. The stdio server
    /// command (`true`) connects to nothing useful, but `connect_all`'s per-server
    /// failure isolation still yields a manager and the registration wiring
    /// surfaces the `mcp` tool in the dispatcher's specs.
    #[test]
    fn nonempty_mcp_servers_registers_mcp_tool() {
        use crate::mcp::{McpServerConfig, McpServerTransport};
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "echo".to_string(),
            McpServerConfig {
                transport: McpServerTransport::Stdio {
                    command: "true".to_string(),
                    args: Vec::new(),
                    env: std::collections::HashMap::new(),
                    cwd: None,
                },
                startup_timeout_ms: Some(200),
                tool_timeout_ms: Some(200),
                enabled_tools: None,
                disabled_tools: None,
            },
        );
        let options = crate::config_overrides::AgentRunOptions {
            mcp_servers: servers,
            ..crate::config_overrides::AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            names.contains(&"mcp"),
            "mcp tool must be registered when servers are configured; got {names:?}"
        );
    }

    /// The production dispatcher ALWAYS advertises the four subagent
    /// orchestration tools (no config gate — the model can always attempt
    /// delegation; a spawn fails honestly when no child runner is wired). This is
    /// the "engine B matches rival A on the subagents row" registration proof.
    #[test]
    fn subagent_tools_are_registered_in_the_dispatcher() {
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        for tool in ["spawn_agent", "wait_agent", "send_input", "list_agents"] {
            assert!(
                names.contains(&tool),
                "{tool} must be registered in the production dispatcher; got {names:?}"
            );
        }
    }

    /// A `spawn_agent` call routes from the production registration into the
    /// `SubagentManager` and reaches the configured `child_agent_runner`, then
    /// returns the child's handle. This exercises the same registration
    /// `register_subagent_tools` installs, dispatched by NAME through the registry
    /// (the path the production `RegistryRunner` uses for a model tool-call), with
    /// a recording runner so it is offline.
    #[tokio::test]
    async fn spawn_agent_routes_through_registration_to_child_runner() {
        use crate::config_overrides::{AgentRunOptions, ChildAgentRunner};
        use crate::tools::orchestrator::ToolOrchestrator;
        use crate::tools::registry::ToolRegistry;
        use crate::tools::runtime::ToolCtx;
        use crate::tools::sandbox::FileSystemSandboxPolicy;
        use std::sync::atomic::{AtomicBool, Ordering};

        // A fire-and-forget child runner that records it was invoked and asserts
        // the child inherits the parent's model on the request (standing in for the
        // real task-driver-backed runner the live entrypoint wires).
        static INVOKED: AtomicBool = AtomicBool::new(false);
        INVOKED.store(false, Ordering::SeqCst);
        let runner = ChildAgentRunner::new(|req| {
            assert_eq!(req.model.as_deref(), Some("fake-model"));
            INVOKED.store(true, Ordering::SeqCst);
            Ok(())
        });
        let options = AgentRunOptions {
            child_agent_runner: Some(runner),
            ..AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);

        // Build the production registration into a registry, then dispatch a
        // spawn_agent call BY NAME through it (the same `dispatch` the production
        // RegistryRunner calls), under the auto-approve stub orchestrator.
        let mut reg: ToolRegistry<NoneSandboxProvider, AutoApprover> = ToolRegistry::new();
        register_subagent_tools(&mut reg, &config, &None);
        assert!(reg.contains("spawn_agent"));

        let orch = ToolOrchestrator::stub();
        let env = TurnEnv {
            file_system_sandbox_policy: FileSystemSandboxPolicy {
                restricted: false,
                denied_read: false,
            },
            managed_network_active: false,
            strict_auto_review: false,
            use_guardian: false,
        };
        let ctx = ToolCtx {
            call_id: "c".to_string(),
            tool_name: "spawn_agent".to_string(),
            cwd: std::env::temp_dir(),
            artifact_root: std::env::temp_dir().join("artifacts"),
        };
        let out = reg
            .dispatch(
                "spawn_agent",
                &serde_json::json!({ "task_name": "explore", "message": "go" }),
                &ctx,
                &env,
                AskForApproval::Never,
                &orch,
            )
            .await
            .expect("spawn_agent should route and run");
        assert_eq!(out.exit_code, 0, "spawn should succeed: {out:?}");
        assert!(
            INVOKED.load(Ordering::SeqCst),
            "the configured child runner must be invoked"
        );
        let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
        assert!(
            body.get("agent_path").and_then(|v| v.as_str()).is_some(),
            "spawn returns the child handle: {body}"
        );
    }

    #[tokio::test]
    async fn child_runner_completion_wakes_wait_agent() {
        use crate::config_overrides::{AgentRunOptions, ChildAgentRunCompletion, ChildAgentRunner};
        use crate::tools::orchestrator::ToolOrchestrator;
        use crate::tools::registry::ToolRegistry;
        use crate::tools::runtime::ToolCtx;
        use crate::tools::sandbox::FileSystemSandboxPolicy;

        let runner = ChildAgentRunner::new(|req| {
            req.completion_handler
                .as_ref()
                .expect("completion handler wired")
                .notify(ChildAgentRunCompletion::success(Some(
                    "child finished".to_string(),
                )))?;
            Ok(())
        });
        let options = AgentRunOptions {
            child_agent_runner: Some(runner),
            ..AgentRunOptions::default()
        };
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);

        let mut reg: ToolRegistry<NoneSandboxProvider, AutoApprover> = ToolRegistry::new();
        register_subagent_tools(&mut reg, &config, &None);

        let orch = ToolOrchestrator::stub();
        let env = TurnEnv {
            file_system_sandbox_policy: FileSystemSandboxPolicy {
                restricted: false,
                denied_read: false,
            },
            managed_network_active: false,
            strict_auto_review: false,
            use_guardian: false,
        };
        let spawn_ctx = ToolCtx {
            call_id: "spawn".to_string(),
            tool_name: "spawn_agent".to_string(),
            cwd: std::env::temp_dir(),
            artifact_root: std::env::temp_dir().join("artifacts"),
        };
        let spawn_out = reg
            .dispatch(
                "spawn_agent",
                &serde_json::json!({ "task_name": "explore", "message": "go" }),
                &spawn_ctx,
                &env,
                AskForApproval::Never,
                &orch,
            )
            .await
            .expect("spawn_agent should run");
        let body: serde_json::Value = serde_json::from_str(&spawn_out.stdout).unwrap();
        let agent_path = body
            .get("agent_path")
            .and_then(|v| v.as_str())
            .expect("agent path");

        let wait_ctx = ToolCtx {
            call_id: "wait".to_string(),
            tool_name: "wait_agent".to_string(),
            cwd: std::env::temp_dir(),
            artifact_root: std::env::temp_dir().join("artifacts"),
        };
        let wait_out = reg
            .dispatch(
                "wait_agent",
                &serde_json::json!({ "agent_path": agent_path, "timeout_secs": 1 }),
                &wait_ctx,
                &env,
                AskForApproval::Never,
                &orch,
            )
            .await
            .expect("wait_agent should run");
        let wait_body: serde_json::Value = serde_json::from_str(&wait_out.stdout).unwrap();
        assert_eq!(wait_body["status"], "completed");
        assert_eq!(wait_body["timed_out"], false);
    }

    /// The production dispatcher ALWAYS advertises the three goal tools
    /// (`get_goal` / `create_goal` / `update_goal`) — the "engine B matches rival
    /// A on the goals row" registration proof. Passes ONLY because
    /// `register_goal_tools` is invoked inside `build_tool_dispatcher`.
    #[test]
    fn goal_tools_are_registered_in_the_dispatcher() {
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);
        let names: Vec<&str> = dispatcher
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        for tool in ["get_goal", "create_goal", "update_goal"] {
            assert!(
                names.contains(&tool),
                "{tool} must be registered in the production dispatcher; got {names:?}"
            );
        }
    }

    /// A `create_goal` call routes from the PRODUCTION registration through the
    /// dispatcher (BY NAME via `dispatch_ordered`, the same path the turn loop
    /// uses for a model tool-call) into the shared `GoalStore`: a follow-up
    /// `get_goal` observes the goal, AND a durable `goal.created` event lands in
    /// the session store (proving the store-backed event sink is wired, not just
    /// the tool listed). `update_goal` then emits a durable `goal.updated`.
    #[tokio::test]
    async fn create_goal_routes_into_store_and_emits_durable_event() {
        use browser_use_llm::schema::{ContentPart, MessageRole};
        use browser_use_store::Store;
        use tokio_util::sync::CancellationToken;

        // Dispatch a single tool call BY NAME and return its JSON tool output.
        async fn dispatch_one(
            dispatcher: &RealToolDispatcher,
            name: &str,
            input: serde_json::Value,
        ) -> serde_json::Value {
            let call = ContentPart::ToolCall {
                id: format!("call-{name}"),
                name: name.to_string(),
                input,
                provider_metadata: None,
            };
            let result = dispatcher
                .dispatch_ordered(vec![call], CancellationToken::new())
                .await;
            let msg = result
                .outputs_in_order
                .into_iter()
                .next()
                .expect("one tool output");
            assert_eq!(msg.role, MessageRole::Tool);
            let text = msg
                .content
                .iter()
                .find_map(|p| match p {
                    ContentPart::ToolResult {
                        content, is_error, ..
                    } => {
                        assert!(!*is_error, "tool call must succeed: {content:?}");
                        content.iter().find_map(|c| match c {
                            ContentPart::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                    }
                    _ => None,
                })
                .expect("a tool-result text part");
            serde_json::from_str(&text).expect("tool output is JSON")
        }

        // Read every persisted `event_type` for the session from the store.
        fn event_types(store: &SharedStore, session_id: &str) -> Vec<String> {
            store
                .lock()
                .unwrap()
                .events_for_session(session_id)
                .expect("read events")
                .into_iter()
                .map(|e| e.event_type)
                .collect()
        }

        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        // Real durable store on a tempdir; create the session ROW first (the
        // store MINTS the id) so the store-backed sink's `append_event`
        // satisfies the events FK on `sessions(id)`.
        let dir = tempfile::tempdir().expect("tempdir");
        let store: SharedStore = Arc::new(std::sync::Mutex::new(
            Store::open(dir.path()).expect("open store"),
        ));
        let session_id = store
            .lock()
            .unwrap()
            .create_session(None, dir.path())
            .expect("create session row")
            .id;
        let session = SessionId(session_id.clone());
        let dispatcher = build_tool_dispatcher(
            Arc::new(MarkerPythonBackend),
            &config,
            Some((store.clone(), session.clone())),
        );

        // create_goal routes into the goal store and returns the folded snapshot.
        let created = dispatch_one(
            &dispatcher,
            "create_goal",
            serde_json::json!({"text": "ship the goals row", "token_budget": 1000}),
        )
        .await;
        assert_eq!(created["active"], true);
        assert_eq!(created["text"], "ship the goals row");
        assert_eq!(created["token_budget"], 1000);

        // get_goal observes the SAME shared state (the store is shared).
        let fetched = dispatch_one(&dispatcher, "get_goal", serde_json::json!({})).await;
        assert_eq!(fetched["text"], "ship the goals row");
        assert_eq!(fetched["active"], true);

        // A durable `goal.created` event landed for this session (proving the
        // store-backed sink — not just the tool listing — is wired).
        let kinds = event_types(&store, &session_id);
        assert!(
            kinds.iter().any(|k| k == "goal.created"),
            "expected a durable goal.created event, got: {kinds:?}"
        );

        // update_goal folds + emits a durable `goal.updated` event.
        let updated = dispatch_one(
            &dispatcher,
            "update_goal",
            serde_json::json!({"status": "complete"}),
        )
        .await;
        assert_eq!(updated["status"], "complete");
        let kinds = event_types(&store, &session_id);
        assert!(
            kinds.iter().any(|k| k == "goal.updated"),
            "expected a durable goal.updated event, got: {kinds:?}"
        );
    }

    // ---- approval-policy routing (the LIVE approval/guardian path) -------------
    //
    // These prove the production dispatcher HONORS `config.options.approval_policy`,
    // routing each gated tool call through the orchestrator's REAL `GuardianApprover`
    // (no OS sandbox — permissive seam). They dispatch a `shell` `echo` call BY NAME
    // through the SAME `dispatch_ordered` path the turn loop uses, asserting the
    // policy decides whether the approver is consulted / can deny.

    /// Dispatch a single tool call through the production dispatcher and return
    /// the recorded tool-result `(text, is_error)`.
    async fn dispatch_call(
        dispatcher: &RealToolDispatcher,
        name: &str,
        input: serde_json::Value,
    ) -> (String, bool) {
        use browser_use_llm::schema::{ContentPart, MessageRole};
        use tokio_util::sync::CancellationToken;

        let call = ContentPart::ToolCall {
            id: format!("call-{name}"),
            name: name.to_string(),
            input,
            provider_metadata: None,
        };
        let result = dispatcher
            .dispatch_ordered(vec![call], CancellationToken::new())
            .await;
        let msg = result
            .outputs_in_order
            .into_iter()
            .next()
            .expect("one tool output");
        assert_eq!(msg.role, MessageRole::Tool);
        msg.content
            .iter()
            .find_map(|p| match p {
                ContentPart::ToolResult {
                    content, is_error, ..
                } => {
                    let text = content
                        .iter()
                        .find_map(|c| match c {
                            ContentPart::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    Some((text, *is_error))
                }
                _ => None,
            })
            .expect("a tool-result part")
    }

    #[tokio::test]
    async fn production_dispatcher_uses_supplied_session_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let dispatcher = build_tool_dispatcher_with_cwd(
            Arc::new(MarkerPythonBackend),
            &config,
            None,
            dir.path().to_path_buf(),
            dir.path().join("artifacts"),
            Arc::new(NoopEventSink),
        );

        let (text, is_error) = dispatch_call(
            &dispatcher,
            "shell",
            serde_json::json!({ "command": ["pwd"] }),
        )
        .await;
        assert!(!is_error, "pwd should run successfully: {text}");
        assert!(
            text.contains(&dir.path().display().to_string()),
            "tool cwd must be the supplied session cwd, got: {text}"
        );
    }

    #[tokio::test]
    async fn production_exec_command_calls_overlap() {
        use browser_use_llm::schema::{ContentPart, MessageRole};
        use std::time::{Duration, Instant};
        use tokio_util::sync::CancellationToken;

        let dir = tempfile::tempdir().expect("tempdir");
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let dispatcher = build_tool_dispatcher_with_cwd(
            Arc::new(MarkerPythonBackend),
            &config,
            None,
            dir.path().to_path_buf(),
            dir.path().join("artifacts"),
            Arc::new(NoopEventSink),
        );

        let call = |id: &str, label: &str| ContentPart::ToolCall {
            id: id.to_string(),
            name: "exec_command".to_string(),
            input: serde_json::json!({
                "command": ["sh", "-c", format!("sleep 1; printf {label}")],
                "yield_time_ms": 1500,
            }),
            provider_metadata: None,
        };

        let started = Instant::now();
        let result = dispatcher
            .dispatch_ordered(
                vec![call("call-exec-a", "first"), call("call-exec-b", "second")],
                CancellationToken::new(),
            )
            .await;
        let elapsed = started.elapsed();

        assert_eq!(result.outputs_in_order.len(), 2);
        assert!(
            elapsed < Duration::from_millis(1800),
            "two 1s exec_command calls should overlap; elapsed={elapsed:?}"
        );

        for (msg, expected) in result.outputs_in_order.iter().zip(["first", "second"]) {
            assert_eq!(msg.role, MessageRole::Tool);
            let (text, is_error) = msg
                .content
                .iter()
                .find_map(|part| match part {
                    ContentPart::ToolResult {
                        content, is_error, ..
                    } => {
                        let text = content
                            .iter()
                            .find_map(|part| match part {
                                ContentPart::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        Some((text, *is_error))
                    }
                    _ => None,
                })
                .expect("tool result");
            assert!(!is_error, "exec_command failed: {text}");
            assert!(
                text.contains(expected),
                "exec_command output should contain {expected:?}, got {text:?}"
            );
        }
    }

    /// `AskForApproval::Never` (the default) AUTO-APPROVES: a gated `shell` call
    /// runs with NO prompt and NO denial — the approver is never consulted. This
    /// is the preserved-default-behavior proof.
    #[tokio::test]
    async fn never_policy_auto_approves_shell_call() {
        // Default config => approval_policy = Never, use_guardian = false.
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        assert_eq!(config.options.approval_policy, AskForApproval::Never);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);

        let (text, is_error) = dispatch_call(
            &dispatcher,
            "shell",
            serde_json::json!({ "command": ["echo", "hi"] }),
        )
        .await;
        assert!(
            !is_error,
            "Never policy must auto-approve (run the tool), got error: {text}"
        );
        assert!(
            text.contains("hi"),
            "the shell tool must have actually run under Never, got: {text}"
        );
    }

    /// A NON-`Never` policy ROUTES the gated call to the real approver, which —
    /// with the guardian gate enabled — DENIES it. Proves the policy is honored
    /// (the call is no longer hardcoded auto-approve) AND the approver can block.
    #[tokio::test]
    async fn non_never_policy_routes_to_approver_and_can_deny() {
        // UnlessTrusted always requires approval; use_guardian => the guardian's
        // reviewer denies, so the gated call is rejected.
        let options = crate::config_overrides::AgentRunOptions::default()
            .with_approval_policy(AskForApproval::UnlessTrusted)
            .with_guardian(true);
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);

        let (text, is_error) = dispatch_call(
            &dispatcher,
            "shell",
            serde_json::json!({ "command": ["echo", "hi"] }),
        )
        .await;
        assert!(
            is_error,
            "a non-Never policy with the guardian denying must reject the call; got ok: {text}"
        );
        assert!(
            text.contains("rejected"),
            "the rejection must surface the approver's denial, got: {text}"
        );
        assert!(
            !text.contains("hi"),
            "a denied shell call must NOT have executed, got: {text}"
        );
    }

    /// A NON-`Never` policy with the guardian DISABLED still routes to the real
    /// approver — which ALLOWS — so the gated call runs. This isolates "routing is
    /// live" from "guardian denies": the approver IS consulted (not bypassed), and
    /// the permissive reviewer permits.
    #[tokio::test]
    async fn non_never_policy_routes_to_approver_and_can_allow() {
        let options = crate::config_overrides::AgentRunOptions::default()
            .with_approval_policy(AskForApproval::UnlessTrusted)
            .with_guardian(false);
        let config =
            ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_options(options);
        let dispatcher = build_tool_dispatcher(Arc::new(MarkerPythonBackend), &config, None);

        let (text, is_error) = dispatch_call(
            &dispatcher,
            "shell",
            serde_json::json!({ "command": ["echo", "hi"] }),
        )
        .await;
        assert!(
            !is_error,
            "the permissive guardian must approve the routed call, got error: {text}"
        );
        assert!(
            text.contains("hi"),
            "the approved shell call must have executed, got: {text}"
        );
    }
}
