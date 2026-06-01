use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{bail, Context, Result};
use browser_use_agent::config_overrides::{
    load_mcp_servers_for_profile, resolve_approval_policy_for_profile,
    resolve_guardian_for_profile, AgentRunOptions, ChildAgentRunCompletion, ChildAgentRunRequest,
    ChildAgentRunner, ConfigOverrides, ProviderRunConfig,
};
use browser_use_agent::context::typed_user_input_payload_from_text_for_cwd;
use browser_use_agent::entrypoint::run_session_with_config;
use browser_use_agent::prompts::CollaborationModeKind;
use browser_use_protocol::{
    failure_from_events, sanitized_agent_context_from_events, session_result_from_events,
    SessionMeta,
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
    let mut config = ProviderRunConfig::new(backend.into(), model.clone())
        .with_options(tui_agent_options(
            &browser,
            &session_id,
            collaboration_mode,
            model_provider_id.as_deref(),
            browser_use_cloud_api_key.as_deref(),
            config_profile.clone(),
            config_overrides.clone(),
        )?)
        .with_fake_result("Fake result from the Rust TUI agent loop.");
    attach_tui_child_agent_runner(
        state_dir.clone(),
        backend,
        model,
        model_provider_id,
        browser,
        collaboration_mode,
        config_profile,
        config_overrides,
        &mut config,
    );
    // The async engine takes a `SharedStore` (`Arc<Mutex<Store>>`) and is driven
    // on a Tokio runtime. The live model transport opens streams through
    // `block_in_place`, so this must be a multi-thread runtime even though the
    // TUI still keeps the existing one OS thread per session entrypoint.
    let shared_store = Arc::new(Mutex::new(store));
    let runtime = build_agent_runtime()?;
    let result = runtime.block_on(run_session_with_config(
        Arc::clone(&shared_store),
        &session_id,
        config,
    ));
    if let Err(error) = result {
        if let Ok(store) = shared_store.lock() {
            let _ = store.append_event(
                &session_id,
                "session.failed",
                serde_json::json!({ "error": error.to_string() }),
            );
        }
        return Err(error);
    }
    Ok(())
}

fn build_agent_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("browser-use-agent-runtime")
        .worker_threads(2)
        .build()
        .context("build TUI agent tokio runtime")
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

fn attach_tui_child_agent_runner(
    state_dir: PathBuf,
    backend: AgentBackend,
    model: String,
    model_provider_id: Option<String>,
    browser: String,
    collaboration_mode: CollaborationModeKind,
    config_profile: Option<String>,
    config_overrides: ConfigOverrides,
    config: &mut ProviderRunConfig,
) {
    let runner = ChildAgentRunner::new(move |request| {
        spawn_tui_child_agent(
            state_dir.clone(),
            backend,
            model.clone(),
            model_provider_id.clone(),
            browser.clone(),
            collaboration_mode,
            config_profile.clone(),
            config_overrides.clone(),
            request,
        )
    });
    config.options = config.options.clone().with_child_agent_runner(runner);
}

fn spawn_tui_child_agent(
    state_dir: PathBuf,
    backend: AgentBackend,
    model: String,
    model_provider_id: Option<String>,
    browser: String,
    collaboration_mode: CollaborationModeKind,
    config_profile: Option<String>,
    mut config_overrides: ConfigOverrides,
    request: ChildAgentRunRequest,
) -> Result<()> {
    let store = Store::open(&state_dir)?;
    let child = create_tui_child_session_from_request(&store, &request)?;
    let child_id = child.id.clone();
    let child_model = request
        .model
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or(model);
    config_overrides.extend(request.config_overrides.clone());
    if let Some(reasoning) = request.reasoning_effort.clone() {
        config_overrides.push((
            "reasoning_effort".to_string(),
            toml::Value::String(reasoning),
        ));
    }
    if let Some(service_tier) = request.service_tier.clone() {
        config_overrides.push((
            "service_tier".to_string(),
            toml::Value::String(service_tier),
        ));
    }
    thread::Builder::new()
        .name(format!("browser-use-tui-child-{child_id}"))
        .spawn(move || {
            let run_state_dir = state_dir.clone();
            let result = run_agent_thread(
                run_state_dir,
                child_id.clone(),
                backend,
                child_model,
                model_provider_id,
                browser,
                collaboration_mode,
                config_profile,
                config_overrides,
                None,
            );
            notify_tui_child_completion(&state_dir, &child_id, &request, result.as_ref().err());
            if let Err(error) = result {
                eprintln!("tui child agent failed: {error:#}");
            }
        })
        .context("spawn TUI child agent thread")?;
    Ok(())
}

