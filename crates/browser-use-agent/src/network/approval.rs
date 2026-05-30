//! Deferred network-approval decision logic.
//!
//! When a request targets a host that is not on the allowlist (and not on the
//! denylist), codex does not hard-fail: it surfaces a *deferred approval*
//! (`NetworkPolicyDecision::Ask`) to be resolved by a decider/user. Hosts on
//! the denylist (or off the allowlist with no decider) become a hard `Deny`.
//!
//! Codex parity (`codex-rs/network-proxy/src/network_policy.rs`):
//! - `NetworkPolicyDecision { Deny, Ask }` (:43)
//! - `NetworkDecisionSource { BaselinePolicy, ModeGuard, ProxyState, Decider }`
//!   (:59)
//! - `NetworkProtocol { Http, HttpsConnect, Socks5Tcp, Socks5Udp }` (:23)
//! - `NetworkDecision { Allow, Deny { reason, source, decision } }` (:122) +
//!   `NetworkDecision::deny`/`ask` constructors (:132/:136)
//! - the allow/not-allowed/deny branching of `evaluate_host_policy`
//!   (:289-359): allowed -> `Allow`; off-allowlist -> decider (`Ask`/`Deny`)
//!   else `BaselinePolicy` `Deny`; on-denylist -> `BaselinePolicy` `Deny`.
//!
//! Agent seam: a hard `Deny` for a specific host is surfaced to the tool
//! orchestrator via `crate::tools::runtime::NetworkPolicyDecision { host }`
//! (tools/runtime.rs:49), attached to a `SandboxDenial`
//! (tools/runtime.rs:56). [`to_runtime_decision`] performs that bridge. We do
//! NOT modify tools/runtime.rs.

use crate::tools::runtime::NetworkPolicyDecision as RuntimeNetworkPolicyDecision;

use super::allowlist::host_matches_any;
use super::allowlist::normalize_host;
use super::config::NetworkProxyConfig;

/// Transport protocol of a network request.
///
/// Codex parity: `NetworkProtocol` in
/// `network-proxy/src/network_policy.rs:23`, including `as_policy_protocol`
/// label strings (:31).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkProtocol {
    Http,
    HttpsConnect,
    Socks5Tcp,
    Socks5Udp,
}

impl NetworkProtocol {
    /// Codex parity: `NetworkProtocol::as_policy_protocol`
    /// (`network_policy.rs:31`).
    pub const fn as_policy_protocol(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::HttpsConnect => "https_connect",
            Self::Socks5Tcp => "socks5_tcp",
            Self::Socks5Udp => "socks5_udp",
        }
    }
}

/// The kind of a non-allow decision.
///
/// Codex parity: `NetworkPolicyDecision { Deny, Ask }` in
/// `network-proxy/src/network_policy.rs:43` (serde `lowercase`, :42).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicyDecision {
    /// Hard deny.
    Deny,
    /// Deferred — surface an approval prompt.
    Ask,
}

impl NetworkPolicyDecision {
    /// Codex parity: `NetworkPolicyDecision::as_str` (`network_policy.rs:48`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

/// Which layer produced a decision.
///
/// Codex parity: `NetworkDecisionSource` in
/// `network-proxy/src/network_policy.rs:59` (serde `snake_case`, :58).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkDecisionSource {
    BaselinePolicy,
    ModeGuard,
    ProxyState,
    Decider,
}

impl NetworkDecisionSource {
    /// Codex parity: `NetworkDecisionSource::as_str` (`network_policy.rs:67`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BaselinePolicy => "baseline_policy",
            Self::ModeGuard => "mode_guard",
            Self::ProxyState => "proxy_state",
            Self::Decider => "decider",
        }
    }
}

/// A request whose network access must be decided.
///
/// Codex parity: `NetworkPolicyRequest` in
/// `network-proxy/src/network_policy.rs:78` (subset — the fields the pure
/// allowlist/approval decision actually consumes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkPolicyRequest {
    pub protocol: NetworkProtocol,
    pub host: String,
    pub port: u16,
    pub method: Option<String>,
}

/// The outcome of evaluating a request against the allow/deny policy.
///
/// Codex parity: `NetworkDecision` in
/// `network-proxy/src/network_policy.rs:122`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkDecision {
    /// Allowed (on the allowlist).
    /// Codex parity: `NetworkDecision::Allow` (:123).
    Allow,
    /// Not allowed: either a hard `Deny` or a deferred `Ask`.
    /// Codex parity: `NetworkDecision::Deny { reason, source, decision }`
    /// (:124-128).
    Deny {
        reason: String,
        source: NetworkDecisionSource,
        decision: NetworkPolicyDecision,
    },
}

impl NetworkDecision {
    /// A hard deny from the `Decider` source.
    /// Codex parity: `NetworkDecision::deny` (`network_policy.rs:132`).
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::deny_with_source(reason, NetworkDecisionSource::Decider)
    }

    /// A deferred ask from the `Decider` source.
    /// Codex parity: `NetworkDecision::ask` (`network_policy.rs:136`).
    pub fn ask(reason: impl Into<String>) -> Self {
        Self::ask_with_source(reason, NetworkDecisionSource::Decider)
    }

    /// Codex parity: `NetworkDecision::deny_with_source`
    /// (`network_policy.rs:140`).
    pub fn deny_with_source(reason: impl Into<String>, source: NetworkDecisionSource) -> Self {
        Self::Deny {
            reason: reason.into(),
            source,
            decision: NetworkPolicyDecision::Deny,
        }
    }

    /// Codex parity: `NetworkDecision::ask_with_source`
    /// (`network_policy.rs:154`).
    pub fn ask_with_source(reason: impl Into<String>, source: NetworkDecisionSource) -> Self {
        Self::Deny {
            reason: reason.into(),
            source,
            decision: NetworkPolicyDecision::Ask,
        }
    }

    /// Whether this is an `Allow`.
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Reason text constants. Codex parity: `network-proxy/src/reasons.rs`.
pub const REASON_DENIED: &str = "denied";
pub const REASON_NOT_ALLOWED: &str = "not_allowed";

