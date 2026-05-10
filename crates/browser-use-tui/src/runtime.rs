use std::path::PathBuf;

use anyhow::Result;
use browser_use_core::{run_existing_session_from_config, AgentRunOptions, ProviderRunConfig};
use browser_use_store::Store;

use crate::settings::AgentBackend;

pub(crate) fn run_agent_thread(
    state_dir: PathBuf,
    session_id: String,
    backend: AgentBackend,
    model: String,
    browser: String,
) -> Result<()> {
    let store = Store::open(&state_dir)?;
    let config = ProviderRunConfig::new(backend.into(), model)
        .with_options(tui_agent_options(&browser))
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

fn tui_agent_options(browser: &str) -> AgentRunOptions {
    match browser {
        "Headless Chromium" => AgentRunOptions::default().with_browser_mode("headless"),
        "Browser Use cloud" => AgentRunOptions::default().with_browser_mode("cloud"),
        _ => AgentRunOptions::default(),
    }
}
