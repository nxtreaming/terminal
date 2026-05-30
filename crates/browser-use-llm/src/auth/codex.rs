//! Codex `auth.json` reader — **DEV/TEST VEHICLE ONLY (codex backend is cut).**
//!
//! The codex/ChatGPT backend (`chatgpt.com/backend-api`) is being **removed from
//! production**; we cannot rely on it. This module is therefore gated behind the
//! `codex-dev` Cargo feature and is **never** part of the default build or any
//! production code path. It exists only so a developer with a local codex login
//! can manually exercise the streaming stack end-to-end against a real backend
//! while the multi-provider production path (OpenAI / Anthropic / OpenAI-compat,
//! resolved from standard env keys) is the one that ships.
//!
//! The Codex CLI stores its OAuth credentials in `~/.codex/auth.json`:
//!
//! ```json
//! {
//!   "auth_mode": "chatgpt",
//!   "OPENAI_API_KEY": null,
//!   "tokens": {
//!     "id_token": "<jwt>",
//!     "access_token": "<jwt>",
//!     "refresh_token": "<opaque>",
//!     "account_id": "<uuid>"
//!   },
//!   "last_refresh": "2026-05-30T01:47:55.301041166Z"
//! }
//! ```
//!
//! For the ChatGPT-backed flow there is **no raw API key** (`OPENAI_API_KEY` is
//! `null`); the (now-cut) codex backend was reached with `Authorization: Bearer
//! <access_token>` plus the `chatgpt-account-id` header.
//!
//! This module is the honest, offline-testable parser: it reads the file, parses
//! the (subset of) fields we need, and resolves a [`CodexAuth`]
//! (`access_token` + `account_id`). It performs **no network I/O** and never
//! refreshes the token — see the refresh seam note below.
//!
//! ## Parity (legacy `browser-use-providers::load_codex_auth`)
//! - path: `$CODEX_HOME/auth.json`, else `$HOME/.codex/auth.json`. `CODEX_HOME`
//!   points *directly* at the `.codex` directory (not its parent).
//! - `access_token` = `tokens.access_token` (legacy also accepts a top-level
//!   `access_token`; we keep that fallback).
//! - `account_id` = `tokens.account_id`, else top-level `account_id` /
//!   `chatgpt_account_id`. The legacy resolver additionally decodes the
//!   `id_token` JWT's `chatgpt_account_id` claim as a last resort; on this
//!   machine `tokens.account_id` is always present, so we flag that JWT fallback
//!   as a follow-up seam ([`AccountIdSource`]) rather than pulling in a base64
//!   dependency here.
//!
//! ## Refresh (debt, not done here)
//! `auth.json` carries a `refresh_token` + `last_refresh`. When the
//! `access_token` is stale, the legacy path refreshes it via an OAuth POST to
//! `https://auth.openai.com/oauth/token` with the codex client id
//! `app_EMoamEEZ73f0CkXaXp7hrann`. This module deliberately does **not** refresh:
//! the access token in `auth.json` is normally still valid, so we use it
//! directly and surface `last_refresh` so a caller can decide. Wiring the refresh
//! POST is a clearly-scoped follow-up (it needs an async HTTP round-trip and the
//! client-id/url constants, which the legacy `browser-use-providers` crate owns).

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Resolved Codex credentials sufficient to talk to the ChatGPT-backed backend.
///
/// Carries the bearer `access_token` and the `account_id` sent as the
/// `chatgpt-account-id` header. Deliberately holds no other token material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexAuth {
    /// The OAuth access token, sent as `Authorization: Bearer <access_token>`.
    pub access_token: String,
    /// The ChatGPT account id, sent as the `chatgpt-account-id` header.
    pub account_id: String,
}

impl CodexAuth {
    /// Construct from the two load-bearing fields.
    pub fn new(access_token: impl Into<String>, account_id: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            account_id: account_id.into(),
        }
    }
}

