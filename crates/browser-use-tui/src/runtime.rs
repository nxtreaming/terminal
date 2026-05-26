use std::path::PathBuf;

use anyhow::{bail, Result};
use browser_use_core::{
    run_existing_session_from_config, AgentRunOptions, CollaborationModeKind, ConfigOverrides,
    ProviderRunConfig,
};
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
    model_provider_id: Option<String>,
    browser: String,
    collaboration_mode: CollaborationModeKind,
    config_profile: Option<String>,
    config_overrides: ConfigOverrides,
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
            collaboration_mode,
            model_provider_id.as_deref(),
            browser_use_cloud_api_key.as_deref(),
            config_profile,
            config_overrides,
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
    collaboration_mode: CollaborationModeKind,
    model_provider_id: Option<&str>,
    browser_use_cloud_api_key: Option<&str>,
    config_profile: Option<String>,
    config_overrides: ConfigOverrides,
) -> AgentRunOptions {
    let mut options = match browser {
        "Headless Chromium" => AgentRunOptions::default()
            .with_collaboration_mode(collaboration_mode)
            .with_browser_mode("managed-headless")
            .with_model_compaction(true)
            .with_analytics_source("tui"),
        BROWSER_USE_CLOUD => {
            let mut options = AgentRunOptions::default()
                .with_collaboration_mode(collaboration_mode)
                .with_browser_mode("cloud")
                .with_model_compaction(true)
                .with_analytics_source("tui");
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
        _ => AgentRunOptions::default()
            .with_collaboration_mode(collaboration_mode)
            .with_browser_mode("local")
            .with_model_compaction(true)
            .with_analytics_source("tui"),
    };
    if let Some(profile) = config_profile {
        options = options.with_config_profile(profile);
    }
    if !config_overrides.is_empty() {
        options = options.with_config_overrides(config_overrides);
    }
    if let Some(model_provider_id) = model_provider_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        options.with_model_provider_id(model_provider_id.to_string())
    } else {
        options
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
        let options = tui_agent_options(
            "Local Chrome",
            "abc123",
            CollaborationModeKind::Default,
            Some("codex"),
            None,
            None,
            Vec::new(),
        );
        assert_eq!(options.browser_mode.as_deref(), Some("local"));
        assert!(options.python_env.is_empty());
    }

    #[test]
    fn headless_chromium_uses_managed_browser_not_inherited_cdp() {
        let options = tui_agent_options(
            "Headless Chromium",
            "abc123",
            CollaborationModeKind::Default,
            Some("codex"),
            None,
            None,
            Vec::new(),
        );
        assert_eq!(options.browser_mode.as_deref(), Some("managed-headless"));
        assert!(options.python_env.is_empty());
    }

    #[test]
    fn tui_agent_options_pass_collaboration_mode_to_core() {
        let options = tui_agent_options(
            "Local Chrome",
            "abc123",
            CollaborationModeKind::Plan,
            Some("codex"),
            None,
            None,
            Vec::new(),
        );
        assert_eq!(options.collaboration_mode, CollaborationModeKind::Plan);
        assert_eq!(options.model_provider_id.as_deref(), Some("codex"));
    }

    #[test]
    fn browser_use_cloud_keeps_cloud_mode() {
        let options = tui_agent_options(
            "Browser Use cloud",
            "abc123",
            CollaborationModeKind::Default,
            Some("codex"),
            None,
            None,
            Vec::new(),
        );
        assert_eq!(options.browser_mode.as_deref(), Some("cloud"));
        assert!(options.python_env.is_empty());
    }

    #[test]
    fn browser_use_cloud_passes_stored_key_to_worker_env() {
        let options = tui_agent_options(
            "Browser Use cloud",
            "abc123",
            CollaborationModeKind::Default,
            Some("codex"),
            Some("bu-test"),
            None,
            Vec::new(),
        );
        assert_eq!(options.browser_mode.as_deref(), Some("cloud"));
        assert_eq!(
            env_value(&options, BROWSER_USE_CLOUD_API_KEY_ENV),
            Some("bu-test")
        );
    }

    #[test]
    fn tui_agent_options_leaves_provider_id_unset_for_config_resolution() {
        let options = tui_agent_options(
            "Local Chrome",
            "abc123",
            CollaborationModeKind::Default,
            None,
            None,
            None,
            Vec::new(),
        );
        assert_eq!(options.browser_mode.as_deref(), Some("local"));
        assert_eq!(options.model_provider_id, None);
    }

    #[test]
    fn tui_agent_options_pass_profile_and_config_overrides_to_core() {
        let config_overrides = browser_use_core::parse_config_overrides(&[
            "developer_instructions=\"Stay precise.\"".to_string(),
        ])
        .expect("valid config override");
        let options = tui_agent_options(
            "Local Chrome",
            "abc123",
            CollaborationModeKind::Default,
            Some("codex"),
            None,
            Some("work".to_string()),
            config_overrides,
        );

        assert_eq!(options.config_profile.as_deref(), Some("work"));
        assert_eq!(
            options
                .config_overrides
                .iter()
                .find(|(key, _)| key == "developer_instructions")
                .and_then(|(_, value)| value.as_str()),
            Some("Stay precise.")
        );
    }
}
