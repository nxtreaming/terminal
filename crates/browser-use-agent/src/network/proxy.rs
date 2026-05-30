//! Network-proxy runtime, handle, and loader plumbing.
//!
//! Codex parity: `codex-rs/network-proxy/src/proxy.rs` (`NetworkProxy`,
//! `NetworkProxyBuilder`, `NetworkProxyHandle`, exported at
//! `network-proxy/src/lib.rs:47-49`) and the state plumbing in
//! `network-proxy/src/state.rs` / `runtime.rs` (`NetworkProxyState`,
//! `build_config_state`, `lib.rs:62,68`).
//!
//! ## HONESTY: what is REAL vs what is a SEAM
//!
//! REAL (fully implemented + tested):
//! - The allowlist/normalize core (`super::allowlist`), config
//!   (`super::config`), and deferred network-approval decision logic
//!   (`super::approval`).
//! - The runtime/handle plumbing here: building the active allow/deny pattern
//!   set from config, a `proxy_url`, a CA-pem field, a shutdown handle, and
//!   (when started) binding a real loopback `127.0.0.1:0` `TcpListener` so the
//!   handle / `proxy_url` / shutdown path is genuinely exercised.
//!
//! SEAM / PARITY DEBT (NOT a working MITM):
//! - There is NO real MITM HTTP/CONNECT interception, NO TLS termination, NO
//!   SOCKS5, and NO real certificate-authority generation. Codex's real proxy
//!   (`NetworkProxy`/`NetworkProxyBuilder`, `network-proxy/src/proxy.rs`) plus
//!   its `certs` module are heavy and depend on `hyper`/TLS/cert crates that
//!   are NOT in this workspace (verified: no `rcgen`/`hudsucker`/MITM crate,
//!   and not even `url`/`globset`). Bringing up a real intercepting proxy is
//!   deliberately OUT OF SCOPE.
//! - [`generate_seam_ca_pem`] returns a clearly-labeled PLACEHOLDER, not a real
//!   CA. Off-allowlist enforcement is provided by the pure `super::approval`
//!   decision layer, NOT on the wire.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context as _;
use anyhow::Result;

use super::config::NetworkProxyConfig;

/// Clearly-labeled placeholder marking the CA-generation seam.
///
/// Codex parity DEBT: codex generates a real CA (its `certs` module, used by
/// `NetworkProxy`) and serves the cert PEM to clients. We have no cert crate in
/// the workspace, so this is a non-functional placeholder. NEVER trust it.
pub const SEAM_CA_PEM_PLACEHOLDER: &str =
    "-----BEGIN CERTIFICATE-----\nSEAM_PLACEHOLDER_NOT_A_REAL_CA\n-----END CERTIFICATE-----\n";

/// Return the seam CA PEM placeholder. See [`SEAM_CA_PEM_PLACEHOLDER`].
pub fn generate_seam_ca_pem() -> String {
    SEAM_CA_PEM_PLACEHOLDER.to_string()
}

/// Handle controlling the lifetime of a started (seam) proxy.
///
/// Codex parity: `NetworkProxyHandle` (`network-proxy/src/proxy.rs`, exported
/// `lib.rs:49`) which owns the running server + a shutdown signal. Here the
/// "server" is a SEAM, so the handle owns the bound loopback listener;
/// [`NetworkProxyHandle::shutdown`] releases the port. There is no serve loop.
pub struct NetworkProxyHandle {
    listener: Mutex<Option<std::net::TcpListener>>,
}

impl NetworkProxyHandle {
    /// Shut the proxy down, releasing the bound port.
    ///
    /// Codex parity: the shutdown path of `NetworkProxyHandle`. Idempotent.
    pub fn shutdown(&self) {
        let _ = self.listener.lock().expect("proxy handle mutex").take();
    }
}

