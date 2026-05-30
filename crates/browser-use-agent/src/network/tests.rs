//! Network-free tests for the network-proxy subsystem.
//!
//! Allowlist / config / approval tests are PURE (no I/O). The single handle
//! test binds a loopback `127.0.0.1:0` listener purely to assert config/url/
//! handle plumbing — it serves NO traffic and makes NO outbound connection.
//!
//! Allowlist + mode tests mirror codex `network-proxy/src/policy.rs` tests
//! (cited per test). Approval tests mirror the decision branching of
//! `network-proxy/src/network_policy.rs` (cited per test).

use super::allowlist::host_matches_any;
use super::allowlist::host_matches_pattern;
use super::allowlist::is_loopback_host;
use super::allowlist::normalize_host;
use super::allowlist::parse_domain_pattern;
use super::allowlist::DomainPattern;
use super::approval::apply_response;
use super::approval::evaluate_request;
use super::approval::to_runtime_decision;
use super::approval::ApprovalResponse;
use super::approval::NetworkDecision;
use super::approval::NetworkDecisionSource;
use super::approval::NetworkPolicyDecision;
use super::approval::NetworkPolicyRequest;
use super::approval::NetworkProtocol;
use super::approval::REASON_DENIED;
use super::approval::REASON_NOT_ALLOWED;
use super::config::NetworkDomainPermission;
use super::config::NetworkDomainPermissions;
use super::config::NetworkMode;
use super::config::NetworkProxyConfig;
use super::config::NetworkProxySettings;
use super::proxy::load_network_proxy;
use super::proxy::SEAM_CA_PEM_PLACEHOLDER;

use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// normalize_host  (codex network-proxy/src/policy.rs)
// ---------------------------------------------------------------------------

/// Codex parity: `normalize_host_lowercases_and_trims` (policy.rs:468).
#[test]
fn normalize_host_lowercases_and_trims() {
    assert_eq!(normalize_host("  ExAmPlE.CoM  "), "example.com");
}

/// Codex parity: `normalize_host_strips_port_for_host_port` (policy.rs:473).
#[test]
fn normalize_host_strips_port_for_host_port() {
    assert_eq!(normalize_host("example.com:1234"), "example.com");
}

/// Codex parity: `normalize_host_preserves_unbracketed_ipv6` (policy.rs:478).
#[test]
fn normalize_host_preserves_unbracketed_ipv6() {
    assert_eq!(normalize_host("2001:db8::1"), "2001:db8::1");
}

/// Codex parity: `normalize_host_strips_trailing_dot` (policy.rs:483).
#[test]
fn normalize_host_strips_trailing_dot() {
    assert_eq!(normalize_host("example.com."), "example.com");
    assert_eq!(normalize_host("ExAmPlE.CoM."), "example.com");
}

/// Codex parity: `normalize_host_strips_trailing_dot_with_port`
/// (policy.rs:489).
#[test]
fn normalize_host_strips_trailing_dot_with_port() {
    assert_eq!(normalize_host("example.com.:443"), "example.com");
}

/// Codex parity: `normalize_host_strips_brackets_for_ipv6` (policy.rs:494).
#[test]
fn normalize_host_strips_brackets_for_ipv6() {
    assert_eq!(normalize_host("[::1]"), "::1");
    assert_eq!(normalize_host("[::1]:443"), "::1");
}

// ---------------------------------------------------------------------------
// is_loopback_host  (codex policy.rs:33)
// ---------------------------------------------------------------------------

/// Codex parity: `is_loopback_host_handles_localhost_variants` (policy.rs:429).
#[test]
fn is_loopback_host_handles_localhost_variants() {
    assert!(is_loopback_host("localhost"));
    assert!(is_loopback_host("localhost."));
    assert!(is_loopback_host("LOCALHOST"));
    assert!(!is_loopback_host("notlocalhost"));
}