/// What the user/decider chose for a deferred `Ask`.
///
/// Codex parity: the `Decider` returns a `NetworkDecision` (`Allow` to
/// override, or `Deny`); we model the resolution of a pending `Ask` as a small
/// enum that maps onto codex's `map_decider_decision` (`network_policy.rs:361`)
/// outcomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalResponse {
    /// Approve this request (decider override).
    Allow,
    /// Approve and add the host to the live allowlist for the session.
    AllowAndRemember,
    /// Deny this request.
    Deny,
}

/// Evaluate a request against the config's allow/deny patterns.
///
/// Codex parity: the host-policy branching of `evaluate_host_policy`
/// (`network-proxy/src/network_policy.rs:289-319`):
/// - denylist match  -> `BaselinePolicy` hard `Deny` (the `host_blocked`
///   `Blocked(reason)` arm, :312-318)
/// - allowlist match -> `Allow` (:296)
/// - otherwise (`NotAllowed`):
///     - with a decider -> the decider's `Ask`/`Deny` from the `Decider`
///       source (:297-301, `map_decider_decision` :361)
///     - without a decider -> `BaselinePolicy` hard `Deny` (:302-309)
///
/// Precedence note: denylist is checked first so deny wins over allow, matching
/// codex's `NetworkDomainPermission` ordering (`config.rs:24`).
pub fn evaluate_request(
    config: &NetworkProxyConfig,
    request: &NetworkPolicyRequest,
    has_decider: bool,
) -> NetworkDecision {
    let host = normalize_host(&request.host);
    let denied = config.denied_domains();
    let allowed = config.allowed_domains();

    if host_matches_any(&host, &denied) {
        return NetworkDecision::deny_with_source(
            REASON_DENIED,
            NetworkDecisionSource::BaselinePolicy,
        );
    }
    if host_matches_any(&host, &allowed) {
        return NetworkDecision::Allow;
    }
    if has_decider {
        // Off-allowlist with a decider present -> defer (Ask).
        NetworkDecision::ask_with_source(REASON_NOT_ALLOWED, NetworkDecisionSource::Decider)
    } else {
        NetworkDecision::deny_with_source(REASON_NOT_ALLOWED, NetworkDecisionSource::BaselinePolicy)
    }
}

/// Resolve a pending [`NetworkDecision`] (only meaningful for an `Ask`) given
/// the user's [`ApprovalResponse`]. `Allow`/`AllowAndRemember` -> `Allow`
/// (and, for remember, the host is appended to the live allow patterns);
/// `Deny` -> `Decider` hard `Deny`.
///
/// Codex parity: the `match rx.await` resolution in codex's network-approval
/// flow (`core/src/tools/network_approval.rs:545-578`): `Allow` overrides to
/// allowed (`NetworkPolicyRuleAction::Allow`, :548), `Deny` becomes a hard deny
/// (:578). `AllowAndRemember` mirrors codex appending an allow rule to the live
/// policy.
pub fn apply_response(
    decision: NetworkDecision,
    response: ApprovalResponse,
    host: &str,
    live_allow: &mut Vec<String>,
) -> NetworkDecision {
    let is_ask = matches!(
        decision,
        NetworkDecision::Deny {
            decision: NetworkPolicyDecision::Ask,
            ..
        }
    );
    if !is_ask {
        // Already resolved (`Allow` or hard `Deny`): unchanged.
        return decision;
    }
    match response {
        ApprovalResponse::Allow => NetworkDecision::Allow,
        ApprovalResponse::AllowAndRemember => {
            let normalized = normalize_host(host);
            if !live_allow.iter().any(|p| p == &normalized) {
                live_allow.push(normalized);
                live_allow.sort();
                live_allow.dedup();
            }
            NetworkDecision::Allow
        }
        ApprovalResponse::Deny => NetworkDecision::deny(REASON_DENIED),
    }
}

/// Bridge a hard-`Deny` [`NetworkDecision`] into the agent's seam type for a
/// blocked request, ready to attach to a `SandboxDenial`.
///
/// Codex parity: a denied network request surfaces the blocked host to the
/// orchestrator. The agent seam carries just the host
/// (`crate::tools::runtime::NetworkPolicyDecision { host }`,
/// tools/runtime.rs:49,56). Returns `Some` only for a hard `Deny`; `Allow` and
/// deferred `Ask` return `None` (an `Ask` is not yet a denial).
pub fn to_runtime_decision(
    decision: &NetworkDecision,
    host: &str,
) -> Option<RuntimeNetworkPolicyDecision> {
    match decision {
        NetworkDecision::Deny {
            decision: NetworkPolicyDecision::Deny,
            ..
        } => Some(RuntimeNetworkPolicyDecision {
            host: normalize_host(host),
        }),
        _ => None,
    }
}
