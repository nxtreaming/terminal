//! Product analytics: PostHog event capture.
//!
//! Ported faithfully from `browser-use-core`'s `product_analytics.rs`
//! (`crates/browser-use-core/src/product_analytics.rs`). It captures anonymous
//! product-usage events for PostHog. It is best-effort: all failures are
//! swallowed, and capture is globally suppressed in several cases.
//!
//! ## Offline / network safety in tests
//!
//! This mirrors the legacy core exactly: [`analytics_disabled`] returns `true`
//! whenever `cfg!(test)` is set, so **every `#[test]`/`#[tokio::test]` in this
//! crate is inherently offline** — `capture_async` / `capture_blocking` return
//! before any `reqwest` call. The unit tests below therefore exercise only the
//! payload SHAPE (`event_properties`) and the pure bucketing/classification
//! helpers; none of them touch the network.
//!
//! The real send uses `reqwest::blocking`, which is already a dependency of this
//! crate (workspace `reqwest` enables the `blocking` + `json` features), so no
//! new dependency is introduced.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use browser_use_store::Store;
use serde_json::{json, Map, Value};

const PROD_POSTHOG_KEY: &str = "phc_F8JMNjW1i2KbGUTaW1unnDdLSPCoyc52SGRU0JecaUh";
const DEV_POSTHOG_KEY: &str = "phc_zA2V4ziA7SjefWYGP4Gg9CCJj9r25rPiG5c926aKhGTG";
const DEFAULT_POSTHOG_HOST: &str = "https://eu.i.posthog.com";
const INSTALL_ID_RELATIVE_PATH: &[&str] = &["product_analytics", "install_id"];
const DEFAULT_TIMEOUT_MS: u64 = 800;

/// Capture an event on a detached background thread (fire-and-forget).
///
/// Mirrors `browser-use-core::product_analytics::capture_async`
/// (`crates/browser-use-core/src/product_analytics.rs:16`).
pub fn capture_async(store: &Store, event: impl Into<String>, properties: Value) {
    if analytics_disabled() {
        return;
    }
    let state_dir = store.state_dir().to_path_buf();
    let event = event.into();
    thread::Builder::new()
        .name("browser-use-product-analytics".to_string())
        .spawn(move || {
            let _ = capture_for_state_dir(&state_dir, &event, properties);
        })
        .ok();
}

/// Capture an event synchronously on the calling thread.
///
/// Mirrors `browser-use-core::product_analytics::capture_blocking`
/// (`crates/browser-use-core/src/product_analytics.rs:30`).
pub fn capture_blocking(store: &Store, event: &str, properties: Value) {
    if analytics_disabled() {
        return;
    }
    let _ = capture_for_state_dir(store.state_dir(), event, properties);
}

/// Build the payload and POST it to PostHog.
///
/// Mirrors `browser-use-core::product_analytics::capture_for_state_dir`
/// (`crates/browser-use-core/src/product_analytics.rs:37`). Only reachable when
/// [`analytics_disabled`] is `false`, so it never runs under `cfg!(test)`.
fn capture_for_state_dir(
    state_dir: &Path,
    event: &str,
    properties: Value,
) -> Result<(), AnalyticsError> {
    let Some(api_key) = posthog_key() else {
        return Ok(());
    };
    let install_id = install_id(state_dir)?;
    let payload = json!({
        "api_key": api_key,
        "event": event,
        "distinct_id": install_id,
        "properties": event_properties(properties),
    });
    let endpoint = format!("{}/i/v0/e/", posthog_host().trim_end_matches('/'));
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(posthog_timeout_ms()))
        .build()
        .map_err(AnalyticsError::Http)?
        .post(endpoint)
        .json(&payload)
        .send()
        .map_err(AnalyticsError::Http)?
        .error_for_status()
        .map_err(AnalyticsError::Http)?;
    Ok(())
}

/// Decorate caller-supplied properties with the standard anonymous metadata.
///
/// Mirrors `browser-use-core::product_analytics::event_properties`
/// (`crates/browser-use-core/src/product_analytics.rs:62`).
fn event_properties(properties: Value) -> Value {
    let mut object = match properties {
        Value::Object(object) => object,
        _ => Map::new(),
    };
    object.insert("$process_person_profile".to_string(), Value::Bool(false));
    object.insert(
        "analytics_env".to_string(),
        Value::String(analytics_env().to_string()),
    );
    object.insert(
        "app".to_string(),
        Value::String("browser-use-terminal".to_string()),
    );
    object.insert(
        "app_version".to_string(),
        Value::String(env!("CARGO_PKG_VERSION").to_string()),
    );
    object.insert(
        "os".to_string(),
        Value::String(std::env::consts::OS.to_string()),
    );
    object.insert(
        "arch".to_string(),
        Value::String(std::env::consts::ARCH.to_string()),
    );
    object.insert(
        "debug_build".to_string(),
        Value::Bool(cfg!(debug_assertions)),
    );
    Value::Object(object)
}

