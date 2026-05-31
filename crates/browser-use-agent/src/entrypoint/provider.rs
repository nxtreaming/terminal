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
use crate::turn::model_path::build_route;
use crate::turn::model_path::build_sampling_driver;
use crate::turn::model_path::build_transport;
use crate::turn::model_path::ModelPathError;
use crate::turn::model_path::ProviderChoice;
use crate::turn::sampling::ModelClientTransport;
use crate::turn::sampling::ModelSamplingDriver;

/// The concrete real-backend sampling driver this facade builds.
///
/// [`build_sampling_driver`] returns `ModelSamplingDriver<ModelClientTransport>`
/// (the default-runner text-only sampler over a live transport); this alias names
/// it so the entrypoint can hold it without spelling the generics each time.
pub type RealSamplingDriver = ModelSamplingDriver<ModelClientTransport>;

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
/// identity; `max_retries` is the codex-style stream retry budget. No network I/O
/// happens here.
pub fn resolve_provider(
    config: &ProviderRunConfig,
    sink: Arc<dyn EventSink>,
    ctx: TurnCtx,
    max_retries: u32,
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

    // *** build_sampling_driver is actually CALLED here (production path). ***
    let driver = build_sampling_driver(transport, sink, ctx, max_retries);
    Ok(ResolvedProvider::Real(Box::new(driver)))
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
    #[test]
    fn resolves_real_openai_driver_offline() {
        // SAFETY: single-threaded test; we set + clear the var around the call.
        std::env::set_var("OPENAI_API_KEY", "sk-test-entrypoint");
        let config = ProviderRunConfig::new(ProviderBackend::Openai, "gpt-x");
        let resolved = resolve_provider(&config, Arc::new(NullSink), ctx(), 3)
            .expect("real openai driver must construct offline");
        std::env::remove_var("OPENAI_API_KEY");
        assert!(matches!(resolved, ResolvedProvider::Real(_)));
    }

    /// A real Anthropic backend also constructs offline given its key.
    #[test]
    fn resolves_real_anthropic_driver_offline() {
        std::env::set_var("ANTHROPIC_API_KEY", "ak-test-entrypoint");
        let config = ProviderRunConfig::new(ProviderBackend::Anthropic, "claude-x");
        let resolved = resolve_provider(&config, Arc::new(NullSink), ctx(), 3)
            .expect("real anthropic driver must construct offline");
        std::env::remove_var("ANTHROPIC_API_KEY");
        assert!(matches!(resolved, ResolvedProvider::Real(_)));
    }

    /// The fake backend resolves to the Fake signal (no real provider, no key).
    #[test]
    fn fake_backend_resolves_to_fake_signal() {
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model");
        let resolved =
            resolve_provider(&config, Arc::new(NullSink), ctx(), 3).expect("fake must resolve");
        assert!(matches!(resolved, ResolvedProvider::Fake));
    }

    /// The cut codex backend surfaces a clear typed error (chatgpt.com stays cut).
    #[test]
    fn codex_backend_is_cut_with_typed_error() {
        let config = ProviderRunConfig::new(ProviderBackend::Codex, "codex-model");
        let err = resolve_provider(&config, Arc::new(NullSink), ctx(), 3)
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
        let err = resolve_provider(&config, Arc::new(NullSink), ctx(), 3)
            .expect_err("missing credentials must error");
        assert!(matches!(err, ProviderResolveError::MissingCredentials(_)));
    }
}