/// Codex parity: `is_loopback_host_handles_ip_literals` (policy.rs:437).
#[test]
fn is_loopback_host_handles_ip_literals() {
    assert!(is_loopback_host("127.0.0.1"));
    assert!(is_loopback_host("::1"));
    assert!(!is_loopback_host("1.2.3.4"));
}

// ---------------------------------------------------------------------------
// parse_domain_pattern + host_matches_pattern  (codex policy.rs grammar)
// ---------------------------------------------------------------------------

/// Codex parity: `DomainPattern::parse` (policy.rs:237) + the grammar at
/// `compile_globset_with_policy` (policy.rs:206-210).
#[test]
fn parse_domain_pattern_decodes_each_form() {
    assert_eq!(parse_domain_pattern("*"), DomainPattern::Global);
    assert_eq!(
        parse_domain_pattern("**.Example.COM"),
        DomainPattern::ApexAndSubdomains("example.com".to_string())
    );
    assert_eq!(
        parse_domain_pattern("*.Example.COM."),
        DomainPattern::SubdomainsOnly("example.com".to_string())
    );
    assert_eq!(
        parse_domain_pattern("Example.COM"),
        DomainPattern::Exact("example.com".to_string())
    );
}

/// Codex parity: `compile_globset_normalizes_wildcards` (policy.rs:386) —
/// `*.example.com` matches subdomains but NOT the apex.
#[test]
fn host_matches_pattern_subdomains_only() {
    assert!(host_matches_pattern("api.example.com", "*.example.com"));
    assert!(!host_matches_pattern("example.com", "*.example.com"));
    assert!(!host_matches_pattern("evil.com", "*.example.com"));
}

/// Codex parity: `compile_globset_normalizes_apex_and_subdomains`
/// (policy.rs:404) — `**.example.com` matches apex AND subdomains.
#[test]
fn host_matches_pattern_apex_and_subdomains() {
    assert!(host_matches_pattern("example.com", "**.example.com"));
    assert!(host_matches_pattern("api.example.com", "**.example.com"));
    assert!(!host_matches_pattern("evil.com", "**.example.com"));
}

/// Codex parity: `compile_globset_normalizes_trailing_dots` (policy.rs:378) —
/// exact host matches itself, not subdomains, and trailing dots normalize.
#[test]
fn host_matches_pattern_exact() {
    assert!(host_matches_pattern("example.com", "Example.COM."));
    assert!(!host_matches_pattern("api.example.com", "example.com"));
}

/// `*` global wildcard matches every host (codex `"*"` early-return,
/// policy.rs:152, used by `compile_allowlist_globset`).
#[test]
fn host_matches_pattern_global() {
    assert!(host_matches_pattern("anything.example.com", "*"));
    assert!(host_matches_pattern("8.8.8.8", "*"));
}

/// `host_matches_any` is the any-of over a pattern set (codex
/// `GlobSet::is_match` via `compile_allowlist_globset`, policy.rs:185).
#[test]
fn host_matches_any_scans_all_patterns() {
    let patterns = vec!["a.example.com".to_string(), "*.api.example.com".to_string()];
    assert!(host_matches_any("a.example.com", &patterns));
    assert!(host_matches_any("v1.api.example.com", &patterns));
    assert!(!host_matches_any("nope.example.com", &patterns));
}

// ---------------------------------------------------------------------------
// config / NetworkMode  (codex config.rs)
// ---------------------------------------------------------------------------

/// Codex parity: `method_allowed_full_allows_everything` (policy.rs:362) +
/// `NetworkMode::allows_method` (config.rs:288).
#[test]
fn network_mode_full_allows_everything() {
    assert!(NetworkMode::Full.allows_method("GET"));
    assert!(NetworkMode::Full.allows_method("POST"));
    assert!(NetworkMode::Full.allows_method("CONNECT"));
}