/// Read or lazily create the per-install anonymous id.
///
/// Mirrors `browser-use-core::product_analytics::install_id`
/// (`crates/browser-use-core/src/product_analytics.rs:95`). The legacy core
/// used `uuid::Uuid`; to avoid adding a `uuid` dependency to this crate the id
/// is generated from a random + time seed in [`generate_install_id`], keeping
/// the same `bu_<hex>` shape.
fn install_id(state_dir: &Path) -> Result<String, AnalyticsError> {
    let path = install_id_path(state_dir);
    if let Ok(value) = std::fs::read_to_string(&path) {
        let value = value.trim();
        if !value.is_empty() {
            return Ok(value.to_string());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(AnalyticsError::Io)?;
    }
    let id = generate_install_id();
    std::fs::write(&path, format!("{id}\n")).map_err(AnalyticsError::Io)?;
    Ok(id)
}

/// Generate a fresh anonymous install id of the form `bu_<32 hex>`.
fn generate_install_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(3 + 32);
    out.push_str("bu_");
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Compute the on-disk path of the install id under `state_dir`.
///
/// Mirrors `browser-use-core::product_analytics::install_id_path`
/// (`crates/browser-use-core/src/product_analytics.rs:113`).
fn install_id_path(state_dir: &Path) -> PathBuf {
    INSTALL_ID_RELATIVE_PATH
        .iter()
        .fold(state_dir.to_path_buf(), |path, segment| path.join(segment))
}

/// Whether analytics should be suppressed.
///
/// Mirrors `browser-use-core::product_analytics::analytics_disabled`
/// (`crates/browser-use-core/src/product_analytics.rs:119`): always disabled
/// under `cfg!(test)`, or when `BUT_TELEMETRY` / `BUT_PRODUCT_ANALYTICS` is set
/// to a false-y value.
fn analytics_disabled() -> bool {
    cfg!(test) || env_flag_is_false("BUT_TELEMETRY") || env_flag_is_false("BUT_PRODUCT_ANALYTICS")
}

/// Resolve the PostHog project key.
///
/// Mirrors `browser-use-core::product_analytics::posthog_key`
/// (`crates/browser-use-core/src/product_analytics.rs:123`).
fn posthog_key() -> Option<String> {
    env_value("BUT_POSTHOG_KEY").or_else(|| match analytics_env().as_str() {
        "development" | "dev" => Some(DEV_POSTHOG_KEY.to_string()),
        "production" | "prod" => Some(PROD_POSTHOG_KEY.to_string()),
        _ if cfg!(debug_assertions) => Some(DEV_POSTHOG_KEY.to_string()),
        _ => Some(PROD_POSTHOG_KEY.to_string()),
    })
}

/// Resolve the PostHog host.
///
/// Mirrors `browser-use-core::product_analytics::posthog_host`
/// (`crates/browser-use-core/src/product_analytics.rs:132`).
fn posthog_host() -> String {
    env_value("BUT_POSTHOG_HOST").unwrap_or_else(|| DEFAULT_POSTHOG_HOST.to_string())
}

/// Resolve the analytics environment label.
///
/// Mirrors `browser-use-core::product_analytics::analytics_env`
/// (`crates/browser-use-core/src/product_analytics.rs:136`).
fn analytics_env() -> String {
    env_value("BUT_ANALYTICS_ENV").unwrap_or_else(|| {
        if cfg!(debug_assertions) {
            "development".to_string()
        } else {
            "production".to_string()
        }
    })
}

/// Resolve the PostHog request timeout in milliseconds.
///
/// Mirrors `browser-use-core::product_analytics::posthog_timeout_ms`
/// (`crates/browser-use-core/src/product_analytics.rs:146`).
fn posthog_timeout_ms() -> u64 {
    env_value("BUT_POSTHOG_TIMEOUT_MS")
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

/// Read a trimmed, non-empty environment variable.
///
/// Mirrors `browser-use-core::product_analytics::env_value`
/// (`crates/browser-use-core/src/product_analytics.rs:153`).
fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Whether an environment flag is set to a false-y value.
///
/// Mirrors `browser-use-core::product_analytics::env_flag_is_false`
/// (`crates/browser-use-core/src/product_analytics.rs:160`).
fn env_flag_is_false(name: &str) -> bool {
    matches!(
        env_value(name)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// Bucket a duration into a coarse, non-identifying label.
///
/// Mirrors `browser-use-core::product_analytics::duration_bucket`
/// (`crates/browser-use-core/src/product_analytics.rs:170`).
pub fn duration_bucket(duration: Duration) -> &'static str {
    let seconds = duration.as_secs();
    if seconds < 10 {
        "<10s"
    } else if seconds < 60 {
        "10-60s"
    } else if seconds < 300 {
        "1-5m"
    } else if seconds < 900 {
        "5-15m"
    } else {
        ">15m"
    }
}

/// Classify a browser mode string into a coarse kind.
///
/// Mirrors `browser-use-core::product_analytics::browser_kind`
/// (`crates/browser-use-core/src/product_analytics.rs:185`).
pub fn browser_kind(mode: Option<&str>) -> &'static str {
    let Some(mode) = mode else {
        return "unknown";
    };
    let normalized = mode.to_ascii_lowercase().replace(['_', ' '], "-");
    match normalized.as_str() {
        "local" | "local-chrome" => "local",
        "headless" | "headless-chromium" | "managed-headless" => "headless",
        "managed" | "managed-headed" => "managed",
        "cloud" | "browser-use-cloud" => "cloud",
        _ => "other",
    }
}

/// Error type for the analytics capture path.
///
/// The legacy core used `anyhow::Result`; this leaf module uses a typed error to
/// avoid pulling `anyhow` into its non-test surface. All variants are swallowed
/// by the public `capture_*` entry points (best-effort semantics preserved).
#[derive(Debug)]
pub enum AnalyticsError {
    /// Filesystem error while reading/writing the install id.
    Io(std::io::Error),
    /// HTTP error while building/sending the PostHog request.
    Http(reqwest::Error),
}

impl std::fmt::Display for AnalyticsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalyticsError::Io(e) => write!(f, "product analytics io error: {e}"),
            AnalyticsError::Http(e) => write!(f, "product analytics http error: {e}"),
        }
    }
}