/// Where the resolved `account_id` came from (for diagnostics / parity tracking).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountIdSource {
    /// `tokens.account_id` (the common, present-on-this-machine case).
    Tokens,
    /// A top-level `account_id` / `chatgpt_account_id` field.
    TopLevel,
}

/// Errors that can occur while resolving Codex credentials.
///
/// Note: an *absent* or *empty* file is NOT an error — it resolves to `None`
/// (the user simply has no codex login). Only a present-but-malformed file or a
/// file missing the required token fields is an error.
#[derive(Debug)]
pub enum CodexAuthError {
    /// The file exists but could not be read.
    Read(std::io::Error),
    /// The file exists but is not valid JSON in the expected shape.
    Parse(serde_json::Error),
    /// The file parsed but is missing an access token.
    MissingAccessToken,
    /// The file parsed but is missing an account id (and no usable fallback).
    MissingAccountId,
}

impl std::fmt::Display for CodexAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Never echo the path/content — keep secrets out of error strings.
            CodexAuthError::Read(_) => write!(f, "failed to read Codex auth.json"),
            CodexAuthError::Parse(_) => write!(f, "failed to parse Codex auth.json"),
            CodexAuthError::MissingAccessToken => {
                write!(f, "Codex auth.json is missing an access token")
            }
            CodexAuthError::MissingAccountId => {
                write!(f, "Codex auth.json is missing an account id")
            }
        }
    }
}

impl std::error::Error for CodexAuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CodexAuthError::Read(e) => Some(e),
            CodexAuthError::Parse(e) => Some(e),
            _ => None,
        }
    }
}

/// The on-disk Codex `auth.json` shape (only the fields we consume).
///
/// Unknown fields are ignored. All fields are optional so a partially-written or
/// future-extended file still parses; the *resolution* step enforces what is
/// actually required.
#[derive(Debug, Default, Deserialize)]
struct CodexAuthFile {
    #[serde(default)]
    tokens: Option<CodexTokens>,
    /// Legacy/top-level fallbacks (older CLI versions wrote these flat).
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    /// RFC3339 timestamp of the last token refresh (parsed for forward-compat /
    /// staleness diagnostics; this dev-only reader does not refresh).
    #[serde(default)]
    #[allow(dead_code)]
    last_refresh: Option<String>,
}

/// The nested `tokens` object the modern Codex CLI writes.
#[derive(Debug, Default, Deserialize)]
struct CodexTokens {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    id_token: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    refresh_token: Option<String>,
}

/// Resolve the path to the Codex `auth.json`.
///
/// `$CODEX_HOME/auth.json` if `CODEX_HOME` is set (it points at the `.codex`
/// directory itself, matching the legacy resolver), else `$HOME/.codex/auth.json`.
/// Returns `None` only if neither env var is set.
pub fn codex_auth_path() -> Option<PathBuf> {
    codex_auth_path_from(std::env::var_os("CODEX_HOME"), std::env::var_os("HOME"))
}

/// Pure path-resolution core (no global env access — unit-test entry point).
fn codex_auth_path_from(
    codex_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(codex_home) = codex_home {
        return Some(PathBuf::from(codex_home).join("auth.json"));
    }
    Some(PathBuf::from(home?).join(".codex").join("auth.json"))
}

/// Load Codex credentials from the default location, if present.
///
/// Returns:
/// - `Ok(None)` when the file is absent or empty (honest "no codex login"),
/// - `Ok(Some(auth))` when it parses and yields an access token + account id,
/// - `Err(_)` when the file exists but is malformed / missing required fields.
///
/// Path resolution honours `CODEX_HOME` (else `$HOME/.codex/auth.json`).
pub fn load_codex_auth() -> Result<Option<CodexAuth>, CodexAuthError> {
    let Some(path) = codex_auth_path() else {
        return Ok(None);
    };
    load_codex_auth_file(path)
}