/// Codex parity: `method_allowed_limited_allows_only_safe_methods`
/// (policy.rs:369).
#[test]
fn network_mode_limited_allows_only_safe_methods() {
    assert!(NetworkMode::Limited.allows_method("GET"));
    assert!(NetworkMode::Limited.allows_method("HEAD"));
    assert!(NetworkMode::Limited.allows_method("OPTIONS"));
    assert!(!NetworkMode::Limited.allows_method("POST"));
    assert!(!NetworkMode::Limited.allows_method("CONNECT"));
}

/// `NetworkMode` default is `Full`. Codex parity: `#[default] Full`
/// (config.rs:283).
#[test]
fn network_mode_default_is_full() {
    assert_eq!(NetworkMode::default(), NetworkMode::Full);
}

/// Deny-wins ordering for `NetworkDomainPermission`.
/// Codex parity: `None < Allow < Deny` (config.rs:24-32).
#[test]
fn network_domain_permission_deny_wins_ordering() {
    assert!(NetworkDomainPermission::Deny > NetworkDomainPermission::Allow);
    assert!(NetworkDomainPermission::Allow > NetworkDomainPermission::None);
}

/// Config exposes allow/deny pattern lists from the domain map.
/// Codex parity: `allowed_domains`/`denied_domains` (config.rs:170/174).
#[test]
fn config_allowed_and_denied_domains() {
    let mut entries = BTreeMap::new();
    entries.insert(
        "allow.example.com".to_string(),
        NetworkDomainPermission::Allow,
    );
    entries.insert(
        "deny.example.com".to_string(),
        NetworkDomainPermission::Deny,
    );
    let config = NetworkProxyConfig {
        network: NetworkProxySettings {
            enabled: true,
            mode: NetworkMode::Full,
            domains: Some(NetworkDomainPermissions { entries }),
        },
    };
    assert_eq!(
        config.allowed_domains(),
        vec!["allow.example.com".to_string()]
    );
    assert_eq!(
        config.denied_domains(),
        vec!["deny.example.com".to_string()]
    );
}

/// Config round-trips through serde with `#[serde(default)]`.
/// Codex parity: `partial_network_config_uses_struct_defaults_for_missing_fields`
/// (config.rs:601).
#[test]
fn config_serde_roundtrip_and_defaults() {
    let json = r#"{"network":{"enabled":true}}"#;
    let cfg: NetworkProxyConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.network.enabled);
    assert_eq!(cfg.network.mode, NetworkMode::Full);
    assert!(cfg.network.domains.is_none());

    // Empty object -> all defaults.
    let empty: NetworkProxyConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(empty, NetworkProxyConfig::default());
    assert!(!empty.network.enabled);
}

/// Domain map serializes as a `{pattern: permission}` map (codex effective
/// shape, config.rs:50).
#[test]
fn domain_permissions_serialize_as_map() {
    let mut entries = BTreeMap::new();
    entries.insert("example.com".to_string(), NetworkDomainPermission::Deny);
    let perms = NetworkDomainPermissions { entries };
    let value = serde_json::to_value(&perms).unwrap();
    assert_eq!(value, serde_json::json!({"example.com": "deny"}));
}

// ---------------------------------------------------------------------------
// deferred network approval  (codex network_policy.rs)
// ---------------------------------------------------------------------------

fn config_with(allow: &[&str], deny: &[&str]) -> NetworkProxyConfig {
    let mut entries = BTreeMap::new();
    for a in allow {
        entries.insert((*a).to_string(), NetworkDomainPermission::Allow);
    }
    for d in deny {
        entries.insert((*d).to_string(), NetworkDomainPermission::Deny);
    }
    NetworkProxyConfig {
        network: NetworkProxySettings {
            enabled: true,
            mode: NetworkMode::Full,
            domains: Some(NetworkDomainPermissions { entries }),
        },
    }
}

fn req(host: &str) -> NetworkPolicyRequest {
    NetworkPolicyRequest {
        protocol: NetworkProtocol::Http,
        host: host.to_string(),
        port: 80,
        method: Some("GET".to_string()),
    }
}

