//! Credential helpers for on-disk login state.
//!
//! ## Scope note — codex backend is being CUT
//! The only thing here is the Codex (`~/.codex/auth.json`) reader, and it exists
//! **solely as a gated dev/test vehicle**: the codex/ChatGPT backend
//! (`chatgpt.com/backend-api`) is being removed from production, so this reader
//! is **not** a production code path and is **not** compiled into the default
//! build. It is behind the `codex-dev` Cargo feature.
//!
//! The production real-model path resolves credentials from standard provider
//! env keys (`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `LLM_BROWSER_*` / etc.) and
//! builds an [`crate::route::Route`] via the first-party [`crate::providers`]
//! facades (`OpenAi`, `Anthropic`, `OpenAiCompatible`). See
//! `browser-use-agent::turn::model_path`.

/// Codex `auth.json` reader — DEV/TEST ONLY (codex backend is being cut).
#[cfg(feature = "codex-dev")]
pub mod codex;

#[cfg(feature = "codex-dev")]
pub use codex::{
    codex_auth_path, load_codex_auth, load_codex_auth_file, AccountIdSource, CodexAuth,
    CodexAuthError,
};
