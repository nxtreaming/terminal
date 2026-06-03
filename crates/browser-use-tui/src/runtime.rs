use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use anyhow::{bail, Context, Result};
use browser_use_agent::config_overrides::{
    load_mcp_servers_for_profile, resolve_agent_roles_for_profile,
    resolve_approval_policy_for_profile, resolve_collab_for_profile, resolve_guardian_for_profile,
    resolve_multi_agent_v2_for_profile, AgentRunOptions, ChildAgentRunCompletion,
    ChildAgentRunRequest, ChildAgentRunner, ConfigOverrides, ProviderRunConfig,
};
use browser_use_agent::context::{
    typed_user_input_payload_from_items_for_cwd, typed_user_input_payload_from_text_for_cwd,
};
use browser_use_agent::entrypoint::run_session_with_config_with_cancel;
use browser_use_agent::prompts::CollaborationModeKind;
use browser_use_agent::rollout::fork_events_by_turn;
use browser_use_agent::session::{
    provider_messages_from_events_for_fork, resume::provider_messages_to_fork_response_items,
    ForkMode,
};
use browser_use_agent::subagents::{display_agent_path_for_session, session_was_interrupted};
use browser_use_protocol::{failure_from_events, session_result_from_events, SessionMeta};
use browser_use_store::{Store, StoreNotifier};
use tokio_util::sync::CancellationToken;

use crate::settings::{
    browser_use_cloud_env_key_present, AgentBackend, BROWSER_USE_CLOUD,
    BROWSER_USE_CLOUD_API_KEY_ENV, BROWSER_USE_CLOUD_API_KEY_SETTING,
};

static ACTIVE_AGENT_RUNS: OnceLock<Mutex<HashMap<String, CancellationToken>>> = OnceLock::new();

fn active_agent_runs() -> &'static Mutex<HashMap<String, CancellationToken>> {
    ACTIVE_AGENT_RUNS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn cancel_agent_run(session_id: &str) -> bool {
    let Some(token) = active_agent_runs()
        .lock()
        .ok()
        .and_then(|runs| runs.get(session_id).cloned())
    else {
        return false;
    };
    token.cancel();
    true
}

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
        let error = "Browser Use Cloud selected, but BROWSER_USE_API_KEY is not set";
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
    let cancel = CancellationToken::new();
    if let Ok(mut runs) = active_agent_runs().lock() {
        runs.insert(session_id.clone(), cancel.clone());
    }
    let result = runtime.block_on(run_session_with_config_with_cancel(
        Arc::clone(&shared_store),
        &session_id,
        config,
        cancel,
    ));
    if let Ok(mut runs) = active_agent_runs().lock() {
        runs.remove(&session_id);
    }
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
    record_child_run_marker_from_request(&store, &child_id, &request)?;
    let child_model = request
        .model
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or(model);
    let child_browser = child_request_browser(&request).unwrap_or(browser);
    let child_model_provider_id = child_request_provider_id(&request).or(model_provider_id);
    let child_backend = child_model_provider_id
        .as_deref()
        .and_then(AgentBackend::from_setting)
        .unwrap_or(backend);
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
                child_backend,
                child_model,
                child_model_provider_id,
                child_browser,
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
            let events = Store::open(state_dir)
                .and_then(|store| store.events_for_session(child_id))
                .ok();
            if events
                .as_deref()
                .is_some_and(child_run_was_interrupted_from_events)
            {
                return;
            }
            let summary = events.and_then(|events| {
                session_result_from_events(&events).or_else(|| failure_from_events(&events))
            });
            ChildAgentRunCompletion::success(summary)
        }
    };
    if let Err(error) = handler.notify(completion) {
        eprintln!("tui child agent completion notification failed: {error:#}");
    }
}

