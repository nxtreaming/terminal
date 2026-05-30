//! Pure host-normalization + domain-pattern matching for the network proxy.
//!
//! This is the PURE, exhaustively-tested core of the subsystem. Every function
//! here mirrors the SEMANTICS of codex's `network-proxy/src/policy.rs` (cited
//! per item). No I/O, no network, no spawning.
//!
//! ## Parity note (read this)
//!
//! Codex's `policy.rs` compiles domain patterns into a `globset::GlobSet` and
//! validates URL hosts with the `url` crate. NEITHER `globset` NOR `url` is a
//! dependency of this workspace (verified: workspace `Cargo.toml` has only
//! `reqwest`/`toml`/… — no `url`, no `globset`). Rather than pull two heavy
//! crates in, this module reimplements the SAME pattern grammar + matching
//! semantics by hand (exact / `*.sub` / `**.apex+sub` / `*` global), which is a
//! faithful behavioral port of codex's pattern set — NOT the byte-identical
//! globset path. The one place codex's glob path is strictly more general is
//! mid-label wildcards (e.g. `region*.v2.example.com`,
//! `network-proxy/src/policy.rs:394`); those are NOT supported here and are
//! flagged as parity debt in [`host_matches_pattern`].
//!
//! Codex parity sources (`codex-rs/network-proxy/src/policy.rs`):
//! - `normalize_host` (:101)
//! - `is_loopback_host` (:33)
//! - the domain-pattern grammar: `DomainPattern::parse` (:237),
//!   `expand_domain_pattern` (:321), `normalize_pattern` (:150)
//! - `is_subdomain_or_equal` (:341) / `is_strict_subdomain` (:350)

use std::net::IpAddr;

/// Normalize a host for policy matching: trim, strip brackets, strip a single
/// `:port`, lowercase, strip a trailing `.`, and preserve unbracketed IPv6.
///
/// Codex parity: `normalize_host` in `network-proxy/src/policy.rs:101`.
/// - bracketed `[..]` -> inner host (loader.rs:103-107)
/// - exactly one `:` -> strip the `:port` suffix (:111-114), so unbracketed
///   IPv6 literals (multiple `:`) are preserved (:478 test).
/// - else lowercase + strip trailing dot (`normalize_dns_host_or_ip_literal`,
///   :121-128).
pub fn normalize_host(host: &str) -> String {
    let host = host.trim();
    if host.starts_with('[') {
        if let Some(end) = host.find(']') {
            return normalize_dns_host_or_ip_literal(&host[1..end]);
        }
    }
    // Strip `:port` only when there is exactly one `:` (defensive; avoids
    // mangling unbracketed IPv6 literals which contain several).
    if host.bytes().filter(|b| *b == b':').count() == 1 {
        let h = host.split(':').next().unwrap_or_default();
        return normalize_dns_host_or_ip_literal(h);
    }
    normalize_dns_host_or_ip_literal(host)
}

/// Codex parity: `normalize_dns_host_or_ip_literal`
/// (`network-proxy/src/policy.rs:121`): lowercase + strip trailing dot
/// (IP-literal canonicalization beyond that is omitted; we keep the literal
/// as-is, which matches codex for the cases we test).
fn normalize_dns_host_or_ip_literal(host: &str) -> String {
    let host = host.to_ascii_lowercase();
    host.trim_end_matches('.').to_string()
}

/// Whether `host` is a loopback hostname or IP literal.
///
/// Codex parity: `is_loopback_host` in `network-proxy/src/policy.rs:33`.
pub fn is_loopback_host(host: &str) -> bool {
    let normalized = normalize_host(host);
    // Strip an IPv6 zone id (`%scope`) before parsing, like codex
    // `unscoped_ip_literal` (policy.rs:130).
    let candidate = normalized
        .split_once('%')
        .map(|(ip, _)| ip)
        .unwrap_or(&normalized);
    if candidate == "localhost" {
        return true;
    }
    if let Ok(ip) = candidate.parse::<IpAddr>() {
        return ip.is_loopback();
    }
    false
}

