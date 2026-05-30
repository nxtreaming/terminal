//! Network-proxy configuration types.
//!
//! Codex parity: `codex-rs/network-proxy/src/config.rs`. This mirrors the
//! shape codex uses at runtime: a top-level `NetworkProxyConfig` wrapping a
//! `NetworkProxySettings`, with a `NetworkMode` and a per-domain
//! allow/deny permission map.
//!
//! Faithful subset: codex's `NetworkProxySettings` (config.rs:121) has many
//! transport fields (`proxy_url`, `socks_url`, unix sockets, mitm hooks, …)
//! that are proxy-server plumbing. Since the actual MITM server is an honest
//! SEAM here (see `proxy.rs`), this config keeps the fields the
//! allowlist/approval core actually consumes — `enabled`, `mode`, and the
//! `domains` permission map — and documents the omissions as parity debt.

use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

/// Effective per-domain permission. Variant order encodes precedence:
/// `None < Allow < Deny`, so deny wins over allow for the same pattern.
///
/// Codex parity: `NetworkDomainPermission` in
/// `network-proxy/src/config.rs:28` (identical variant order + `Ord` for
/// deny-wins, config.rs:24-32).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum NetworkDomainPermission {
    None,
    Allow,
    Deny,
}

/// A per-domain permission map (pattern -> permission).
///
/// Codex parity: `NetworkDomainPermissions` in
/// `network-proxy/src/config.rs:41` (codex stores an ordered `Vec` of entries
/// with a custom serde map repr; we use a `BTreeMap`, which matches codex's
/// *serialized* effective shape — a `{pattern: permission}` map, config.rs:50).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct NetworkDomainPermissions {
    pub entries: BTreeMap<String, NetworkDomainPermission>,
}

impl NetworkDomainPermissions {
    /// Patterns with the given permission.
    ///
    /// Codex parity: `domain_entries` (`config.rs:178`).
    pub fn patterns_with(&self, permission: NetworkDomainPermission) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, p)| **p == permission)
            .map(|(pattern, _)| pattern.clone())
            .collect()
    }

    /// Allowed patterns.
    /// Codex parity: `allowed_domains` (`config.rs:170`).
    pub fn allowed(&self) -> Vec<String> {
        self.patterns_with(NetworkDomainPermission::Allow)
    }

    /// Denied patterns.
    /// Codex parity: `denied_domains` (`config.rs:174`).
    pub fn denied(&self) -> Vec<String> {
        self.patterns_with(NetworkDomainPermission::Deny)
    }
}

/// Network access mode.
///
/// Codex parity: `NetworkMode` in `network-proxy/src/config.rs:276`,
/// including `Full` as `#[default]` (config.rs:283).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// Limited (read-only): only GET/HEAD/OPTIONS for HTTP.
    /// Codex parity: `NetworkMode::Limited` (config.rs:280).
    Limited,
    /// Full access: all HTTP methods.
    /// Codex parity: `NetworkMode::Full` (config.rs:283, `#[default]`).
    #[default]
    Full,
}

impl NetworkMode {
    /// Whether `method` is permitted in this mode.
    ///
    /// Codex parity: `NetworkMode::allows_method`
    /// (`network-proxy/src/config.rs:288`).
    pub fn allows_method(self, method: &str) -> bool {
        match self {
            Self::Full => true,
            Self::Limited => matches!(method, "GET" | "HEAD" | "OPTIONS"),
        }
    }
}

/// Network-proxy runtime settings.
///
/// Codex parity: `NetworkProxySettings` in `network-proxy/src/config.rs:121`
/// (subset — see module docs for omitted transport/mitm fields).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct NetworkProxySettings {
    /// Whether the network proxy is enabled.
    /// Codex parity: `NetworkProxySettings::enabled` (config.rs:123).
    pub enabled: bool,
    /// Access mode. Codex parity: `NetworkProxySettings::mode` (config.rs:136).
    pub mode: NetworkMode,
    /// Per-domain allow/deny map.
    /// Codex parity: `NetworkProxySettings::domains` (config.rs:138,
    /// `Option<NetworkDomainPermissions>`).
    pub domains: Option<NetworkDomainPermissions>,
}

impl NetworkProxySettings {
    /// Allowed domain patterns (empty if none).
    /// Codex parity: `allowed_domains` (`config.rs:170`).
    pub fn allowed_domains(&self) -> Vec<String> {
        self.domains
            .as_ref()
            .map(|d| d.allowed())
            .unwrap_or_default()
    }

    /// Denied domain patterns (empty if none).
    /// Codex parity: `denied_domains` (`config.rs:174`).
    pub fn denied_domains(&self) -> Vec<String> {
        self.domains
            .as_ref()
            .map(|d| d.denied())
            .unwrap_or_default()
    }
}

/// Top-level network-proxy config.
///
/// Codex parity: `NetworkProxyConfig` in `network-proxy/src/config.rs:19`
/// (`{ network: NetworkProxySettings }`, with `#[serde(default)]` on the
/// `network` field, config.rs:20-21).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkProxyConfig {
    #[serde(default)]
    pub network: NetworkProxySettings,
}

impl NetworkProxyConfig {
    /// A disabled config (matches `Default`).
    pub fn disabled() -> Self {
        Self::default()
    }

    /// The allow patterns.
    pub fn allowed_domains(&self) -> Vec<String> {
        self.network.allowed_domains()
    }

    /// The deny patterns.
    pub fn denied_domains(&self) -> Vec<String> {
        self.network.denied_domains()
    }
}