fn notify_tui_child_completion(
    state_dir: &Path,
    child_id: &str,
    request: &ChildAgentRunRequest,
    run_error: Option<&anyhow::Error>,
) {
    let Some(handler) = request.completion_handler.clone() else {
        return;
    };
    let completion = match run_error {
        Some(error) => ChildAgentRunCompletion::failure(format!("{error:#}")),
        None => {
            let summary = Store::open(state_dir)
                .and_then(|store| store.events_for_session(child_id))
                .ok()
                .and_then(|events| {
                    session_result_from_events(&events).or_else(|| failure_from_events(&events))
                });
            ChildAgentRunCompletion::success(summary)
        }
    };
    if let Err(error) = handler.notify(completion) {
        eprintln!("tui child agent completion notification failed: {error:#}");
    }
}

fn create_tui_child_session_from_request(
    store: &Store,
    request: &ChildAgentRunRequest,
) -> Result<SessionMeta> {
    if let Some(existing) = store.load_session(&request.child_session_id)? {
        return Ok(existing);
    }
    let parent = store
        .load_session(&request.parent_session_id)?
        .with_context(|| format!("unknown parent session id: {}", request.parent_session_id))?;
    let child = store.create_child_session_with_id(
        &request.parent_session_id,
        Path::new(&parent.cwd),
        request.agent_path.as_deref(),
        request.nickname.as_deref(),
        request.role.as_deref(),
        request.child_session_id.clone(),
    )?;
    let parent_events = store.events_for_session(&request.parent_session_id)?;
    let inherited_context = sanitized_agent_context_from_events(&parent_events);
    store.append_event(
        &child.id,
        "agent.context",
        serde_json::json!({
            "from_session_id": request.parent_session_id.clone(),
            "fork_mode": request.fork_turns.as_deref().unwrap_or("all"),
            "history_mode": "compact_context",
            "agent_path": request.agent_path.clone(),
            "nickname": request.nickname.clone(),
            "role": request.role.clone(),
            "context": inherited_context,
        }),
    )?;
    store.append_event(
        &child.id,
        "workspace.context",
        serde_json::json!({
            "kind": "environment_context",
            "content": format!(
                "<environment_context>\n<cwd>{}</cwd>\n</environment_context>",
                child.cwd
            ),
        }),
    )?;
    store.append_event(
        &child.id,
        "session.input",
        typed_user_input_payload_from_text_for_cwd(&request.message, &child.cwd)?,
    )?;
    store.append_event(
        &request.parent_session_id,
        "agent.spawned",
        serde_json::json!({
            "child_session_id": child.id.clone(),
            "agent_path": request.agent_path.clone(),
            "nickname": request.nickname.clone(),
            "role": request.role.clone(),
        }),
    )?;
    Ok(child)
}

