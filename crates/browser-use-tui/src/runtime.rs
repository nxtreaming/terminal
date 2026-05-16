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
    session_id: &str,
    browser_use_cloud_api_key: Option<&str>,
) -> AgentRunOptions {
    match browser {
        "Headless Chromium" => AgentRunOptions::default()
            .with_browser_mode("headless")
            .with_python_env(managed_browser_env(session_id, false)),
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
        _ => AgentRunOptions::default()
            .with_browser_mode("local")
            .with_python_env(managed_browser_env(session_id, true)),
    }
}

fn clear_cdp_env() -> Vec<(String, String)> {
    [("BU_CDP_URL", ""), ("BU_CDP_WS", ""), ("BU_BROWSER_ID", "")]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn managed_browser_env(session_id: &str, visible: bool) -> Vec<(String, String)> {
    let mut env = clear_cdp_env();
    let daemon_name = format!("but-tui-{}", safe_env_segment(session_id));
    let runtime_dir = format!("/tmp/{daemon_name}");
    env.extend([
        ("BU_NAME".to_string(), daemon_name),
        ("BH_RUNTIME_DIR".to_string(), runtime_dir.clone()),
        ("BH_TMP_DIR".to_string(), runtime_dir),
        ("LLM_BROWSER_AUTO_CHROME".to_string(), "1".to_string()),
    ]);
    if visible {
        env.push((
            "LLM_BROWSER_MANAGED_CHROME_VISIBLE".to_string(),
            "1".to_string(),
        ));
    }
    env
}

fn safe_env_segment(value: &str) -> String {
    let segment = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if segment.is_empty() {
        "session".to_string()
    } else {
        segment
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
        assert_eq!(env_value(&options, "BU_CDP_URL"), Some(""));
        assert_eq!(env_value(&options, "BU_CDP_WS"), Some(""));
        assert_eq!(env_value(&options, "BU_BROWSER_ID"), Some(""));
        assert_eq!(env_value(&options, "LLM_BROWSER_AUTO_CHROME"), Some("1"));
        assert_eq!(
            env_value(&options, "LLM_BROWSER_MANAGED_CHROME_VISIBLE"),
            Some("1")
        );
        assert_eq!(env_value(&options, "BU_NAME"), Some("but-tui-abc123"));
        assert_eq!(
            env_value(&options, "BH_RUNTIME_DIR"),
            Some("/tmp/but-tui-abc123")
        );
    }

    #[test]
    fn headless_chromium_uses_managed_browser_not_inherited_cdp() {
        let options = tui_agent_options("Headless Chromium", "abc123", None);
        assert_eq!(options.browser_mode.as_deref(), Some("headless"));
        assert_eq!(env_value(&options, "BU_CDP_URL"), Some(""));
        assert_eq!(env_value(&options, "BU_CDP_WS"), Some(""));
        assert_eq!(env_value(&options, "BU_BROWSER_ID"), Some(""));
        assert_eq!(env_value(&options, "BU_NAME"), Some("but-tui-abc123"));
        assert_eq!(env_value(&options, "LLM_BROWSER_AUTO_CHROME"), Some("1"));
        assert_eq!(
            env_value(&options, "LLM_BROWSER_MANAGED_CHROME_VISIBLE"),
            None
        );
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

    #[test]
    fn local_chrome_sanitizes_session_id_for_daemon_name() {
        let options = tui_agent_options("Local Chrome", "abc/123 !?", None);
        assert_eq!(env_value(&options, "BU_NAME"), Some("but-tui-abc-123"));
        assert_eq!(
            env_value(&options, "BH_RUNTIME_DIR"),
            Some("/tmp/but-tui-abc-123")
        );
    }
}
