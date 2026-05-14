use std::path::PathBuf;

use anyhow::Result;
use browser_use_core::{run_existing_session_from_config, AgentRunOptions, ProviderRunConfig};
use browser_use_store::{Store, StoreNotifier};

use crate::settings::AgentBackend;

pub(crate) fn run_agent_thread(
    state_dir: PathBuf,
    session_id: String,
    backend: AgentBackend,
    model: String,
    browser: String,
    notifier: Option<StoreNotifier>,
) -> Result<()> {
    let store = Store::open_with_optional_notifier(&state_dir, notifier)?;
    let config = ProviderRunConfig::new(backend.into(), model)
        .with_options(tui_agent_options(&browser, &session_id))
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

fn tui_agent_options(browser: &str, session_id: &str) -> AgentRunOptions {
    match browser {
        "Headless Chromium" => AgentRunOptions::default()
            .with_browser_mode("headless")
            .with_python_env(managed_browser_env(session_id, false)),
        "Browser Use cloud" => AgentRunOptions::default().with_browser_mode("cloud"),
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
        let options = tui_agent_options("Local Chrome", "abc123");
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
        let options = tui_agent_options("Headless Chromium", "abc123");
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
        let options = tui_agent_options("Browser Use cloud", "abc123");
        assert_eq!(options.browser_mode.as_deref(), Some("cloud"));
        assert!(options.python_env.is_empty());
    }

    #[test]
    fn local_chrome_sanitizes_session_id_for_daemon_name() {
        let options = tui_agent_options("Local Chrome", "abc/123 !?");
        assert_eq!(env_value(&options, "BU_NAME"), Some("but-tui-abc-123"));
        assert_eq!(
            env_value(&options, "BH_RUNTIME_DIR"),
            Some("/tmp/but-tui-abc-123")
        );
    }
}