/// Load Codex credentials from a specific `auth.json` path.
///
/// Same `Ok(None)` semantics as [`load_codex_auth`] for an absent/empty file, so
/// this is the unit-test entry point (point it at a tempfile fixture).
pub fn load_codex_auth_file(path: impl AsRef<Path>) -> Result<Option<CodexAuth>, CodexAuthError> {
    let path = path.as_ref();
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        // Absent file -> no codex login (not an error).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CodexAuthError::Read(e)),
    };
    if contents.trim().is_empty() {
        return Ok(None);
    }
    let file: CodexAuthFile = serde_json::from_str(&contents).map_err(CodexAuthError::Parse)?;
    resolve(file).map(Some)
}

/// Resolve a parsed file into [`CodexAuth`], applying the parity fallbacks.
fn resolve(file: CodexAuthFile) -> Result<CodexAuth, CodexAuthError> {
    let access_token = file
        .tokens
        .as_ref()
        .and_then(|t| non_empty(t.access_token.clone()))
        .or_else(|| non_empty(file.access_token.clone()))
        .ok_or(CodexAuthError::MissingAccessToken)?;

    let account_id = file
        .tokens
        .as_ref()
        .and_then(|t| non_empty(t.account_id.clone()))
        .or_else(|| non_empty(file.account_id.clone()))
        .or_else(|| non_empty(file.chatgpt_account_id.clone()))
        // NOTE (follow-up seam): the legacy resolver additionally decodes the
        // `id_token` JWT's `chatgpt_account_id` claim here. We do not, to keep
        // this crate base64-dep-free; on the ChatGPT flow `tokens.account_id` is
        // always populated. See module docs.
        .ok_or(CodexAuthError::MissingAccountId)?;

    Ok(CodexAuth::new(access_token, account_id))
}