impl std::error::Error for AnalyticsError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analytics_is_disabled_under_cfg_test() {
        // The whole point of the offline guarantee: in test builds, capture is
        // suppressed before any network call.
        assert!(analytics_disabled());
    }

    #[test]
    fn event_properties_force_anonymous_capture() {
        let properties = event_properties(json!({"surface": "tui"}));
        assert_eq!(properties["$process_person_profile"], false);
        assert_eq!(properties["surface"], "tui");
        assert_eq!(properties["app"], "browser-use-terminal");
    }

    #[test]
    fn event_properties_has_full_metadata_shape() {
        let properties = event_properties(json!({}));
        for key in [
            "$process_person_profile",
            "analytics_env",
            "app",
            "app_version",
            "os",
            "arch",
            "debug_build",
        ] {
            assert!(properties.get(key).is_some(), "missing key {key}");
        }
    }

    #[test]
    fn event_properties_replaces_non_object_input() {
        // Non-object input is discarded and replaced with the metadata object.
        let properties = event_properties(json!("not-an-object"));
        assert!(properties.is_object());
        assert_eq!(properties["app"], "browser-use-terminal");
    }

    #[test]
    fn install_id_path_appends_relative_segments() {
        let base = Path::new("/tmp/state");
        let path = install_id_path(base);
        assert!(path.ends_with("product_analytics/install_id"));
        assert!(path.starts_with(base));
    }

    #[test]
    fn generated_install_id_has_expected_shape() {
        let id = generate_install_id();
        assert!(id.starts_with("bu_"));
        assert_eq!(id.len(), 3 + 32);
        assert!(id[3..].chars().all(|c| c.is_ascii_hexdigit()));
        // Two consecutive ids should not collide.
        assert_ne!(generate_install_id(), generate_install_id());
    }

    #[test]
    fn buckets_duration_without_exact_values() {
        assert_eq!(duration_bucket(Duration::from_secs(0)), "<10s");
        assert_eq!(duration_bucket(Duration::from_secs(10)), "10-60s");
        assert_eq!(duration_bucket(Duration::from_secs(60)), "1-5m");
        assert_eq!(duration_bucket(Duration::from_secs(300)), "5-15m");
        assert_eq!(duration_bucket(Duration::from_secs(900)), ">15m");
    }

    #[test]
    fn browser_kind_normalizes_modes() {
        assert_eq!(browser_kind(Some("Local Chrome")), "local");
        assert_eq!(browser_kind(Some("managed-headless")), "headless");
        assert_eq!(browser_kind(Some("cloud")), "cloud");
        assert_eq!(browser_kind(Some("weird")), "other");
        assert_eq!(browser_kind(None), "unknown");
    }
}