/// A parsed allowlist/denylist domain pattern.
///
/// Codex parity: `DomainPattern` in `network-proxy/src/policy.rs:225` and the
/// pattern grammar described at `compile_globset_with_policy` (:206-210):
/// - `example.com`   -> [`DomainPattern::Exact`] (exact host)
/// - `*.example.com` -> [`DomainPattern::SubdomainsOnly`] (subdomains, NOT apex)
/// - `**.example.com`-> [`DomainPattern::ApexAndSubdomains`] (apex + subdomains)
/// - `*`             -> [`DomainPattern::Global`] (every host)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DomainPattern {
    /// `*` — matches every host. Codex: the `"*"` early-return in
    /// `normalize_pattern` (policy.rs:152) + global-wildcard handling.
    Global,
    /// `**.domain` — apex and any subdomain.
    /// Codex: `DomainPattern::ApexAndSubdomains` (policy.rs:227).
    ApexAndSubdomains(String),
    /// `*.domain` — subdomains only (NOT the apex).
    /// Codex: `DomainPattern::SubdomainsOnly` (policy.rs:228).
    SubdomainsOnly(String),
    /// `domain` — exact host.
    /// Codex: `DomainPattern::Exact` (policy.rs:229).
    Exact(String),
}

/// Parse a single pattern string into a [`DomainPattern`].
///
/// Codex parity: `normalize_pattern` (`policy.rs:150`) + `DomainPattern::parse`
/// (`policy.rs:237`). The domain remainder is run through [`normalize_host`]
/// (lowercase / strip port / strip trailing dot) exactly as codex does
/// (policy.rs:164).
pub fn parse_domain_pattern(pattern: &str) -> DomainPattern {
    let pattern = pattern.trim();
    if pattern == "*" {
        return DomainPattern::Global;
    }
    if let Some(rest) = pattern.strip_prefix("**.") {
        return DomainPattern::ApexAndSubdomains(normalize_host(rest));
    }
    if let Some(rest) = pattern.strip_prefix("*.") {
        return DomainPattern::SubdomainsOnly(normalize_host(rest));
    }
    DomainPattern::Exact(normalize_host(pattern))
}

/// Whether `host` matches `parent` exactly (after normalization).
///
/// Codex parity: `domain_eq` (`policy.rs:337`).
fn domain_eq(host: &str, parent: &str) -> bool {
    normalize_host(host) == normalize_host(parent)
}

/// Whether `child` is `parent` or a subdomain of it.
///
/// Codex parity: `is_subdomain_or_equal` (`policy.rs:341`).
fn is_subdomain_or_equal(child: &str, parent: &str) -> bool {
    let child = normalize_host(child);
    let parent = normalize_host(parent);
    child == parent || child.ends_with(&format!(".{parent}"))
}

/// Whether `child` is a strict subdomain of `parent` (not the apex).
///
/// Codex parity: `is_strict_subdomain` (`policy.rs:350`).
fn is_strict_subdomain(child: &str, parent: &str) -> bool {
    let child = normalize_host(child);
    let parent = normalize_host(parent);
    child != parent && child.ends_with(&format!(".{parent}"))
}

/// Whether a (raw) `host` matches a single (raw) `pattern`.
///
/// Codex parity: the expansion in `expand_domain_pattern` (`policy.rs:321`) as
/// applied by `compile_globset_with_policy` (:206-210):
/// - `*`              -> always matches
/// - `**.example.com` -> apex OR subdomain ([`is_subdomain_or_equal`])
/// - `*.example.com`  -> strict subdomain only ([`is_strict_subdomain`])
/// - `example.com`    -> exact ([`domain_eq`])
///
/// PARITY DEBT: codex's globset path also supports mid-label wildcards like
/// `region*.v2.example.com` (`policy.rs:394`). This hand-rolled matcher does
/// NOT; only the four prefix forms above are honored.
pub fn host_matches_pattern(host: &str, pattern: &str) -> bool {
    match parse_domain_pattern(pattern) {
        DomainPattern::Global => true,
        DomainPattern::ApexAndSubdomains(domain) => is_subdomain_or_equal(host, &domain),
        DomainPattern::SubdomainsOnly(domain) => is_strict_subdomain(host, &domain),
        DomainPattern::Exact(domain) => domain_eq(host, &domain),
    }
}

/// Whether `host` matches ANY pattern in `patterns`.
///
/// Codex parity: the `GlobSet::is_match` any-of semantics produced by
/// `compile_allowlist_globset` (`policy.rs:185`).
pub fn host_matches_any(host: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| host_matches_pattern(host, p))
}
