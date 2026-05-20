use std::path::PathBuf;

use anyhow::{bail, Result};
use browser_use_core::{run_existing_session_from_config, AgentRunOptions, ProviderRunConfig};
use browser_use_store::{Store, StoreNotifier};

use crate::settings::{
    browser_use_cloud_env_key_present, AgentBackend, BROWSER_USE_CLOUD,
    BROWSER_USE_CLOUD_API_KEY_ENV, BROWSER_USE_CLOUD_API_KEY_SETTING,
};

pub(crate) fn run_agent_thread(
    state_dir: PathBuf,
    session_id: String,
    backend: AgentBackend,
    model: String,
    browser: String,
    notifier: Option<StoreNotifier>,
) -> Result<()> {
    let store = Store::open_with_optional_notifier(&state_dir, notifier)?;
    let browser_use_cloud_api_key = if browser == BROWSER_USE_CLOUD {
        browser_use_cloud_api_key(&store)?
    } else {
        None
    };
    if browser == BROWSER_USE_CLOUD && browser_use_cloud_api_key.is_none() {
        let error = "Browser Use cloud selected, but BROWSER_USE_API_KEY is not set";
        let _ = store.append_event(
            &session_id,
            "session.failed",
            serde_json::json!({ "error": error }),
        );
        bail!(error);
    }
    if let Some(api_key) = browser_use_cloud_api_key
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        // Browser runtime is Rust-owned now, so the cloud API key must also be
        // visible to Rust-side Browser Use API calls, not only the legacy Python worker.
        std::env::set_var(BROWSER_USE_CLOUD_API_KEY_ENV, api_key);
    }
    let config = ProviderRunConfig::new(backend.into(), model)
        .with_options(tui_agent_options(
            &browser,
            &session_id,
            browser_use_cloud_api_key.as_deref(),
        ))
        .with_fake_result("Fake result from the Rust TUI agent loop.");
    let result = run_existing_session_from_config(&store, &session_id, config);
    if let Err(error) = result {
        let _ = store.append_event(
            &session_id,
            "session.failed",
            serde_json::json!({ "error": error.to_string() }),
        );
        return Err(error);
    }
    Ok(())
}

fn browser_use_cloud_api_key(store: &Store) -> Result<Option<String>> {
    if let Some(value) = store
        .get_setting(BROWSER_USE_CLOUD_API_KEY_SETTING)?
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(Some(value));
    }
    if browser_use_cloud_env_key_present() {
        return Ok(std::env::var(BROWSER_USE_CLOUD_API_KEY_ENV).ok());
    }
    Ok(None)
}

fn tui_agent_options(
    browser: &str,
    _session_id: &str,
    browser_use_cloud_api_key: Option<&str>,
) -> AgentRunOptions {
    match browser {
        "Headless Chromium" => AgentRunOptions::default().with_browser_mode("managed-headless"),
        BROWSER_USE_CLOUD => {
            let mut options = AgentRunOptions::default().with_browser_mode("cloud");
            if let Some(api_key) =
                browser_use_cloud_api_key.filter(|value| !value.trim().is_empty())
            {
                options = options.with_python_env(vec![(
                    BROWSER_USE_CLOUD_API_KEY_ENV.to_string(),
                    api_key.to_string(),
                )]);
            }
            options
        }
        _ => AgentRunOptions::default().with_browser_mode("local"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_value<'a>(options: &'a AgentRunOptions, key: &str) -> Option<&'a str> {
        options
            .python_env
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn local_chrome_overrides_cloud_dotenv_mode() {
        let options = tui_agent_options("Local Chrome", "abc123", None);
        assert_eq!(options.browser_mode.as_deref(), Some("local"));
        assert!(options.python_env.is_empty());
    }

    #[test]
    fn headless_chromium_uses_managed_browser_not_inherited_cdp() {
        let options = tui_agent_options("Headless Chromium", "abc123", None);
        assert_eq!(options.browser_mode.as_deref(), Some("managed-headless"));
        assert!(options.python_env.is_empty());
    }

    #[test]
    fn browser_use_cloud_keeps_cloud_mode() {
        let options = tui_agent_options("Browser Use cloud", "abc123", None);
        assert_eq!(options.browser_mode.as_deref(), Some("cloud"));
        assert!(options.python_env.is_empty());
    }

    #[test]
    fn browser_use_cloud_passes_stored_key_to_worker_env() {
        let options = tui_agent_options("Browser Use cloud", "abc123", Some("bu-test"));
        assert_eq!(options.browser_mode.as_deref(), Some("cloud"));
        assert_eq!(
            env_value(&options, BROWSER_USE_CLOUD_API_KEY_ENV),
            Some("bu-test")
        );
    }
}