/// Running network-proxy runtime.
///
/// Codex parity: the bundle codex hands callers after starting the proxy — the
/// bound proxy URL, the CA PEM, the active allow/deny patterns, and a handle.
/// (Codex spreads these across `NetworkProxy` + `NetworkProxyState`; we collect
/// the parts the agent needs into one struct, analogous to the
/// `NetworkProxyRuntime` shape.)
#[derive(Clone)]
pub struct NetworkProxyRuntime {
    /// `http://<bound-addr>` of the (seam) proxy listener.
    pub proxy_url: String,
    /// CA PEM. SEAM placeholder — see [`generate_seam_ca_pem`].
    pub ca_cert_pem: String,
    /// Active allow patterns (from config).
    pub allow_patterns: Vec<String>,
    /// Active deny patterns (from config).
    pub deny_patterns: Vec<String>,
    /// Handle controlling the runtime lifetime.
    pub handle: Arc<NetworkProxyHandle>,
}

impl NetworkProxyRuntime {
    /// Whether `host` is permitted by this runtime's allow patterns AND not on
    /// its deny patterns (deny wins).
    ///
    /// Codex parity: the allow/deny precedence of `NetworkProxyState`
    /// host-block evaluation (`network-proxy/src/state.rs`, consumed by
    /// `evaluate_host_policy`, `network_policy.rs:294`).
    pub fn allows_host(&self, host: &str) -> bool {
        use super::allowlist::host_matches_any;
        !host_matches_any(host, &self.deny_patterns) && host_matches_any(host, &self.allow_patterns)
    }
}

/// Load + start the network proxy if enabled, else `None`.
///
/// Codex parity: `load_network_proxy` returns `None` when the network proxy is
/// disabled, otherwise `Some(start)` (the codex loader gates on
/// `config.network.enabled`). Here `start` is the seam in
/// [`start_network_proxy`].
pub fn load_network_proxy(config: &NetworkProxyConfig) -> Option<Result<NetworkProxyRuntime>> {
    if !config.network.enabled {
        return None;
    }
    Some(start_network_proxy(config))
}

/// Build the active patterns + (seam) start the proxy.
///
/// Codex parity: the codex start path builds the policy state from config and
/// spawns the proxy server. Our equivalent extracts the allow/deny patterns and
/// binds a loopback listener (the SEAM) instead of running a real MITM server.
pub fn start_network_proxy(config: &NetworkProxyConfig) -> Result<NetworkProxyRuntime> {
    let allow_patterns = config.allowed_domains();
    let deny_patterns = config.denied_domains();
    let ca_cert_pem = generate_seam_ca_pem();

    // Codex default proxy bind is loopback (`config.rs:296`,
    // `http://127.0.0.1:3128`). We bind an ephemeral loopback port so the
    // handle/url plumbing is real; nothing is served on it.
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("valid loopback addr");
    let (bound, listener) = bind_seam_listener(bind_addr)?;

    Ok(NetworkProxyRuntime {
        proxy_url: format!("http://{bound}"),
        ca_cert_pem,
        allow_patterns,
        deny_patterns,
        handle: Arc::new(NetworkProxyHandle {
            listener: Mutex::new(Some(listener)),
        }),
    })
}

/// SEAM: bind a loopback listener so the handle/url plumbing is real.
///
/// Codex parity DEBT for the real proxy server bind. This binds a real
/// `TcpListener` on `bind_addr` (loopback) and reports the bound `SocketAddr`,
/// but NOTHING is served on it — no accept loop, no TLS, no MITM. It exists
/// only so `proxy_url` and the shutdown handle are genuinely exercised.
fn bind_seam_listener(bind_addr: SocketAddr) -> Result<(SocketAddr, std::net::TcpListener)> {
    let listener =
        std::net::TcpListener::bind(bind_addr).context("failed to bind seam proxy listener")?;
    let bound = listener
        .local_addr()
        .context("failed to read seam proxy listener addr")?;
    Ok((bound, listener))
}