/// Codex parity: the `Allowed` arm of `evaluate_host_policy` (network_policy.rs:296)
/// — host on the allowlist -> `Allow`.
#[test]
fn evaluate_allows_host_on_allowlist() {
    let config = config_with(&["example.com", "*.api.example.com"], &[]);
    assert_eq!(
        evaluate_request(&config, &req("example.com"), /*has_decider*/ true),
        NetworkDecision::Allow
    );
    assert_eq!(
        evaluate_request(&config, &req("v1.api.example.com"), true),
        NetworkDecision::Allow
    );
}

/// Codex parity: the not-allowed-with-decider arm (network_policy.rs:297-301,
/// `map_decider_decision` :361) — off-allowlist + decider -> deferred `Ask`
/// from the `Decider` source.
#[test]
fn evaluate_off_allowlist_with_decider_asks() {
    let config = config_with(&["example.com"], &[]);
    assert_eq!(
        evaluate_request(&config, &req("EVIL.com"), /*has_decider*/ true),
        NetworkDecision::Deny {
            reason: REASON_NOT_ALLOWED.to_string(),
            source: NetworkDecisionSource::Decider,
            decision: NetworkPolicyDecision::Ask,
        }
    );
}

/// Codex parity: the not-allowed-without-decider arm (network_policy.rs:302-309)
/// — off-allowlist + no decider -> hard `BaselinePolicy` `Deny`.
#[test]
fn evaluate_off_allowlist_without_decider_denies() {
    let config = config_with(&["example.com"], &[]);
    assert_eq!(
        evaluate_request(&config, &req("evil.com"), /*has_decider*/ false),
        NetworkDecision::Deny {
            reason: REASON_NOT_ALLOWED.to_string(),
            source: NetworkDecisionSource::BaselinePolicy,
            decision: NetworkPolicyDecision::Deny,
        }
    );
}

/// Codex parity: the `Blocked(reason)` arm (network_policy.rs:312-318) — a host
/// on the denylist is a hard `BaselinePolicy` `Deny`, even with a decider
/// present (deny wins, `config.rs:24`).
#[test]
fn evaluate_denylist_host_denies_even_with_decider() {
    let config = config_with(&["example.com"], &["blocked.example.com"]);
    assert_eq!(
        evaluate_request(
            &config,
            &req("blocked.example.com"),
            /*has_decider*/ true
        ),
        NetworkDecision::Deny {
            reason: REASON_DENIED.to_string(),
            source: NetworkDecisionSource::BaselinePolicy,
            decision: NetworkPolicyDecision::Deny,
        }
    );
}

/// Codex parity: `ask_uses_decider_source_and_ask_decision` (network_policy.rs:887).
#[test]
fn ask_constructor_uses_decider_source_and_ask() {
    assert_eq!(
        NetworkDecision::ask(REASON_NOT_ALLOWED),
        NetworkDecision::Deny {
            reason: REASON_NOT_ALLOWED.to_string(),
            source: NetworkDecisionSource::Decider,
            decision: NetworkPolicyDecision::Ask,
        }
    );
}

/// Resolving a pending `Ask` with `Allow` -> `Allow`, allowlist unchanged.
/// Codex parity: the `NetworkPolicyRuleAction::Allow` resolution
/// (core/src/tools/network_approval.rs:548).
#[test]
fn apply_response_allow_resolves_allow() {
    let config = config_with(&["example.com"], &[]);
    let decision = evaluate_request(&config, &req("evil.com"), true);
    let mut live = vec!["example.com".to_string()];
    let resolved = apply_response(decision, ApprovalResponse::Allow, "evil.com", &mut live);
    assert_eq!(resolved, NetworkDecision::Allow);
    assert_eq!(live, vec!["example.com".to_string()]);
}

