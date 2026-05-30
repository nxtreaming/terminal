//! OAuth 2.0 PKCE helpers for MCP streamable-HTTP servers.
//!
//! The PURE, testable pieces mirror codex's mechanism. Codex delegates PKCE/URL
//! building to the `oauth2` crate (`rmcp-client/src/perform_oauth_login.rs:615`
//! `start_authorization`, which uses S256), but the underlying spec is fixed:
//! - PKCE code challenge = `base64url-nopad(SHA-256(verifier))`. Codex pulls in
//!   exactly these primitives: `base64::engine::general_purpose::URL_SAFE_NO_PAD`
//!   (`perform_oauth_login.rs:11`) and `sha2::Sha256`
//!   (`perform_oauth_login.rs:19`, used at `:404-405` for the same
//!   base64url-nopad(sha256(..)) construction on a different value).
//! - the token cache mirrors codex `StoredOAuthTokens`
//!   (`rmcp-client/src/oauth.rs:57-63`) and its `.credentials.json` file
//!   fallback (`oauth.rs:371` `FALLBACK_FILENAME = ".credentials.json"`,
//!   `:381-385` the on-disk entry, `:433-454` save).
//!
//! PARITY DEBT (see report): the INTERACTIVE leg (dynamic client registration,
//! open-browser, loopback redirect listener, token HTTP exchange) is NOT wired;
//! [`perform_interactive_login`] returns a clear
//! [`OauthError::InteractiveNotWired`]. Everything that leg would feed off (PKCE,
//! URL, callback parse, token cache) is real and unit-tested, and a static
//! `bearer_token` works end-to-end through the HTTP transport without it. The
//! codex OS-keyring store (`oauth.rs:2-3,46-47`) is also dropped — we keep only
//! the JSON-file cache.

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Default scopes for an MCP OAuth login.
pub const DEFAULT_OAUTH_SCOPES: &[&str] = &["openid", "profile", "email", "offline_access"];

#[derive(Debug, Error)]
pub enum OauthError {
    #[error("interactive oauth flow not wired: {0}")]
    InteractiveNotWired(String),
    #[error("invalid redirect callback: {0}")]
    InvalidCallback(String),
    #[error("token cache io error: {0}")]
    Io(String),
}

/// A PKCE verifier/challenge pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Generate `num_bytes` of CSPRNG bytes encoded as url-safe-no-pad base64.
///
/// Uses `rand`'s thread-local CSPRNG (`rand::rng()`, the OS-seeded `ThreadRng`)
/// filled via `RngCore::fill_bytes`.
pub fn random_url_safe_token(num_bytes: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; num_bytes];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate a PKCE verifier: 32 random bytes, url-safe-no-pad base64 (a 43-char
/// high-entropy string, within RFC 7636's 43..128 range).
pub fn generate_pkce_verifier() -> String {
    random_url_safe_token(32)
}

/// Compute the S256 code challenge for a verifier:
/// `base64url-nopad(SHA-256(verifier))` (RFC 7636 §4.2).
pub fn code_challenge_s256(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    URL_SAFE_NO_PAD.encode(digest)
}

/// Generate a full PKCE pair.
pub fn generate_pkce() -> Pkce {
    let verifier = generate_pkce_verifier();
    let challenge = code_challenge_s256(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

/// Percent-encode per the RFC 3986 unreserved set (everything else escaped).
pub fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    encoded
}

/// Build an authorization-code + PKCE (S256) authorization URL.
#[allow(clippy::too_many_arguments)]
pub fn build_authorization_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
    scopes: &[&str],
    resource: Option<&str>,
) -> String {
    let scope = scopes.join(" ");
    let mut url = format!(
        "{authorization_endpoint}?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}&scope={}",
        urlencode(client_id),
        urlencode(redirect_uri),
        urlencode(code_challenge),
        urlencode(state),
        urlencode(&scope),
    );
    if let Some(resource) = resource {
        url.push_str(&format!("&resource={}", urlencode(resource)));
    }
    url
}

/// Parse an OAuth redirect callback query string (the part after `?`) and return
/// the authorization `code`. If the IdP returned an `error`, that is surfaced.
pub fn parse_redirect_callback(query: &str) -> Result<String, OauthError> {
    let mut code: Option<String> = None;
    let mut error: Option<String> = None;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);
        match key {
            "code" => code = Some(value),
            "error" => error = Some(value),
            _ => {}
        }
    }
    if let Some(err) = error {
        return Err(OauthError::InvalidCallback(format!(
            "idp returned error: {err}"
        )));
    }
    code.ok_or_else(|| OauthError::InvalidCallback("missing `code` parameter".to_string()))
}

/// Minimal percent-decoder for query values (also turns `+` into a space, per
/// `application/x-www-form-urlencoded`).
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1]);
                let lo = hex_val(bytes[i + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi << 4) | lo);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Persisted OAuth tokens for one server. Mirrors codex `StoredOAuthTokens`
/// (`rmcp-client/src/oauth.rs:57-63`) — access/refresh token + expiry — flattened
/// to plain strings (we do not carry codex's full `WrappedOAuthTokenResponse`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredOAuthTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<SystemTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
}

/// On-disk token cache keyed by server name. The JSON-file form mirrors codex's
/// `.credentials.json` fallback (`rmcp-client/src/oauth.rs:371-385`). The
/// OS-keyring path is intentionally not implemented (parity debt).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OAuthTokenStore {
    #[serde(default)]
    pub servers: HashMap<String, StoredOAuthTokens>,
}

impl OAuthTokenStore {
    pub fn load(path: &Path) -> Result<Self, OauthError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(path)
            .map_err(|e| OauthError::Io(format!("reading {path:?}: {e}")))?;
        serde_json::from_str(&contents)
            .map_err(|e| OauthError::Io(format!("parsing {path:?}: {e}")))
    }

    pub fn save(&self, path: &Path) -> Result<(), OauthError> {
        let contents = serde_json::to_string_pretty(self)
            .map_err(|e| OauthError::Io(format!("serializing token store: {e}")))?;
        std::fs::write(path, contents).map_err(|e| OauthError::Io(format!("writing {path:?}: {e}")))
    }

    pub fn get(&self, server: &str) -> Option<&StoredOAuthTokens> {
        self.servers.get(server)
    }

    pub fn set(&mut self, server: impl Into<String>, tokens: StoredOAuthTokens) {
        self.servers.insert(server.into(), tokens);
    }
}

/// The interactive authorization-code login leg. NOT wired: opening a browser,
/// running a loopback redirect listener, and exchanging the code for tokens are
/// out of scope for this work package. Returns a clear error so callers fall
/// back to a static `bearer_token`.
pub fn perform_interactive_login(server: &str) -> Result<StoredOAuthTokens, OauthError> {
    Err(OauthError::InteractiveNotWired(format!(
        "server `{server}`: open-browser + loopback redirect + token exchange not implemented; \
         configure a static bearer_token instead"
    )))
}