fn child_run_was_interrupted_from_events(events: &[browser_use_protocol::EventRecord]) -> bool {
    session_was_interrupted(events)
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
    store.append_event(
        &child.id,
        "agent.context",
        child_request_agent_context_payload(&parent_events, request)?,
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
    seed_child_permissions_context_event(store, &child.id, request)?;
    append_child_initial_input_from_request(store, &child.id, &child.cwd, request)?;
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

fn record_child_run_marker_from_request(
    store: &Store,
    child_id: &str,
    request: &ChildAgentRunRequest,
) -> Result<()> {
    let Some(run_id) = request.run_id.as_deref() else {
        return Ok(());
    };
    let config_overrides = request
        .config_overrides
        .iter()
        .map(|(key, value)| {
            serde_json::json!({
                "key": key,
                "value": value,
            })
        })
        .collect::<Vec<_>>();
    store.append_event(
        child_id,
        "agent.run.started",
        serde_json::json!({
            "run_id": run_id,
            "parent_session_id": request.parent_session_id.as_str(),
            "child_session_id": child_id,
            "agent_path": request.agent_path.as_deref(),
            "model": request.model.as_deref(),
            "reasoning_effort": request.reasoning_effort.as_deref(),
            "service_tier": request.service_tier.as_deref(),
            "config_overrides": config_overrides,
        }),
    )?;
    Ok(())
}

fn append_child_initial_input_from_request(
    store: &Store,
    child_id: &str,
    child_cwd: &str,
    request: &ChildAgentRunRequest,
) -> Result<()> {
    if request.input_is_inter_agent_communication {
        let author_path = display_agent_path_for_session(store, &request.parent_session_id)
            .unwrap_or_else(|_| "/root".to_string());
        let recipient_path = request.agent_path.clone().unwrap_or_else(|| {
            display_agent_path_for_session(store, child_id).unwrap_or_else(|_| child_id.to_string())
        });
        store.append_event(
            child_id,
            "agent.mailbox_input",
            serde_json::json!({
                "id": browser_use_store::new_thread_id(),
                "author_session_id": request.parent_session_id,
                "target_session_id": child_id,
                "author_path": author_path,
                "recipient_path": recipient_path,
                "content": request.message,
                "trigger_turn": true,
            }),
        )?;
    } else {
        let payload = if let Some(items) = request.input_items.as_ref() {
            typed_user_input_payload_from_items_for_cwd(items, child_cwd)?
        } else {
            typed_user_input_payload_from_text_for_cwd(&request.message, child_cwd)?
        };
        store.append_event(child_id, "session.input", payload)?;
    }
    Ok(())
}

fn seed_child_permissions_context_event(
    store: &Store,
    session_id: &str,
    request: &ChildAgentRunRequest,
) -> Result<()> {
    let Some(content) = child_request_developer_instructions(request) else {
        return Ok(());
    };
    store.append_event(
        session_id,
        "workspace.context",
        serde_json::json!({
            "kind": "permissions",
            "content": content,
        }),
    )?;
    Ok(())
}

fn child_request_developer_instructions(request: &ChildAgentRunRequest) -> Option<String> {
    request
        .config_overrides
        .iter()
        .rev()
        .find(|(key, _)| key == "developer_instructions")
        .and_then(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn child_request_provider_id(request: &ChildAgentRunRequest) -> Option<String> {
    request
        .config_overrides
        .iter()
        .rev()
        .find(|(key, _)| {
            matches!(
                key.as_str(),
                "model_provider" | "model_provider_id" | "provider"
            )
        })
        .and_then(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn child_request_browser(request: &ChildAgentRunRequest) -> Option<String> {
    let browser_mode = request
        .config_overrides
        .iter()
        .rev()
        .find(|(key, _)| key == "browser_mode")
        .and_then(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    match browser_mode {
        "managed-headless" => Some("Headless Chromium".to_string()),
        "cloud" => Some(BROWSER_USE_CLOUD.to_string()),
        "local" => Some("Local Chrome".to_string()),
        _ => None,
    }
}

fn child_request_agent_context_payload(
    parent_events: &[browser_use_protocol::EventRecord],
    request: &ChildAgentRunRequest,
) -> Result<serde_json::Value> {
    let mode = child_request_fork_mode(request.fork_turns.as_deref())?;
    let forked = fork_events_by_turn(parent_events, &mode);
    let history = provider_messages_from_events_for_fork(&forked.carried);
    let response_items = provider_messages_to_fork_response_items(&history);
    let raw_mode = request
        .fork_turns
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("all");
    let mut payload = serde_json::json!({
        "from_session_id": request.parent_session_id.clone(),
        "fork_mode": raw_mode,
        "agent_path": request.agent_path.clone(),
        "nickname": request.nickname.clone(),
        "role": request.role.clone(),
    });
    if matches!(mode, ForkMode::None) {
        payload["history_mode"] = serde_json::json!("none");
    } else {
        payload["history_mode"] = serde_json::json!("fork_response_items");
        payload["fork_response_items"] = serde_json::Value::Array(response_items);
    }
    Ok(payload)
}

fn child_request_fork_mode(raw: Option<&str>) -> Result<ForkMode> {
    let value = raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("all");
    if value.eq_ignore_ascii_case("none") {
        return Ok(ForkMode::None);
    }
    if value.eq_ignore_ascii_case("all") {
        return Ok(ForkMode::All);
    }
    let turns = value
        .parse::<usize>()
        .with_context(|| "fork_turns must be `none`, `all`, or a positive integer string")?;
    if turns == 0 {
        bail!("fork_turns must be `none`, `all`, or a positive integer string");
    }
    Ok(ForkMode::LastN(turns))
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
    options = options.with_multi_agent_v2(resolve_multi_agent_v2_for_profile(
        profile_ref,
        &config_overrides,
    )?);
    options =
        options.with_collab_enabled(resolve_collab_for_profile(profile_ref, &config_overrides)?);
    options = options.with_agent_roles(resolve_agent_roles_for_profile(
        profile_ref,
        &config_overrides,
    )?);
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
    fn tui_agent_options_normalize_deprecated_plan_mode() {
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
        assert_eq!(options.collaboration_mode, CollaborationModeKind::Default);
        assert_eq!(options.model_provider_id.as_deref(), Some("codex"));
    }

    #[test]
    fn browser_use_cloud_keeps_cloud_mode() {
        let options = tui_agent_options(
            BROWSER_USE_CLOUD,
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
            BROWSER_USE_CLOUD,
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
            run_id: Some("run-1".to_string()),
            message: "Handle the child task".to_string(),
            input_items: None,
            input_is_inter_agent_communication: false,
            agent_path: Some("/root/worker_1".to_string()),
            nickname: Some("Worker".to_string()),
            role: Some("explorer".to_string()),
            fork_turns: Some("all".to_string()),
            model: Some("gpt-test".to_string()),
            reasoning_effort: Some("high".to_string()),
            service_tier: Some("priority".to_string()),
            config_overrides: vec![(
                "model_provider".to_string(),
                toml::Value::String("anthropic".to_string()),
            )],
            completion_handler: None,
        };

        let child = create_tui_child_session_from_request(&store, &request).unwrap();
        record_child_run_marker_from_request(&store, &child.id, &request).unwrap();

        assert_eq!(child.id, "00000000dcba");
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
        let child_events = store.events_for_session(&child.id).unwrap();
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "workspace.context"));
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "session.input"));
        let marker = child_events
            .iter()
            .find(|event| event.event_type == "agent.run.started")
            .expect("run marker");
        assert_eq!(marker.payload["model"], "gpt-test");
        assert_eq!(marker.payload["reasoning_effort"], "high");
        assert_eq!(marker.payload["service_tier"], "priority");
        let model_provider_override = marker.payload["config_overrides"]
            .as_array()
            .and_then(|items| {
                items.iter().find(|item| {
                    item.get("key").and_then(serde_json::Value::as_str) == Some("model_provider")
                })
            })
            .expect("model_provider override");
        assert_eq!(model_provider_override["value"], "anthropic");
        let parent_events = store.events_for_session(&parent.id).unwrap();
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "agent.spawned"
                && event.payload["child_session_id"] == child.id));
    }
}