fn tui_agent_options(
    browser: &str,
    _session_id: &str,
    collaboration_mode: CollaborationModeKind,
    model_provider_id: Option<&str>,
    browser_use_cloud_api_key: Option<&str>,
    config_profile: Option<String>,
    config_overrides: ConfigOverrides,
) -> Result<AgentRunOptions> {
    let profile_ref = config_profile.as_deref();
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
    if let Some(policy) = resolve_approval_policy_for_profile(profile_ref, &config_overrides, None)?
    {
        options = options.with_approval_policy(policy);
    }
    if let Some(use_guardian) = resolve_guardian_for_profile(profile_ref, &config_overrides, None)?
    {
        options = options.with_guardian(use_guardian);
    }
    let mcp_servers = load_mcp_servers_for_profile(profile_ref, &[])?;
    if !mcp_servers.is_empty() {
        options = options.with_mcp_servers(mcp_servers);
    }
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
        Ok(options.with_model_provider_id(model_provider_id.to_string()))
    } else {
        Ok(options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_use_agent::tools::AskForApproval;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn env_value<'a>(options: &'a AgentRunOptions, key: &str) -> Option<&'a str> {
        options
            .python_env
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn tui_agent_runtime_supports_block_in_place() {
        let runtime = build_agent_runtime().unwrap();
        let result = runtime.block_on(async { tokio::task::block_in_place(|| 42) });
        assert_eq!(result, 42);
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
        )
        .unwrap();
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
        )
        .unwrap();
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
        )
        .unwrap();
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
        )
        .unwrap();
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
        )
        .unwrap();
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
        )
        .unwrap();
        assert_eq!(options.browser_mode.as_deref(), Some("local"));
        assert_eq!(options.model_provider_id, None);
    }

    #[test]
    fn tui_agent_options_pass_profile_and_config_overrides_to_core() {
        let config_overrides = browser_use_agent::config_overrides::parse_config_overrides(&[
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
        )
        .unwrap();

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

    #[test]
    fn tui_agent_options_apply_runtime_config_layer() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock poisoned");
        let temp = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("BROWSER_USE_TERMINAL_HOME");
        unsafe {
            std::env::set_var("BROWSER_USE_TERMINAL_HOME", temp.path());
        }
        std::fs::write(
            temp.path().join("config.toml"),
            r#"
approval_policy = "unless-trusted"
use_guardian = true

[mcp_servers.local]
transport = "stdio"
command = "test-mcp"
"#,
        )
        .unwrap();

        let options = tui_agent_options(
            "Local Chrome",
            "abc123",
            CollaborationModeKind::Default,
            Some("codex"),
            None,
            None,
            Vec::new(),
        )
        .unwrap();

        assert_eq!(options.approval_policy, AskForApproval::UnlessTrusted);
        assert!(options.use_guardian);
        assert!(options.mcp_servers.contains_key("local"));

        unsafe {
            match previous {
                Some(value) => std::env::set_var("BROWSER_USE_TERMINAL_HOME", value),
                None => std::env::remove_var("BROWSER_USE_TERMINAL_HOME"),
            }
        }
    }

    #[test]
    fn tui_child_runner_request_creates_store_child_session() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let cwd = temp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = Store::open(&state_dir).unwrap();
        let parent = store.create_session(None, &cwd).unwrap();
        let request = ChildAgentRunRequest {
            parent_session_id: parent.id.clone(),
            child_session_id: "00000000dcba".to_string(),
            message: "Handle the child task".to_string(),
            agent_path: Some("/root/worker_1".to_string()),
            nickname: Some("Worker".to_string()),
            role: Some("explorer".to_string()),
            fork_turns: Some("all".to_string()),
            model: None,
            reasoning_effort: None,
            service_tier: None,
            config_overrides: Vec::new(),
            completion_handler: None,
        };

        let child = create_tui_child_session_from_request(&store, &request).unwrap();

        assert_eq!(child.id, "00000000dcba");
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
        let child_events = store.events_for_session(&child.id).unwrap();
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "workspace.context"));
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "session.input"));
        let parent_events = store.events_for_session(&parent.id).unwrap();
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "agent.spawned"
                && event.payload["child_session_id"] == child.id));
    }
}