/// `Some(v)` only when `v` is present and not blank.
fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Build a dev-only [`Route`](crate::route::Route) against the (cut) codex
/// backend from resolved [`CodexAuth`].
///
/// **DEV/TEST ONLY.** This points at `chatgpt.com/backend-api/codex/responses`
/// — a backend that is being removed from production. It exists purely so a
/// developer with a local codex login can smoke-test the real streaming stack;
/// production must use [`crate::providers`] (`OpenAi` / `Anthropic` /
/// `OpenAiCompatible`) instead. Wire format is the OpenAI Responses SSE protocol;
/// headers mirror the legacy provider (`Authorization: Bearer`,
/// `chatgpt-account-id`, `originator`, `OpenAI-Beta`).
pub fn dev_codex_route(auth: &CodexAuth) -> crate::route::Route {
    use crate::protocols::OpenAiResponsesProtocol;
    use crate::route::{Auth, Endpoint, Route};

    const DEV_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
    Route::new(
        Box::new(OpenAiResponsesProtocol::new()),
        Endpoint::new(DEV_CODEX_BASE_URL, "/codex/responses"),
        Auth::bearer(auth.access_token.clone())
            .and_then(Auth::header("chatgpt-account-id", auth.account_id.clone()))
            .and_then(Auth::header("originator", "browser-use-terminal"))
            .and_then(Auth::header("OpenAI-Beta", "responses=experimental")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `contents` to a fresh tempfile and return its path + the temp dir
    /// (kept alive by the caller so the file isn't deleted early).
    fn fixture(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(contents.as_bytes()).expect("write");
        (dir, path)
    }

    #[test]
    fn parses_modern_tokens_shape() {
        // Mirrors the real ~/.codex/auth.json (ChatGPT flow, null API key).
        let json = r#"{
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "eyJhbGciOi.payload.sig",
                "access_token": "access-abc-123",
                "refresh_token": "refresh-xyz",
                "account_id": "acct-uuid-0001"
            },
            "last_refresh": "2026-05-30T01:47:55.301041166Z"
        }"#;
        let (_dir, path) = fixture(json);
        let auth = load_codex_auth_file(&path)
            .expect("parses")
            .expect("present");
        assert_eq!(auth.access_token, "access-abc-123");
        assert_eq!(auth.account_id, "acct-uuid-0001");
    }

    #[test]
    fn missing_file_resolves_to_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.json");
        assert_eq!(load_codex_auth_file(&path).expect("ok"), None);
    }

    #[test]
    fn empty_file_resolves_to_none() {
        let (_dir, path) = fixture("   \n  ");
        assert_eq!(load_codex_auth_file(&path).expect("ok"), None);
    }

    #[test]
    fn top_level_fallbacks_are_honored() {
        // Older CLI layout: flat fields, no `tokens` object.
        let json = r#"{
            "access_token": "flat-access",
            "chatgpt_account_id": "flat-acct"
        }"#;
        let (_dir, path) = fixture(json);
        let auth = load_codex_auth_file(&path)
            .expect("parses")
            .expect("present");
        assert_eq!(auth.access_token, "flat-access");
        assert_eq!(auth.account_id, "flat-acct");
    }

    #[test]
    fn tokens_take_precedence_over_top_level() {
        let json = r#"{
            "access_token": "flat-access",
            "account_id": "flat-acct",
            "tokens": { "access_token": "nested-access", "account_id": "nested-acct" }
        }"#;
        let (_dir, path) = fixture(json);
        let auth = load_codex_auth_file(&path).unwrap().unwrap();
        assert_eq!(auth.access_token, "nested-access");
        assert_eq!(auth.account_id, "nested-acct");
    }

    #[test]
    fn missing_access_token_is_error() {
        let json = r#"{ "tokens": { "account_id": "acct" } }"#;
        let (_dir, path) = fixture(json);
        let err = load_codex_auth_file(&path).unwrap_err();
        assert!(matches!(err, CodexAuthError::MissingAccessToken));
    }

    #[test]
    fn missing_account_id_is_error() {
        let json = r#"{ "tokens": { "access_token": "acc" } }"#;
        let (_dir, path) = fixture(json);
        let err = load_codex_auth_file(&path).unwrap_err();
        assert!(matches!(err, CodexAuthError::MissingAccountId));
    }

    #[test]
    fn blank_token_is_treated_as_missing() {
        let json = r#"{ "tokens": { "access_token": "  ", "account_id": "acct" } }"#;
        let (_dir, path) = fixture(json);
        let err = load_codex_auth_file(&path).unwrap_err();
        assert!(matches!(err, CodexAuthError::MissingAccessToken));
    }

    #[test]
    fn malformed_json_is_parse_error() {
        let (_dir, path) = fixture("{ not json ");
        let err = load_codex_auth_file(&path).unwrap_err();
        assert!(matches!(err, CodexAuthError::Parse(_)));
        // The error Display must not leak file contents.
        assert!(!format!("{err}").contains("not json"));
    }

    #[test]
    fn path_prefers_codex_home_then_home() {
        // Pure, env-free: CODEX_HOME points directly at the .codex dir, else
        // fall back to $HOME/.codex.
        assert_eq!(
            codex_auth_path_from(Some("/tmp/codex-home-x".into()), Some("/tmp/ignored".into())),
            Some(PathBuf::from("/tmp/codex-home-x/auth.json"))
        );
        assert_eq!(
            codex_auth_path_from(None, Some("/tmp/home-y".into())),
            Some(PathBuf::from("/tmp/home-y/.codex/auth.json"))
        );
        // Neither set -> None (honest, no panic).
        assert_eq!(codex_auth_path_from(None, None), None);
    }

    #[test]
    fn dev_codex_route_is_built_but_marked_dev_only() {
        // The dev-only route targets the (cut) codex backend; assert its shape so
        // a developer smoke-testing it gets the expected target + headers, and
        // confirm the token never leaks via Debug.
        let auth = CodexAuth::new("dev-access", "dev-acct");
        let route = dev_codex_route(&auth);
        assert_eq!(
            route.endpoint.url(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        let header = |name: &str| {
            route
                .auth
                .headers()
                .into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v)
        };
        assert_eq!(header("authorization").as_deref(), Some("Bearer dev-access"));
        assert_eq!(header("chatgpt-account-id").as_deref(), Some("dev-acct"));
        assert!(!format!("{route:?}").contains("dev-access"));
    }
}
