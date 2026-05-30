//! Network proxy / allowlist / deferred-network-approval subsystem.
//!
//! Codex parity: `codex-rs/network-proxy/src/{policy,config,network_policy,
//! proxy,state}.rs` (the `codex_network_proxy` crate).
//!
//! ## What is REAL vs what is a SEAM (read this)
//!
//! REAL + exhaustively tested (the bulk of the value):
//! - [`allowlist`] — pure host normalization + domain-pattern matching, a
//!   faithful behavioral port of codex `policy.rs` (no `globset`/`url` deps; see
//!   `allowlist.rs` for the one mid-label-wildcard parity gap).
//! - [`config`] — `NetworkProxyConfig` / `NetworkMode` / per-domain
//!   allow-deny map (codex `config.rs`).
//! - [`approval`] — `NetworkDecision` / `NetworkPolicyDecision {Deny,Ask}` +
//!   `evaluate_request` (codex `network_policy.rs`), bridged to the agent's
//!   `crate::tools::runtime::NetworkPolicyDecision` seam.
//!
//! SEAM / PARITY DEBT (NOT a working MITM):
//! - [`proxy`] implements the runtime/handle/loader plumbing and, when started,
//!   binds a loopback listener so `proxy_url`/handle/shutdown are genuinely
//!   exercised — but there is NO real HTTP/CONNECT MITM, NO TLS, NO SOCKS5, and
//!   NO real CA generation (placeholder PEM only). The crates codex's real
//!   proxy needs (hyper/TLS/cert) are not in this workspace. Off-allowlist
//!   enforcement is the pure [`approval`] decision layer, not the wire. See
//!   `proxy.rs` for full detail.

pub mod allowlist;
pub mod approval;
pub mod config;
pub mod proxy;

// Pure allowlist core (codex policy.rs).
pub use allowlist::host_matches_any;
pub use allowlist::host_matches_pattern;
pub use allowlist::is_loopback_host;
pub use allowlist::normalize_host;
pub use allowlist::parse_domain_pattern;
pub use allowlist::DomainPattern;

// Config (codex config.rs).
pub use config::NetworkDomainPermission;
pub use config::NetworkDomainPermissions;
pub use config::NetworkMode;
pub use config::NetworkProxyConfig;
pub use config::NetworkProxySettings;

// Deferred approval (codex network_policy.rs).
pub use approval::apply_response;
pub use approval::evaluate_request;
pub use approval::to_runtime_decision;
pub use approval::ApprovalResponse;
pub use approval::NetworkDecision;
pub use approval::NetworkDecisionSource;
pub use approval::NetworkPolicyDecision;
pub use approval::NetworkPolicyRequest;
pub use approval::NetworkProtocol;

// Runtime / handle / loader (codex proxy.rs / state.rs) — SEAM proxy.
pub use proxy::generate_seam_ca_pem;
pub use proxy::load_network_proxy;
pub use proxy::start_network_proxy;
pub use proxy::NetworkProxyHandle;
pub use proxy::NetworkProxyRuntime;

#[cfg(test)]
mod tests;