/// Resolving an `Ask` with `AllowAndRemember` -> `Allow` AND host added to the
/// live allow set (codex appends an allow rule to the live policy).
#[test]
fn apply_response_allow_and_remember_adds_host() {
    let config = config_with(&["example.com"], &[]);
    let decision = evaluate_request(&config, &req("new.host"), true);
    let mut live = vec!["example.com".to_string()];
    let resolved = apply_response(
        decision,
        ApprovalResponse::AllowAndRemember,
        "NEW.host",
        &mut live,
    );
    assert_eq!(resolved, NetworkDecision::Allow);
    assert_eq!(
        live,
        vec!["example.com".to_string(), "new.host".to_string()]
    );
    assert!(host_matches_any("new.host", &live));
}

/// Resolving an `Ask` with `Deny` -> `Decider` hard `Deny`.
/// Codex parity: the `NetworkPolicyRuleAction::Deny` resolution
/// (core/src/tools/network_approval.rs:578).
#[test]
fn apply_response_deny_resolves_deny() {
    let config = config_with(&["example.com"], &[]);
    let decision = evaluate_request(&config, &req("evil.com"), true);
    let mut live = vec!["example.com".to_string()];
    let resolved = apply_response(decision, ApprovalResponse::Deny, "evil.com", &mut live);
    assert_eq!(
        resolved,
        NetworkDecision::Deny {
            reason: REASON_DENIED.to_string(),
            source: NetworkDecisionSource::Decider,
            decision: NetworkPolicyDecision::Deny,
        }
    );
}

/// A hard `Deny` bridges to the agent seam (`tools::runtime::NetworkPolicyDecision
/// { host }`, tools/runtime.rs:49); `Allow`/`Ask` do not.
#[test]
fn to_runtime_decision_bridges_only_hard_deny() {
    let config = config_with(&["example.com"], &[]);

    let deny = evaluate_request(&config, &req("EVIL.com"), /*has_decider*/ false);
    let runtime = to_runtime_decision(&deny, "EVIL.com").expect("hard deny -> Some");
    assert_eq!(runtime.host, "evil.com");

    let ask = evaluate_request(&config, &req("evil.com"), /*has_decider*/ true);
    assert!(to_runtime_decision(&ask, "evil.com").is_none());

    assert!(to_runtime_decision(&NetworkDecision::Allow, "example.com").is_none());
}

// ---------------------------------------------------------------------------
// loader / runtime plumbing (SEAM)  (codex proxy.rs / state.rs)
// ---------------------------------------------------------------------------

/// `load_network_proxy` returns `None` when disabled (codex gates on
/// `config.network.enabled`).
#[test]
fn load_network_proxy_disabled_returns_none() {
    let config = NetworkProxyConfig::disabled();
    assert!(load_network_proxy(&config).is_none());
}

/// `load_network_proxy` -> seam start: binds a loopback listener and asserts the
/// proxy_url / patterns / ca / handle plumbing. Binds only `127.0.0.1:0`; serves
/// NO traffic, makes NO outbound connection.
#[test]
fn load_network_proxy_enabled_binds_loopback_seam() {
    let config = config_with(&["example.com"], &["blocked.example.com"]);
    let runtime = load_network_proxy(&config)
        .expect("enabled -> Some")
        .expect("seam listener binds");

    assert!(
        runtime.proxy_url.starts_with("http://127.0.0.1:"),
        "proxy_url={}",
        runtime.proxy_url
    );
    assert_eq!(runtime.allow_patterns, vec!["example.com".to_string()]);
    assert_eq!(
        runtime.deny_patterns,
        vec!["blocked.example.com".to_string()]
    );
    assert!(runtime.allows_host("example.com"));
    assert!(!runtime.allows_host("evil.com"));
    // deny wins even if it were also on the allow set.
    assert!(!runtime.allows_host("blocked.example.com"));
    // ca_cert is the honest SEAM placeholder, not a real CA.
    assert_eq!(runtime.ca_cert_pem, SEAM_CA_PEM_PLACEHOLDER);
    assert!(runtime
        .ca_cert_pem
        .contains("SEAM_PLACEHOLDER_NOT_A_REAL_CA"));

    // Shutdown releases the port (idempotent).
    runtime.handle.shutdown();
    runtime.handle.shutdown();
}
