use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

mod telemetry;
mod tools;

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use browser_use_protocol::{
    failure_from_events, result_from_events, sanitized_agent_context_from_events, ModelEvent,
    SessionMeta, SessionStatus, ToolCall, ToolSpec,
};
use browser_use_providers::{
    load_codex_auth, refresh_claude_code_oauth, AnthropicMessagesProvider,
    ClaudeCodeOAuthCredential, CodexAuth, CodexResponsesProvider, FakeProvider, ModelProvider,
    OpenAICompatibleChatProvider, OpenAIResponsesProvider, ProviderTurn, ScriptedProvider,
};
use browser_use_python_worker::{PythonWorker, PythonWorkerEvent, RunPythonResponse};
use browser_use_store::{now_ms, AgentSummary, Store};
use opentelemetry::KeyValue;
use serde_json::{json, Map, Value};
use telemetry::{AgentTelemetry, ModelTurnSpanInput};
use tools::{ToolHandlerKind, ToolRegistry};

const APPROX_CHARS_PER_TOKEN: usize = 4;
const MAX_TOOL_OUTPUT_TEXT_TOKENS: usize = 4_000;
const IMAGE_CONTEXT_BUDGET_TOKENS: usize = 2_000;
const BROWSER_PREF_MODE: &str = "browser.preference.mode";
const BROWSER_PREF_PROFILE: &str = "browser.preference.profile";
const BROWSER_DOMAIN_PROFILE_PREFIX: &str = "browser.domain_profile.";
static TOOL_OUTPUT_ARTIFACT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct DoneResultFile {
    requested_path: String,
    path: PathBuf,
    bytes: u64,
    mime: &'static str,
}

pub struct FakeAgentOptions<'a> {
    pub python_code: Option<&'a str>,
}

pub fn install_process_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderBackend {
    Codex,
    Openai,
    Anthropic,
    Openrouter,
    Fake,
    None,
}

#[derive(Clone, Debug)]
pub struct ProviderRunConfig {
    pub backend: ProviderBackend,
    pub model: String,
    pub options: AgentRunOptions,
    pub fake_result: Option<String>,
}

impl ProviderRunConfig {
    pub fn new(backend: ProviderBackend, model: impl Into<String>) -> Self {
        Self {
            backend,
            model: model.into(),
            options: AgentRunOptions::default(),
            fake_result: None,
        }
    }

    pub fn with_options(mut self, options: AgentRunOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_fake_result(mut self, result: impl Into<String>) -> Self {
        self.fake_result = Some(result.into());
        self
    }
}

#[derive(Clone, Debug)]
pub struct ChildAgentRunRequest {
    pub parent_session_id: String,
    pub child_session_id: String,
}

#[derive(Clone)]
pub struct ChildAgentRunner {
    run: Arc<dyn Fn(ChildAgentRunRequest) -> Result<()> + Send + Sync>,
}

impl std::fmt::Debug for ChildAgentRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildAgentRunner").finish_non_exhaustive()
    }
}

impl ChildAgentRunner {
    pub fn new(run: impl Fn(ChildAgentRunRequest) -> Result<()> + Send + Sync + 'static) -> Self {
        Self { run: Arc::new(run) }
    }

    fn run(&self, request: ChildAgentRunRequest) -> Result<()> {
        (self.run)(request)
    }
}

#[derive(Clone, Debug)]
pub struct AgentRunOptions {
    pub max_turns: usize,
    pub max_context_chars: usize,
    pub browser_mode: Option<String>,
    pub python_tool_timeout_seconds: u64,
    pub python_env: Vec<(String, String)>,
    pub child_agent_runner: Option<ChildAgentRunner>,
}

impl Default for AgentRunOptions {
    fn default() -> Self {
        Self {
            max_turns: 80,
            max_context_chars: 240_000,
            browser_mode: None,
            python_tool_timeout_seconds: 120,
            python_env: Vec::new(),
            child_agent_runner: None,
        }
    }
}

impl AgentRunOptions {
    pub fn with_browser_mode(mut self, mode: impl Into<String>) -> Self {
        self.browser_mode = Some(mode.into());
        self
    }

    pub fn with_python_tool_timeout_seconds(mut self, timeout_seconds: u64) -> Self {
        self.python_tool_timeout_seconds = timeout_seconds;
        self
    }

    pub fn with_python_env(mut self, env: Vec<(String, String)>) -> Self {
        self.python_env = env;
        self
    }

    pub fn with_child_agent_runner(mut self, runner: ChildAgentRunner) -> Self {
        self.child_agent_runner = Some(runner);
        self
    }
}

fn browser_mode_instruction(mode: &str) -> String {
    let normalized = mode.to_ascii_lowercase().replace(['_', ' '], "-");
    match normalized.as_str() {
        "local" | "local-chrome" => concat!(
            "Selected browser mode: Local Chrome. Use `browser connect local` before page work. ",
            "This attaches to the user's already-open browser after remote debugging is enabled. ",
            "If connection is blocked, run `browser local setup` and wait for the user to approve remote debugging."
        )
        .to_string(),
        "headless" | "headless-chromium" | "managed-headless" => concat!(
            "Selected browser mode: Headless Chromium. Use `browser connect managed --headless` before page work. ",
            "This starts a Rust-owned managed browser with an isolated automation profile."
        )
        .to_string(),
        "managed" | "managed-headed" => concat!(
            "Selected browser mode: managed headed browser. Use `browser connect managed --headed` before page work. ",
            "This starts a Rust-owned visible browser with an isolated automation profile."
        )
        .to_string(),
        "cloud" | "browser-use-cloud" => concat!(
            "Selected browser mode: Browser Use cloud. Use `browser remote start` before page work. ",
            "Remote start means start and connect; use `browser remote live-url` to retrieve the watch URL."
        )
        .to_string(),
        other => format!(
            "Selected browser mode: {other}. Use `browser status --json` first, then choose an explicit browser connect command."
        ),
    }
}

pub fn run_fake_agent(
    store: &Store,
    task_text: &str,
    cwd: impl AsRef<Path>,
    options: FakeAgentOptions<'_>,
) -> Result<String> {
    let provider = if let Some(code) = options.python_code {
        ScriptedProvider::new(vec![
            vec![
                ModelEvent::TextDelta {
                    text: "Starting browser task.\n".to_string(),
                },
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "call_python".to_string(),
                        name: "python".to_string(),
                        arguments: serde_json::json!({ "code": code }),
                    },
                },
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "call_done".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({
                            "result": "Python tool completed."
                        }),
                    },
                },
                ModelEvent::Done,
            ],
        ])
    } else {
        ScriptedProvider::new(vec![vec![
            ModelEvent::TextDelta {
                text: format!("Fake result for: {task_text}"),
            },
            ModelEvent::Done,
        ]])
    };
    run_agent_with_provider(store, &provider, task_text, cwd, AgentRunOptions::default())
}

pub fn run_agent_with_provider<P: ModelProvider>(
    store: &Store,
    provider: &P,
    task_text: &str,
    cwd: impl AsRef<Path>,
    options: AgentRunOptions,
) -> Result<String> {
    let session = store.create_session(None, cwd.as_ref())?;
    store.append_event(
        &session.id,
        "session.input",
        serde_json::json!({ "text": task_text }),
    )?;
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": task_text,
    })];
    run_loaded_session_with_provider(store, provider, session, messages, options)
}

pub fn run_agent_from_config(
    store: &Store,
    task_text: &str,
    cwd: impl AsRef<Path>,
    config: ProviderRunConfig,
) -> Result<String> {
    let session = store.create_session(None, cwd.as_ref())?;
    store.append_event(
        &session.id,
        "session.input",
        serde_json::json!({ "text": task_text }),
    )?;
    run_existing_session_from_config(store, &session.id, config)
}

pub fn run_existing_session_with_provider<P: ModelProvider>(
    store: &Store,
    provider: &P,
    session_id: &str,
    options: AgentRunOptions,
) -> Result<String> {
    let session = store
        .load_session(session_id)?
        .with_context(|| format!("unknown session id: {session_id}"))?;
    let events = store.events_for_session(session_id)?;
    let messages = provider_messages_from_events(&events);
    run_loaded_session_with_provider(store, provider, session, messages, options)
}

pub fn run_existing_session_from_config(
    store: &Store,
    session_id: &str,
    mut config: ProviderRunConfig,
) -> Result<String> {
    install_config_child_agent_runner(store, &mut config);
    match config.backend {
        ProviderBackend::Codex => {
            let provider = codex_provider(store, config.model)?;
            run_existing_session_with_provider(store, &provider, session_id, config.options)
        }
        ProviderBackend::Openai => {
            let provider = openai_provider(store, config.model)?;
            run_existing_session_with_provider(store, &provider, session_id, config.options)
        }
        ProviderBackend::Anthropic => {
            let provider = anthropic_provider(store, config.model)?;
            run_existing_session_with_provider(store, &provider, session_id, config.options)
        }
        ProviderBackend::Openrouter => {
            let provider = openrouter_provider(store, config.model)?;
            run_existing_session_with_provider(store, &provider, session_id, config.options)
        }
        ProviderBackend::Fake => {
            let provider = FakeProvider::with_text(
                config
                    .fake_result
                    .as_deref()
                    .unwrap_or("Fake browser task completed."),
            );
            run_existing_session_with_provider(store, &provider, session_id, config.options)
        }
        ProviderBackend::None => Ok(session_id.to_string()),
    }
}

fn install_config_child_agent_runner(store: &Store, config: &mut ProviderRunConfig) {
    let mut base_options = config.options.clone();
    base_options.child_agent_runner = None;
    config.options.child_agent_runner = Some(config_child_agent_runner(
        store.state_dir().to_path_buf(),
        store.notifier(),
        config.backend,
        config.model.clone(),
        config.fake_result.clone(),
        base_options,
    ));
}

fn config_child_agent_runner(
    state_dir: std::path::PathBuf,
    notifier: Option<browser_use_store::StoreNotifier>,
    backend: ProviderBackend,
    model: String,
    fake_result: Option<String>,
    options: AgentRunOptions,
) -> ChildAgentRunner {
    ChildAgentRunner::new(move |request| {
        let store = Store::open_with_optional_notifier(&state_dir, notifier.clone())?;
        let mut options = options.clone();
        retarget_managed_child_python_env(&mut options, &request.child_session_id);
        let config = ProviderRunConfig {
            backend,
            model: model.clone(),
            options,
            fake_result: fake_result.clone(),
        };
        run_existing_session_from_config(&store, &request.child_session_id, config)?;
        Ok(())
    })
}

fn retarget_managed_child_python_env(options: &mut AgentRunOptions, child_session_id: &str) {
    let has_managed_browser_env = options
        .python_env
        .iter()
        .any(|(key, _)| key == "BU_NAME" || key == "BH_RUNTIME_DIR" || key == "BH_TMP_DIR");
    if !has_managed_browser_env {
        return;
    }
    let daemon_name = format!("but-tui-{}", safe_agent_env_segment(child_session_id));
    let runtime_dir = format!("/tmp/{daemon_name}");
    for (key, value) in &mut options.python_env {
        match key.as_str() {
            "BU_NAME" => *value = daemon_name.clone(),
            "BH_RUNTIME_DIR" | "BH_TMP_DIR" => *value = runtime_dir.clone(),
            _ => {}
        }
    }
}

fn safe_agent_env_segment(value: &str) -> String {
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

fn openai_provider(store: &Store, model: String) -> Result<OpenAIResponsesProvider> {
    let api_key = stored_or_env(
        store,
        "auth.openai.api_key",
        &["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"],
    )?
    .context("run `auth login openai --api-key ...` or set LLM_BROWSER_OPENAI_API_KEY")?;
    let base_url = setting_or_env_or_default(
        store,
        "auth.openai.base_url",
        &["LLM_BROWSER_OPENAI_BASE_URL"],
        "https://api.openai.com/v1",
    )?;
    Ok(OpenAIResponsesProvider::with_base_url(
        api_key, model, base_url,
    ))
}

fn codex_provider(store: &Store, model: String) -> Result<CodexResponsesProvider> {
    let auth = match stored_codex_auth(store)? {
        Some(auth) => auth,
        None => load_codex_auth()?,
    };
    let base_url = setting_or_env_or_default(
        store,
        "auth.codex.base_url",
        &["LLM_BROWSER_CODEX_BASE_URL"],
        "https://chatgpt.com/backend-api",
    )?;
    Ok(CodexResponsesProvider::with_base_url(auth, model, base_url))
}

fn anthropic_provider(store: &Store, model: String) -> Result<AnthropicMessagesProvider> {
    let base_url = setting_or_env_or_default(
        store,
        "auth.anthropic.base_url",
        &["LLM_BROWSER_ANTHROPIC_BASE_URL"],
        "https://api.anthropic.com/v1",
    )?;
    if store
        .get_setting("account")?
        .as_deref()
        .is_some_and(is_claude_code_account)
    {
        let auth_token = claude_code_access_token(store)?;
        return Ok(AnthropicMessagesProvider::with_auth_token(
            auth_token, model, base_url,
        ));
    }
    let api_key = stored_or_env(
        store,
        "auth.anthropic.api_key",
        &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
    )?
    .context("run `auth login anthropic --api-key ...` or set LLM_BROWSER_ANTHROPIC_API_KEY")?;
    Ok(AnthropicMessagesProvider::with_base_url(
        api_key, model, base_url,
    ))
}

fn claude_code_access_token(store: &Store) -> Result<String> {
    if let Some(refresh_token) = store.get_setting("auth.claude_code.refresh_token")? {
        let expires_ms = store
            .get_setting("auth.claude_code.expires_ms")?
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        if !refresh_token.trim().is_empty() && expires_ms <= now_ms() + 60_000 {
            let credential = refresh_claude_code_oauth(refresh_token.trim())
                .context("refresh Claude Code OAuth token")?;
            store_claude_code_oauth(store, &credential)?;
            return Ok(credential.access_token);
        }
    }
    if let Some(access_token) = stored_or_env(
        store,
        "auth.claude_code.access_token",
        &[
            "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "LLM_BROWSER_ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
        ],
    )? {
        return Ok(access_token);
    }
    stored_or_env(
        store,
        "auth.claude_code.auth_token",
        &[
            "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "LLM_BROWSER_ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
        ],
    )?
    .context(
        "run `auth login claude-code` to sign in with Claude Code, or set CLAUDE_CODE_OAUTH_TOKEN",
    )
}

fn store_claude_code_oauth(store: &Store, credential: &ClaudeCodeOAuthCredential) -> Result<()> {
    store.set_setting(
        "auth.claude_code.access_token",
        credential.access_token.trim(),
    )?;
    if credential.refresh_token.trim().is_empty() {
        store.delete_setting("auth.claude_code.refresh_token")?;
    } else {
        store.set_setting(
            "auth.claude_code.refresh_token",
            credential.refresh_token.trim(),
        )?;
    }
    if credential.expires_ms > 0 {
        store.set_setting(
            "auth.claude_code.expires_ms",
            &credential.expires_ms.to_string(),
        )?;
    }
    store.delete_setting("auth.claude_code.auth_token")?;
    Ok(())
}

fn is_claude_code_account(account: &str) -> bool {
    matches!(account, "Claude Code login" | "Claude Code subscription")
}

fn openrouter_provider(store: &Store, model: String) -> Result<OpenAICompatibleChatProvider> {
    let api_key = stored_or_env(
        store,
        "auth.openrouter.api_key",
        &["LLM_BROWSER_OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY"],
    )?
    .context("run `auth login openrouter --api-key ...` or set OPENROUTER_API_KEY")?;
    let base_url = setting_or_env_or_default(
        store,
        "auth.openrouter.base_url",
        &["LLM_BROWSER_OPENAI_COMPAT_BASE_URL", "OPENROUTER_BASE_URL"],
        "https://openrouter.ai/api/v1",
    )?;
    Ok(OpenAICompatibleChatProvider::with_base_url(
        api_key, model, base_url,
    ))
}

fn stored_codex_auth(store: &Store) -> Result<Option<CodexAuth>> {
    let Some(access_token) = store.get_setting("auth.codex.access_token")? else {
        return Ok(None);
    };
    let Some(account_id) = store.get_setting("auth.codex.account_id")? else {
        return Ok(None);
    };
    if access_token.trim().is_empty() || account_id.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(CodexAuth {
        access_token,
        account_id,
    }))
}

fn stored_or_env(store: &Store, setting_key: &str, env_names: &[&str]) -> Result<Option<String>> {
    if let Some(value) = store.get_setting(setting_key)? {
        if !value.trim().is_empty() {
            return Ok(Some(value));
        }
    }
    Ok(env_names
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty()))
}

fn setting_or_env_or_default(
    store: &Store,
    setting_key: &str,
    env_names: &[&str],
    default: &str,
) -> Result<String> {
    Ok(stored_or_env(store, setting_key, env_names)?.unwrap_or_else(|| default.to_string()))
}

fn run_loaded_session_with_provider<P: ModelProvider>(
    store: &Store,
    provider: &P,
    session: SessionMeta,
    mut messages: Vec<Value>,
    options: AgentRunOptions,
) -> Result<String> {
    let run_id = store.record_run_started(&session.id, Some(std::process::id() as i64))?;
    let telemetry = match AgentTelemetry::from_store(store) {
        Ok(telemetry) => telemetry,
        Err(error) => {
            store.append_event(
                &session.id,
                "telemetry.failed",
                serde_json::json!({
                    "backend": "laminar",
                    "error": format!("{error:#}"),
                }),
            )?;
            AgentTelemetry::disabled()
        }
    };
    let task_text = task_text_from_provider_messages(&messages);
    if let Some(browser_mode) = options.browser_mode.as_deref() {
        messages.insert(
            0,
            serde_json::json!({
                "role": "system",
                "content": browser_mode_instruction(browser_mode),
            }),
        );
    }
    let agent_span = telemetry.start_agent_span(
        &session.id,
        session.parent_id.as_deref(),
        &session.cwd,
        task_text.as_deref(),
    );
    if telemetry.is_enabled() {
        store.append_event(
            &session.id,
            "telemetry.trace",
            serde_json::json!({
                "backend": "laminar",
                "transport": "otlp_http_proto",
                "trace_id": agent_span.trace_id(),
                "endpoint": telemetry.endpoint(),
            }),
        )?;
    }
    let result = (|| -> Result<String> {
        let mut worker = PythonWorker::start_with_browser_mode_and_env(
            options.browser_mode.as_deref(),
            options
                .python_env
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )?;
        let run_result = (|| -> Result<String> {
            store.append_event(
                &session.id,
                "session.status",
                serde_json::json!({ "status": "running" }),
            )?;
            store.append_event(
                &session.id,
                "model.config",
                serde_json::json!({
                    "provider": provider.provider_name(),
                    "model": provider.model_name(),
                }),
            )?;

            let mut deadline_warning_emitted = false;
            let mut last_external_session_message_seq =
                latest_session_message_seq(store, &session.id)?;
            for turn_idx in 0..options.max_turns {
                ensure_not_cancelled(store, &session.id)?;
                append_new_external_session_messages(
                    store,
                    &session.id,
                    &mut messages,
                    &mut last_external_session_message_seq,
                )?;
                normalize_provider_messages(&mut messages);
                maybe_compact_messages(
                    store,
                    &session.id,
                    &mut messages,
                    options.max_context_chars,
                )?;
                normalize_provider_messages(&mut messages);
                if maybe_emit_deadline_warning(
                    store,
                    &session.id,
                    turn_idx,
                    options.max_turns,
                    &mut deadline_warning_emitted,
                )? {
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": "The turn budget is nearly exhausted. Stop starting new lines of investigation. Produce the best available final answer now, write or reference any artifacts you have, and explicitly mark unknown or ambiguous fields instead of timing out with no deliverable.",
                    }));
                }
                let mut assistant_text = String::new();
                let mut tool_calls = Vec::new();
                let step_span = telemetry.start_step_span(
                    &agent_span,
                    &session.id,
                    turn_idx,
                    options.max_turns,
                );

                let turn_messages = messages.clone();
                let turn_tools = browser_tool_specs();
                let model_span = telemetry.start_model_turn_span(ModelTurnSpanInput {
                    parent: &step_span,
                    session_id: &session.id,
                    turn_idx,
                    provider_name: provider.provider_name(),
                    model_name: provider.model_name(),
                    messages: &turn_messages,
                    tools: &turn_tools,
                });
                let provider_events = match start_provider_turn_with_retries(
                    store,
                    &session.id,
                    provider,
                    ProviderTurn {
                        messages: turn_messages,
                        tools: turn_tools.clone(),
                    },
                    turn_idx,
                ) {
                    Ok(events) => {
                        telemetry.record_model_events(
                            &model_span,
                            provider.provider_name(),
                            turn_idx,
                            &events,
                        );
                        events
                    }
                    Err(error) => {
                        if is_context_overflow_provider_error(&format!("{error:#}")) {
                            model_span.record_error(error.as_ref());
                            store.append_event(
                                &session.id,
                                "model.turn.context_overflow",
                                serde_json::json!({
                                    "turn_idx": turn_idx,
                                    "provider": provider.provider_name(),
                                    "model": provider.model_name(),
                                    "error": format!("{error:#}"),
                                    "action": "compact_and_retry_once",
                                }),
                            )?;
                            force_compact_messages(
                                store,
                                &session.id,
                                &mut messages,
                                options.max_context_chars,
                                "provider_context_overflow",
                            )?;
                            normalize_provider_messages(&mut messages);
                            let retry_messages = messages.clone();
                            match start_provider_turn_with_retries(
                                store,
                                &session.id,
                                provider,
                                ProviderTurn {
                                    messages: retry_messages,
                                    tools: turn_tools,
                                },
                                turn_idx,
                            ) {
                                Ok(events) => {
                                    telemetry.record_model_events(
                                        &model_span,
                                        provider.provider_name(),
                                        turn_idx,
                                        &events,
                                    );
                                    events
                                }
                                Err(retry_error) => {
                                    model_span.record_error(retry_error.as_ref());
                                    step_span.record_error(retry_error.as_ref());
                                    return Err(retry_error);
                                }
                            }
                        } else {
                            model_span.record_error(error.as_ref());
                            step_span.record_error(error.as_ref());
                            return Err(error);
                        }
                    }
                };
                drop(model_span);

                for event in provider_events {
                    match event {
                        ModelEvent::TextDelta { text } => {
                            if let Some(delta) = assistant_delta_to_append(&assistant_text, &text) {
                                store.append_event(
                                    &session.id,
                                    "model.delta",
                                    serde_json::json!({ "text": delta }),
                                )?;
                                assistant_text.push_str(&delta);
                            }
                        }
                        ModelEvent::ThinkingDelta { .. } => {}
                        ModelEvent::Usage { usage } => {
                            store.append_event(
                                &session.id,
                                "model.usage",
                                serde_json::to_value(usage)?,
                            )?;
                        }
                        ModelEvent::ToolCall { call } => {
                            store.append_event(
                                &session.id,
                                "model.tool_call",
                                serde_json::to_value(&call)?,
                            )?;
                            tool_calls.push(call);
                        }
                        ModelEvent::Done => {}
                    }
                }
                ensure_not_cancelled(store, &session.id)?;

                if !assistant_text.is_empty() || !tool_calls.is_empty() {
                    messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": assistant_text,
                        "tool_calls": tool_calls.iter().map(tool_call_message).collect::<Vec<_>>(),
                    }));
                }
                maybe_compact_messages(
                    store,
                    &session.id,
                    &mut messages,
                    options.max_context_chars,
                )?;

                if tool_calls.is_empty() {
                    if !assistant_text.trim().is_empty() {
                        let requested_result = assistant_text.trim_end();
                        if let Some(error) = guard_or_close_active_descendants(
                            store,
                            &session.id,
                            &serde_json::json!({}),
                        )? {
                            store.append_event(
                                &session.id,
                                "tool.failed",
                                serde_json::json!({
                                    "name": "done",
                                    "source": "assistant_text_active_subagents",
                                    "error": error,
                                }),
                            )?;
                            messages.push(serde_json::json!({
                                "role": "user",
                                "content": error,
                            }));
                            maybe_compact_messages(
                                store,
                                &session.id,
                                &mut messages,
                                options.max_context_chars,
                            )?;
                            step_span.set_ok();
                            continue;
                        }
                        store.append_event(
                            &session.id,
                            "session.done",
                            serde_json::json!({ "result": requested_result }),
                        )?;
                        step_span.set_ok();
                        return Ok(session.id.clone());
                    }
                    step_span.set_ok();
                    continue;
                }

                for outcome in dispatch_tool_calls_for_turn(
                    store,
                    provider,
                    &session,
                    &mut worker,
                    tool_calls,
                    &options,
                    &telemetry,
                    &step_span,
                    turn_idx,
                )? {
                    messages.extend(outcome.messages);
                    maybe_compact_messages(
                        store,
                        &session.id,
                        &mut messages,
                        options.max_context_chars,
                    )?;
                    if outcome.finished {
                        step_span.set_ok();
                        return Ok(session.id.clone());
                    }
                }
                step_span.set_ok();
            }

            store.append_event(
                &session.id,
                "session.failed",
                serde_json::json!({ "error": "agent exceeded maximum provider turns" }),
            )?;
            bail!("agent exceeded maximum provider turns");
        })();
        // Browser state is session-level, not run-level. Keep local, managed, and
        // remote browser ownership alive across normal task completion so follow-ups
        // can inspect/recover the same browser. Explicit close/stop actions own
        // teardown.
        let cleaned_commands = tools::command::cleanup_session_commands(&session.id);
        if cleaned_commands > 0 {
            store.append_event(
                &session.id,
                "command.cleaned_up",
                serde_json::json!({ "count": cleaned_commands }),
            )?;
        }
        run_result
    })();
    let cancelled = is_cancelled(store, &session.id)?;
    let final_events = store.events_for_session(&session.id).unwrap_or_default();
    if cancelled {
        telemetry.record_agent_output(&agent_span, "cancelled");
    } else if let Err(error) = &result {
        let output = failure_from_events(&final_events).unwrap_or_else(|| format!("{error:#}"));
        telemetry.record_agent_output(&agent_span, &output);
        agent_span.record_error(error.as_ref());
        if !cancelled && !has_terminal_session_event(store, &session.id)? {
            store.append_event(
                &session.id,
                "session.failed",
                serde_json::json!({ "error": format!("{error:#}") }),
            )?;
        }
    } else {
        if let Some(output) = result_from_events(&final_events) {
            telemetry.record_agent_output(&agent_span, &output);
        }
        agent_span.set_ok();
    }
    let run_status = if cancelled {
        "cancelled"
    } else if result.is_ok() {
        "done"
    } else {
        "failed"
    };
    store.finish_run(&run_id, run_status)?;
    drop(agent_span);
    telemetry.force_flush();
    result
}

fn start_provider_turn_with_retries<P: ModelProvider>(
    store: &Store,
    session_id: &str,
    provider: &P,
    turn: ProviderTurn,
    turn_idx: usize,
) -> Result<Vec<ModelEvent>> {
    record_model_turn_request(store, session_id, provider, turn_idx, &turn)?;
    let max_retries = provider_retry_budget(provider.provider_name());
    let mut attempt = 0_usize;
    loop {
        let mut events = Vec::new();
        let mut streamed_text = String::new();
        let mut streamed_thinking_text = String::new();
        match provider.stream_turn(turn.clone(), &mut |event| {
            match &event {
                ModelEvent::TextDelta { text } => {
                    if let Some(delta) = assistant_delta_to_append(&streamed_text, text) {
                        record_model_stream_delta(
                            store,
                            session_id,
                            provider,
                            turn_idx,
                            attempt + 1,
                            &delta,
                        )?;
                        streamed_text.push_str(&delta);
                    }
                }
                ModelEvent::ThinkingDelta { text, label } => {
                    if let Some(delta) = assistant_delta_to_append(&streamed_thinking_text, text) {
                        record_model_thinking_delta(
                            store,
                            session_id,
                            provider,
                            turn_idx,
                            attempt + 1,
                            &delta,
                            label.as_deref(),
                        )?;
                        streamed_thinking_text.push_str(&delta);
                    }
                }
                ModelEvent::ToolCall { .. } | ModelEvent::Usage { .. } | ModelEvent::Done => {}
            }
            events.push(event);
            Ok(())
        }) {
            Ok(()) => {
                record_model_turn_response(store, session_id, turn_idx, attempt + 1, &events)?;
                return Ok(events);
            }
            Err(error) => {
                let error_chain = format!("{error:#}");
                let transient = is_transient_provider_error(&error_chain);
                store.append_event(
                    session_id,
                    "model.turn.error",
                    serde_json::json!({
                        "turn_idx": turn_idx,
                        "attempt": attempt + 1,
                        "provider": provider.provider_name(),
                        "model": provider.model_name(),
                        "transient": transient,
                        "error": error_chain,
                    }),
                )?;
                if !transient || attempt >= max_retries {
                    return Err(error);
                }
                attempt += 1;
                let delay = provider_retry_delay(attempt);
                store.append_event(
                    session_id,
                    "model.turn.retry",
                    serde_json::json!({
                        "turn_idx": turn_idx,
                        "attempt": attempt,
                        "max_retries": max_retries,
                        "delay_ms": delay.as_millis() as u64,
                        "provider": provider.provider_name(),
                        "message": format!("Reconnecting... {attempt}/{max_retries}"),
                        "error": error_chain,
                    }),
                )?;
                thread::sleep(delay);
            }
        }
    }
}

fn record_model_stream_delta<P: ModelProvider>(
    store: &Store,
    session_id: &str,
    provider: &P,
    turn_idx: usize,
    attempt: usize,
    text: &str,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    store.append_event(
        session_id,
        "model.stream_delta",
        serde_json::json!({
            "turn_idx": turn_idx,
            "attempt": attempt,
            "provider": provider.provider_name(),
            "model": provider.model_name(),
            "text": text,
        }),
    )?;
    Ok(())
}

fn record_model_thinking_delta<P: ModelProvider>(
    store: &Store,
    session_id: &str,
    provider: &P,
    turn_idx: usize,
    attempt: usize,
    text: &str,
    label: Option<&str>,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let mut payload = serde_json::json!({
        "turn_idx": turn_idx,
        "attempt": attempt,
        "provider": provider.provider_name(),
        "model": provider.model_name(),
        "text": text,
    });
    if let Some(label) = label.filter(|label| !label.trim().is_empty()) {
        payload["label"] = serde_json::json!(label);
    }
    store.append_event(session_id, "model.thinking_delta", payload)?;
    Ok(())
}

fn provider_retry_budget(provider_name: &str) -> usize {
    if let Ok(raw) = std::env::var("LLM_BROWSER_PROVIDER_MAX_RETRIES") {
        if let Ok(value) = raw.parse::<usize>() {
            return value.min(10);
        }
    }
    match provider_name {
        "codex" => 5,
        "openai" | "openai-compatible" | "anthropic" => 3,
        _ => 0,
    }
}

fn provider_retry_delay(attempt: usize) -> Duration {
    let shift = attempt.saturating_sub(1).min(5) as u32;
    Duration::from_millis(200_u64.saturating_mul(1_u64 << shift))
}

fn is_transient_provider_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    let terminal = [
        "incorrect api key",
        "401 unauthorized",
        "403 forbidden",
        "400 bad request",
        "content was flagged",
        "cybersecurity risk",
        "context_length_exceeded",
        "invalid_request_error",
        "schema",
        "unsupported",
    ];
    if terminal.iter().any(|needle| error.contains(needle)) {
        return false;
    }
    [
        "read codex sse line",
        "stream disconnected",
        "stream error",
        "connection reset",
        "connection closed",
        "connection aborted",
        "operation timed out",
        "timed out",
        "timeout",
        "temporarily",
        "overloaded",
        "rate limit",
        "too many requests",
        "eof",
        "502",
        "503",
        "504",
        "gateway",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

fn is_context_overflow_provider_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "context_length_exceeded",
        "context length",
        "context window",
        "maximum context",
        "too many tokens",
        "token limit",
        "input is too long",
        "input too long",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

fn record_model_turn_request<P: ModelProvider>(
    store: &Store,
    session_id: &str,
    provider: &P,
    turn_idx: usize,
    turn: &ProviderTurn,
) -> Result<()> {
    let messages_value = Value::Array(turn.messages.clone());
    let tools_value = serde_json::to_value(&turn.tools)?;
    let mut payload = serde_json::json!({
        "turn_idx": turn_idx,
        "provider": provider.provider_name(),
        "model": provider.model_name(),
        "message_count": turn.messages.len(),
        "tool_count": turn.tools.len(),
        "estimated_context_chars": estimated_context_chars(&turn.messages)?,
        "estimated_context_tokens": estimated_context_tokens(&turn.messages)?,
        "input_image_count": count_input_images(&messages_value),
        "messages_fingerprint": value_fingerprint(&messages_value)?,
        "tools_fingerprint": value_fingerprint(&tools_value)?,
    });
    if record_full_model_io() {
        payload["messages"] = messages_value;
        payload["tools"] = tools_value;
    }
    store.append_event(session_id, "model.turn.request", payload)?;
    Ok(())
}

fn record_model_turn_response(
    store: &Store,
    session_id: &str,
    turn_idx: usize,
    attempts: usize,
    events: &[ModelEvent],
) -> Result<()> {
    let events_value = serde_json::to_value(events)?;
    let mut payload = serde_json::json!({
        "turn_idx": turn_idx,
        "attempts": attempts,
        "event_count": events.len(),
        "text_delta_chars": events.iter().map(|event| match event {
            ModelEvent::TextDelta { text } => text.chars().count(),
            _ => 0,
        }).sum::<usize>(),
        "thinking_delta_chars": events.iter().map(|event| match event {
            ModelEvent::ThinkingDelta { text, .. } => text.chars().count(),
            _ => 0,
        }).sum::<usize>(),
        "tool_call_count": events.iter().filter(|event| matches!(event, ModelEvent::ToolCall { .. })).count(),
        "usage_count": events.iter().filter(|event| matches!(event, ModelEvent::Usage { .. })).count(),
        "events_fingerprint": value_fingerprint(&events_value)?,
    });
    if record_full_model_io() {
        payload["events"] = events_value;
    }
    store.append_event(session_id, "model.turn.response", payload)?;
    Ok(())
}

fn record_full_model_io() -> bool {
    std::env::var("LLM_BROWSER_RECORD_MODEL_IO")
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn value_fingerprint(value: &Value) -> Result<String> {
    let serialized = serde_json::to_string(value)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serialized.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

fn count_input_images(value: &Value) -> usize {
    match value {
        Value::Array(items) => items.iter().map(count_input_images).sum(),
        Value::Object(map) => {
            let self_count = usize::from(
                map.get("type").and_then(Value::as_str) == Some("input_image")
                    || map
                        .get("image_url")
                        .and_then(Value::as_str)
                        .is_some_and(|url| url.starts_with("data:image/")),
            );
            self_count + map.values().map(count_input_images).sum::<usize>()
        }
        _ => 0,
    }
}

fn assistant_delta_to_append(current: &str, incoming: &str) -> Option<String> {
    if incoming.is_empty() {
        return None;
    }
    if current.is_empty() {
        return Some(incoming.to_string());
    }
    if incoming == current || incoming.trim() == current.trim() {
        return None;
    }
    if let Some(suffix) = incoming.strip_prefix(current) {
        return (!suffix.is_empty()).then(|| suffix.to_string());
    }
    if incoming.chars().count() >= 24 && current.ends_with(incoming) {
        return None;
    }
    Some(incoming.to_string())
}

fn ensure_not_cancelled(store: &Store, session_id: &str) -> Result<()> {
    if is_cancelled(store, session_id)? {
        bail!("agent cancelled");
    }
    Ok(())
}

fn has_terminal_session_event(store: &Store, session_id: &str) -> Result<bool> {
    Ok(store.events_for_session(session_id)?.iter().any(|event| {
        matches!(
            event.event_type.as_str(),
            "session.done" | "session.failed" | "session.cancelled"
        )
    }))
}

fn is_cancelled(store: &Store, session_id: &str) -> Result<bool> {
    Ok(store
        .load_session(session_id)?
        .is_some_and(|session| session.status == SessionStatus::Cancelled))
}

fn maybe_emit_deadline_warning(
    store: &Store,
    session_id: &str,
    turn_idx: usize,
    max_turns: usize,
    emitted: &mut bool,
) -> Result<bool> {
    if *emitted || max_turns < 2 || turn_idx + 1 < max_turns {
        return Ok(false);
    }
    *emitted = true;
    store.append_event(
        session_id,
        "session.deadline_warning",
        serde_json::json!({
            "reason": "agent turn budget is nearly exhausted",
            "max_turns": max_turns,
            "remaining_turns": max_turns.saturating_sub(turn_idx),
        }),
    )?;
    Ok(true)
}

fn maybe_compact_messages(
    store: &Store,
    session_id: &str,
    messages: &mut Vec<Value>,
    max_context_chars: usize,
) -> Result<()> {
    if max_context_chars == 0 {
        return Ok(());
    }
    let max_context_tokens = approx_token_count_from_chars(max_context_chars);
    let before_chars = estimated_context_chars(messages)?;
    let before_tokens = estimated_context_tokens(messages)?;
    if before_tokens <= max_context_tokens {
        return Ok(());
    }
    compact_messages(
        store,
        session_id,
        messages,
        max_context_chars,
        max_context_tokens,
        before_chars,
        before_tokens,
        "estimated_budget_exceeded",
    )
}

fn force_compact_messages(
    store: &Store,
    session_id: &str,
    messages: &mut Vec<Value>,
    max_context_chars: usize,
    reason: &str,
) -> Result<()> {
    let max_context_chars = if max_context_chars == 0 {
        80_000
    } else {
        max_context_chars
    };
    let max_context_tokens = approx_token_count_from_chars(max_context_chars);
    let before_chars = estimated_context_chars(messages)?;
    let before_tokens = estimated_context_tokens(messages)?;
    compact_messages(
        store,
        session_id,
        messages,
        max_context_chars,
        max_context_tokens,
        before_chars,
        before_tokens,
        reason,
    )
}

#[allow(clippy::too_many_arguments)]
fn compact_messages(
    store: &Store,
    session_id: &str,
    messages: &mut Vec<Value>,
    max_context_chars: usize,
    max_context_tokens: usize,
    before_chars: usize,
    before_tokens: usize,
    reason: &str,
) -> Result<()> {
    store.append_event(
        session_id,
        "session.compaction_started",
        serde_json::json!({
            "reason": reason,
            "message_count": messages.len(),
            "chars": before_chars,
            "tokens": before_tokens,
            "max_chars": max_context_chars,
            "max_tokens": max_context_tokens,
        }),
    )?;
    let result = (|| -> Result<()> {
        let events = store.events_for_session(session_id)?;
        let context = sanitized_agent_context_from_events(&events);
        let mut compacted = vec![serde_json::json!({
            "role": "system",
            "content": compacted_context_system_message(&context)?,
        })];
        let recent_messages = recent_messages_to_preserve(messages);
        for recent_message in recent_messages {
            let max_recent_content_tokens = (max_context_tokens / 4).max(50);
            compacted.push(compact_recent_message(
                recent_message,
                max_recent_content_tokens,
            ));
        }
        *messages = compacted;
        Ok(())
    })();
    if let Err(error) = result {
        store.append_event(
            session_id,
            "session.compaction_failed",
            serde_json::json!({ "error": format!("{error:#}") }),
        )?;
        return Err(error);
    }
    store.append_event(
        session_id,
        "session.compacted",
        serde_json::json!({
            "reason": reason,
            "message_count": messages.len(),
            "chars": estimated_context_chars(messages)?,
            "tokens": estimated_context_tokens(messages)?,
        }),
    )?;
    Ok(())
}

fn estimated_context_chars(messages: &[Value]) -> Result<usize> {
    Ok(serde_json::to_string(&context_budget_value(&Value::Array(messages.to_vec())))?.len())
}

fn estimated_context_tokens(messages: &[Value]) -> Result<usize> {
    let text = serde_json::to_string(&context_budget_value(&Value::Array(messages.to_vec())))?;
    Ok(approx_token_count(&text))
}

fn approx_token_count_from_chars(chars: usize) -> usize {
    chars.div_ceil(APPROX_CHARS_PER_TOKEN).max(1)
}

fn approx_token_count(text: &str) -> usize {
    let char_tokens = approx_token_count_from_chars(text.chars().count());
    let wordish_tokens = text
        .split(|ch: char| ch.is_whitespace() || ch.is_ascii_punctuation())
        .filter(|part| !part.is_empty())
        .count();
    char_tokens
        .max(wordish_tokens)
        .max(usize::from(!text.is_empty()))
}

fn token_budget_to_char_budget(tokens: usize) -> usize {
    tokens.saturating_mul(APPROX_CHARS_PER_TOKEN)
}

fn context_budget_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(context_budget_value).collect()),
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (key, value) in map {
                if key == "image_url"
                    && value
                        .as_str()
                        .is_some_and(|url| url.starts_with("data:image/"))
                {
                    out.insert(
                        key.clone(),
                        Value::String(format!(
                            "[image data omitted from text budget]{}",
                            ".".repeat(token_budget_to_char_budget(IMAGE_CONTEXT_BUDGET_TOKENS))
                        )),
                    );
                } else {
                    out.insert(key.clone(), context_budget_value(value));
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn recent_messages_to_preserve(messages: &[Value]) -> Vec<Value> {
    let Some(message) = messages.last() else {
        return Vec::new();
    };
    match message.get("role").and_then(Value::as_str) {
        Some("assistant") => {
            let has_tool_calls = message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty());
            if has_tool_calls {
                vec![message.clone()]
            } else {
                Vec::new()
            }
        }
        Some("tool") => {
            let mut out = Vec::new();
            if let Some(call_id) = message.get("tool_call_id").and_then(Value::as_str) {
                if let Some(assistant) = matching_assistant_tool_call(messages, call_id) {
                    out.push(assistant.clone());
                }
            }
            out.push(message.clone());
            out
        }
        _ => Vec::new(),
    }
}

fn matching_assistant_tool_call<'a>(messages: &'a [Value], call_id: &str) -> Option<&'a Value> {
    messages.iter().rev().skip(1).find(|message| {
        message.get("role").and_then(Value::as_str) == Some("assistant")
            && message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| {
                    calls
                        .iter()
                        .any(|call| call.get("id").and_then(Value::as_str) == Some(call_id))
                })
    })
}

fn compact_recent_message(mut message: Value, max_content_tokens: usize) -> Value {
    let Some(object) = message.as_object_mut() else {
        return message;
    };
    if let Some(content) = object.get_mut("content") {
        compact_content_value(content, max_content_tokens);
    }
    message
}

fn compact_content_value(value: &mut Value, max_content_tokens: usize) {
    match value {
        Value::String(text) => {
            if approx_token_count(text) > max_content_tokens {
                *text = truncate_for_context_tokens(text, max_content_tokens);
            }
        }
        Value::Array(parts) => {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if approx_token_count(text) > max_content_tokens {
                        part["text"] =
                            Value::String(truncate_for_context_tokens(text, max_content_tokens));
                    }
                }
            }
        }
        _ => {}
    }
}

fn truncate_for_context_tokens(text: &str, max_tokens: usize) -> String {
    truncate_for_context(text, token_budget_to_char_budget(max_tokens))
}

fn truncate_for_context(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(96).max(32);
    let head = keep / 2;
    let tail = keep.saturating_sub(head);
    let head_text = text.chars().take(head).collect::<String>();
    let tail_text = text
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!(
        "{head_text}\n[truncated]\nomitted {} chars\n{tail_text}",
        char_count.saturating_sub(keep)
    )
}

fn compacted_context_system_message(context: &Value) -> Result<String> {
    let context_json = serde_json::to_string_pretty(context)?;
    let browser_agent_contract = browser_agent_contract();
    Ok(render_prompt_template(
        include_str!("../../../prompts/compacted-context-system.md"),
        &[
            ("{{browser_agent_contract}}", &browser_agent_contract),
            ("{{context_json}}", &context_json),
        ],
    ))
}

fn browser_agent_contract() -> String {
    let mut contract = include_str!("../../../prompts/browser-agent-system.md")
        .trim()
        .to_string();
    contract.push_str("\n\n## Loaded Browser-Harness Interaction Skills");
    contract.push_str(
        "\n\nThese are the same interaction-skill playbooks from browser-harness. Apply the relevant section when the page mechanic appears.",
    );
    for (path, content) in browser_harness_interaction_skills() {
        contract.push_str("\n\n### ");
        contract.push_str(path);
        contract.push_str("\n\n");
        contract.push_str(content.trim());
    }
    contract
}

fn browser_harness_interaction_skills() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "interaction-skills/connection.md",
            include_str!("../../../prompts/interaction-skills/connection.md"),
        ),
        (
            "interaction-skills/cookies.md",
            include_str!("../../../prompts/interaction-skills/cookies.md"),
        ),
        (
            "interaction-skills/cross-origin-iframes.md",
            include_str!("../../../prompts/interaction-skills/cross-origin-iframes.md"),
        ),
        (
            "interaction-skills/dialogs.md",
            include_str!("../../../prompts/interaction-skills/dialogs.md"),
        ),
        (
            "interaction-skills/downloads.md",
            include_str!("../../../prompts/interaction-skills/downloads.md"),
        ),
        (
            "interaction-skills/drag-and-drop.md",
            include_str!("../../../prompts/interaction-skills/drag-and-drop.md"),
        ),
        (
            "interaction-skills/dropdowns.md",
            include_str!("../../../prompts/interaction-skills/dropdowns.md"),
        ),
        (
            "interaction-skills/iframes.md",
            include_str!("../../../prompts/interaction-skills/iframes.md"),
        ),
        (
            "interaction-skills/network-requests.md",
            include_str!("../../../prompts/interaction-skills/network-requests.md"),
        ),
        (
            "interaction-skills/print-as-pdf.md",
            include_str!("../../../prompts/interaction-skills/print-as-pdf.md"),
        ),
        (
            "interaction-skills/profile-sync.md",
            include_str!("../../../prompts/interaction-skills/profile-sync.md"),
        ),
        (
            "interaction-skills/screenshots.md",
            include_str!("../../../prompts/interaction-skills/screenshots.md"),
        ),
        (
            "interaction-skills/scrolling.md",
            include_str!("../../../prompts/interaction-skills/scrolling.md"),
        ),
        (
            "interaction-skills/shadow-dom.md",
            include_str!("../../../prompts/interaction-skills/shadow-dom.md"),
        ),
        (
            "interaction-skills/tabs.md",
            include_str!("../../../prompts/interaction-skills/tabs.md"),
        ),
        (
            "interaction-skills/uploads.md",
            include_str!("../../../prompts/interaction-skills/uploads.md"),
        ),
        (
            "interaction-skills/viewport.md",
            include_str!("../../../prompts/interaction-skills/viewport.md"),
        ),
    ]
}

fn render_prompt_template(template: &str, replacements: &[(&str, &str)]) -> String {
    let mut rendered = template.trim().to_string();
    for (placeholder, value) in replacements {
        rendered = rendered.replace(placeholder, value);
    }
    rendered
}

fn latest_session_message_seq(store: &Store, session_id: &str) -> Result<i64> {
    Ok(store
        .events_for_session(session_id)?
        .into_iter()
        .filter(|event| {
            matches!(
                event.event_type.as_str(),
                "session.input" | "session.followup"
            )
        })
        .map(|event| event.seq)
        .max()
        .unwrap_or(0))
}

fn append_new_external_session_messages(
    store: &Store,
    session_id: &str,
    messages: &mut Vec<Value>,
    last_seq: &mut i64,
) -> Result<()> {
    let mut events = store
        .events_for_session(session_id)?
        .into_iter()
        .filter(|event| {
            event.seq > *last_seq
                && matches!(
                    event.event_type.as_str(),
                    "session.input" | "session.followup"
                )
        })
        .collect::<Vec<_>>();
    events.sort_by_key(|event| event.seq);
    for event in events {
        *last_seq = (*last_seq).max(event.seq);
        let Some(text) = event
            .payload
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        messages.push(serde_json::json!({
            "role": "user",
            "content": text,
        }));
    }
    Ok(())
}

fn provider_messages_from_events(events: &[browser_use_protocol::EventRecord]) -> Vec<Value> {
    let mut messages = Vec::new();
    let mut assistant_text = String::new();
    let mut assistant_tool_calls = Vec::<Value>::new();
    let mut tool_names = HashMap::<String, String>::new();
    let mut emitted_tool_messages = HashSet::<String>::new();

    fn flush_assistant(
        messages: &mut Vec<Value>,
        assistant_text: &mut String,
        assistant_tool_calls: &mut Vec<Value>,
    ) {
        if assistant_text.is_empty() && assistant_tool_calls.is_empty() {
            return;
        }
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": std::mem::take(assistant_text),
            "tool_calls": std::mem::take(assistant_tool_calls),
        }));
    }

    for event in events {
        match event.event_type.as_str() {
            "agent.context" => {
                flush_assistant(
                    &mut messages,
                    &mut assistant_text,
                    &mut assistant_tool_calls,
                );
                let mut sections = Vec::new();
                if let Some(role) = event.payload.get("role").and_then(Value::as_str) {
                    let canonical_task_sentence = event
                        .payload
                        .get("agent_path")
                        .and_then(Value::as_str)
                        .map(|path| {
                            format!(
                                " Canonical task name: {path}. This is an agent routing name, not a filesystem path."
                            )
                        })
                        .unwrap_or_default();
                    let explorer_instruction = if role.to_ascii_lowercase().contains("explor") {
                        "As the explorer, inspect the repository/codebase directly with local tools from the task cwd. Use `.` for the repository root; do not use your canonical task name as a directory. Do not spawn another explorer for this same repository-analysis task unless explicitly instructed."
                    } else {
                        ""
                    };
                    sections.push(render_prompt_template(
                        include_str!("../../../prompts/helper-session-identity.md"),
                        &[
                            ("{{role}}", role),
                            ("{{canonical_task_sentence}}", &canonical_task_sentence),
                            ("{{explorer_instruction}}", explorer_instruction),
                        ],
                    ));
                }
                if let Some(context) = event.payload.get("context") {
                    let context = context.to_string();
                    sections.push(render_prompt_template(
                        include_str!("../../../prompts/helper-session-inherited-context.md"),
                        &[("{{context}}", &context)],
                    ));
                }
                if !sections.is_empty() {
                    messages.push(serde_json::json!({
                        "role": "system",
                        "content": sections.join("\n\n"),
                    }));
                }
            }
            "session.input" | "session.followup" => {
                flush_assistant(
                    &mut messages,
                    &mut assistant_text,
                    &mut assistant_tool_calls,
                );
                if let Some(text) = event.payload.get("text").and_then(Value::as_str) {
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": text,
                    }));
                }
            }
            "model.delta" => {
                if let Some(text) = event.payload.get("text").and_then(Value::as_str) {
                    assistant_text.push_str(text);
                }
            }
            "model.tool_call" => {
                let call = event.payload.clone();
                if let Some(call_id) = call.get("id").and_then(Value::as_str) {
                    if let Some(name) = call.get("name").and_then(Value::as_str) {
                        tool_names.insert(call_id.to_string(), name.to_string());
                    }
                }
                assistant_tool_calls.push(call);
            }
            "tool.output" => {
                flush_assistant(
                    &mut messages,
                    &mut assistant_text,
                    &mut assistant_tool_calls,
                );
                if let Some(call_id) = event.payload.get("tool_call_id").and_then(Value::as_str) {
                    messages.push(tool_message_from_output_event(&event.payload, call_id));
                    emitted_tool_messages.insert(call_id.to_string());
                }
            }
            "tool.failed" => {
                flush_assistant(
                    &mut messages,
                    &mut assistant_text,
                    &mut assistant_tool_calls,
                );
                if let Some(call_id) = event.payload.get("tool_call_id").and_then(Value::as_str) {
                    if !emitted_tool_messages.contains(call_id) {
                        let name = event
                            .payload
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool");
                        let error = event
                            .payload
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("tool failed");
                        messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "name": name,
                            "content": format!("{name} failed: {error}"),
                        }));
                        emitted_tool_messages.insert(call_id.to_string());
                    }
                }
            }
            "tool.finished" => {
                flush_assistant(
                    &mut messages,
                    &mut assistant_text,
                    &mut assistant_tool_calls,
                );
                if let Some(call_id) = event.payload.get("tool_call_id").and_then(Value::as_str) {
                    if !emitted_tool_messages.contains(call_id) {
                        let name = event
                            .payload
                            .get("name")
                            .and_then(Value::as_str)
                            .or_else(|| tool_names.get(call_id).map(String::as_str))
                            .unwrap_or("tool");
                        messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "name": name,
                            "content": synthetic_tool_result_text(name),
                        }));
                        emitted_tool_messages.insert(call_id.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    flush_assistant(
        &mut messages,
        &mut assistant_text,
        &mut assistant_tool_calls,
    );
    normalize_provider_messages(&mut messages);
    messages
}

#[derive(Clone, Debug)]
struct PendingToolCall {
    id: String,
    name: String,
}

fn normalize_provider_messages(messages: &mut Vec<Value>) {
    let mut normalized = Vec::with_capacity(messages.len());
    let mut pending = Vec::<PendingToolCall>::new();
    let mut emitted_outputs = HashSet::<String>::new();

    for message in std::mem::take(messages) {
        match message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
        {
            "assistant" => {
                append_synthetic_outputs_for_pending(
                    &mut normalized,
                    &mut pending,
                    &mut emitted_outputs,
                );
                if let Some((assistant, calls)) = normalized_assistant_message(message) {
                    for call in calls {
                        pending.push(call);
                    }
                    normalized.push(assistant);
                }
            }
            "tool" => {
                let call_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let Some(call_id) = call_id else {
                    if let Some(context) = orphan_tool_output_context_message(&message, "<missing>")
                    {
                        normalized.push(context);
                    }
                    continue;
                };
                if let Some(index) = pending.iter().position(|call| call.id == call_id) {
                    let call = pending.remove(index);
                    normalized.push(normalized_tool_message(message, &call.id, &call.name));
                    emitted_outputs.insert(call.id);
                } else if let Some(context) = orphan_tool_output_context_message(&message, &call_id)
                {
                    normalized.push(context);
                }
            }
            _ => {
                append_synthetic_outputs_for_pending(
                    &mut normalized,
                    &mut pending,
                    &mut emitted_outputs,
                );
                normalized.push(message);
            }
        }
    }

    append_synthetic_outputs_for_pending(&mut normalized, &mut pending, &mut emitted_outputs);
    *messages = normalized;
}

fn append_synthetic_outputs_for_pending(
    messages: &mut Vec<Value>,
    pending: &mut Vec<PendingToolCall>,
    emitted_outputs: &mut HashSet<String>,
) {
    for call in pending.drain(..) {
        if emitted_outputs.insert(call.id.clone()) {
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": call.id,
                "name": call.name,
                "content": format!(
                    "{} output was unavailable after history normalization; continue with the available context.",
                    call.name
                ),
            }));
        }
    }
}

fn normalized_assistant_message(mut message: Value) -> Option<(Value, Vec<PendingToolCall>)> {
    let calls = normalized_assistant_tool_calls(&message);
    let text = message_content_text(&message);
    if text.trim().is_empty() && calls.is_empty() {
        return None;
    }
    let pending = calls
        .iter()
        .filter_map(|call| {
            Some(PendingToolCall {
                id: call.get("id").and_then(Value::as_str)?.to_string(),
                name: call.get("name").and_then(Value::as_str)?.to_string(),
            })
        })
        .collect::<Vec<_>>();
    let Some(object) = message.as_object_mut() else {
        return None;
    };
    object.insert("role".to_string(), Value::String("assistant".to_string()));
    if calls.is_empty() {
        object.remove("tool_calls");
    } else {
        object.insert("tool_calls".to_string(), Value::Array(calls));
    }
    Some((message, pending))
}

fn normalized_assistant_tool_calls(message: &Value) -> Vec<Value> {
    let mut seen = HashSet::new();
    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|call| {
            let id = call
                .get("id")
                .or_else(|| call.get("call_id"))
                .and_then(Value::as_str)?
                .to_string();
            if !seen.insert(id.clone()) {
                return None;
            }
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| {
                    call.get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                })?
                .to_string();
            let arguments = call
                .get("arguments")
                .cloned()
                .or_else(|| {
                    call.get("function")
                        .and_then(|function| function.get("arguments"))
                        .and_then(Value::as_str)
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                })
                .unwrap_or_else(|| serde_json::json!({}));
            Some(serde_json::json!({
                "id": id,
                "name": name,
                "arguments": arguments,
            }))
        })
        .collect()
}

fn normalized_tool_message(mut message: Value, call_id: &str, name: &str) -> Value {
    let object = message
        .as_object_mut()
        .expect("tool message should be a JSON object");
    object.insert("role".to_string(), Value::String("tool".to_string()));
    object.insert(
        "tool_call_id".to_string(),
        Value::String(call_id.to_string()),
    );
    if !object
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
    {
        object.insert("name".to_string(), Value::String(name.to_string()));
    }
    if !object.contains_key("content") {
        object.insert(
            "content".to_string(),
            Value::String(synthetic_tool_result_text(name)),
        );
    }
    message
}

fn orphan_tool_output_context_message(message: &Value, call_id: &str) -> Option<Value> {
    let text = message_content_text(message);
    let images = tool_message_input_images(message);
    if text.trim().is_empty() && images.is_empty() {
        return None;
    }
    let mut content = vec![serde_json::json!({
        "type": "input_text",
        "text": format!(
            "Tool output retained as context after history normalization. Original tool call {call_id} ({}):\n{}",
            message.get("name").and_then(Value::as_str).unwrap_or("tool"),
            text,
        ),
    })];
    content.extend(images);
    Some(serde_json::json!({
        "role": "user",
        "content": content,
    }))
}

fn tool_message_input_images(message: &Value) -> Vec<Value> {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
        .cloned()
        .collect()
}

fn tool_message_from_output_event(payload: &Value, call_id: &str) -> Value {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let mut text = payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(data) = payload.get("data").filter(|data| !data.is_null()) {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("data: ");
        text.push_str(&data.to_string());
    }
    if text.trim().is_empty() {
        text = format!("{name} completed");
    }

    let image_parts = payload
        .get("images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(replay_image_part)
        .collect::<Vec<_>>();
    let content = if image_parts.is_empty() {
        Value::String(text)
    } else {
        let mut parts = vec![serde_json::json!({
            "type": "output_text",
            "text": text,
        })];
        parts.extend(image_parts);
        Value::Array(parts)
    };
    serde_json::json!({
        "role": "tool",
        "tool_call_id": call_id,
        "name": name,
        "content": content,
    })
}

fn replay_image_part(image: &Value) -> Option<Value> {
    let path = image.get("path").and_then(Value::as_str)?;
    let bytes = std::fs::read(path).ok()?;
    let mime_type = image
        .get("mime_type")
        .and_then(Value::as_str)
        .or_else(|| image.get("mime").and_then(Value::as_str))
        .unwrap_or("image/png");
    let mut part = serde_json::json!({
        "type": "input_image",
        "image_url": format!("data:{mime_type};base64,{}", general_purpose::STANDARD.encode(bytes)),
    });
    if let Some(detail) = image.get("detail").and_then(Value::as_str) {
        part["detail"] = Value::String(detail.to_string());
    }
    Some(part)
}

fn synthetic_tool_result_text(name: &str) -> String {
    match name {
        "update_plan" => "plan updated".to_string(),
        "done" => "done".to_string(),
        other => format!("{other} completed"),
    }
}

fn task_text_from_provider_messages(messages: &[Value]) -> Option<String> {
    messages.iter().find_map(|message| {
        (message.get("role").and_then(Value::as_str) == Some("user"))
            .then(|| message_content_text(message))
            .filter(|text| !text.trim().is_empty())
    })
}

fn message_content_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

struct ToolDispatchOutcome {
    finished: bool,
    messages: Vec<Value>,
}

fn browser_tool_specs() -> Vec<ToolSpec> {
    ToolRegistry::browser_agent().specs()
}

fn tool_call_message(call: &ToolCall) -> Value {
    serde_json::json!({
        "id": call.id,
        "name": call.name,
        "arguments": call.arguments,
    })
}

#[allow(clippy::too_many_arguments)]
fn dispatch_tool_calls_for_turn<P: ModelProvider>(
    store: &Store,
    provider: &P,
    session: &browser_use_protocol::SessionMeta,
    worker: &mut PythonWorker,
    tool_calls: Vec<ToolCall>,
    options: &AgentRunOptions,
    telemetry: &AgentTelemetry,
    step_span: &telemetry::ActiveSpan,
    turn_idx: usize,
) -> Result<Vec<ToolDispatchOutcome>> {
    let registry = ToolRegistry::browser_agent();
    let mut outcomes = Vec::new();
    let mut index = 0;
    while index < tool_calls.len() {
        ensure_not_cancelled(store, &session.id)?;
        if tool_call_supports_parallel(&registry, &tool_calls[index]) {
            let batch_start = index;
            index += 1;
            while index < tool_calls.len()
                && tool_call_supports_parallel(&registry, &tool_calls[index])
            {
                index += 1;
            }
            let batch = tool_calls[batch_start..index].to_vec();
            outcomes.extend(dispatch_parallel_tool_batch(
                store, session, batch, telemetry, step_span, turn_idx,
            )?);
            continue;
        }

        let call = tool_calls[index].clone();
        index += 1;
        let outcome = dispatch_serial_tool_call_for_turn(
            store, provider, session, worker, &call, options, telemetry, step_span, turn_idx,
        )?;
        let finished = outcome.finished;
        outcomes.push(outcome);
        if finished {
            break;
        }
    }
    Ok(outcomes)
}

fn dispatch_parallel_tool_batch(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    batch: Vec<ToolCall>,
    telemetry: &AgentTelemetry,
    step_span: &telemetry::ActiveSpan,
    turn_idx: usize,
) -> Result<Vec<ToolDispatchOutcome>> {
    if batch.is_empty() {
        return Ok(Vec::new());
    }
    if batch.len() == 1 {
        let call = batch.into_iter().next().expect("batch not empty");
        return Ok(vec![dispatch_parallel_tool_call_for_turn(
            store, session, call, telemetry, step_span, turn_idx,
        )?]);
    }

    store.append_event(
        &session.id,
        "tool.batch_started",
        serde_json::json!({
            "mode": "parallel",
            "tool_call_ids": batch.iter().map(|call| call.id.clone()).collect::<Vec<_>>(),
            "tools": batch.iter().map(|call| call.name.clone()).collect::<Vec<_>>(),
        }),
    )?;
    let tool_spans = batch
        .iter()
        .map(|call| telemetry.start_tool_span(step_span, &session.id, turn_idx, call))
        .collect::<Vec<_>>();
    let state_dir = store.state_dir().to_path_buf();
    let notifier = store.notifier();
    let handles = batch
        .iter()
        .cloned()
        .map(|call| {
            let state_dir = state_dir.clone();
            let session = session.clone();
            let notifier = notifier.clone();
            thread::spawn(move || {
                let store = Store::open_with_optional_notifier(state_dir, notifier)?;
                dispatch_parallel_tool_call_recoverably(&store, &session, &call)
            })
        })
        .collect::<Vec<_>>();

    let mut outcomes = Vec::with_capacity(batch.len());
    for ((call, tool_span), handle) in batch.into_iter().zip(tool_spans).zip(handles) {
        let outcome = match handle.join() {
            Ok(result) => match result {
                Ok(outcome) => {
                    record_tool_success(telemetry, &tool_span, &outcome);
                    outcome
                }
                Err(error) => {
                    tool_span.record_error(error.as_ref());
                    return Err(error);
                }
            },
            Err(payload) => {
                let error = anyhow!(
                    "parallel tool task panicked: {}",
                    panic_payload_text(payload)
                );
                tool_span.record_error(error.as_ref());
                return Err(error);
            }
        };
        drop(tool_span);
        store.append_event(
            &session.id,
            "tool.batch_result",
            serde_json::json!({
                "mode": "parallel",
                "tool_call_id": call.id,
                "name": call.name,
                "message_count": outcome.messages.len(),
            }),
        )?;
        outcomes.push(outcome);
    }
    store.append_event(
        &session.id,
        "tool.batch_finished",
        serde_json::json!({
            "mode": "parallel",
            "count": outcomes.len(),
        }),
    )?;
    Ok(outcomes)
}

#[allow(clippy::too_many_arguments)]
fn dispatch_serial_tool_call_for_turn<P: ModelProvider>(
    store: &Store,
    provider: &P,
    session: &browser_use_protocol::SessionMeta,
    worker: &mut PythonWorker,
    call: &ToolCall,
    options: &AgentRunOptions,
    telemetry: &AgentTelemetry,
    step_span: &telemetry::ActiveSpan,
    turn_idx: usize,
) -> Result<ToolDispatchOutcome> {
    let tool_span = telemetry.start_tool_span(step_span, &session.id, turn_idx, call);
    let outcome =
        match dispatch_tool_call_recoverably(store, provider, session, worker, call, options) {
            Ok(outcome) => {
                record_tool_success(telemetry, &tool_span, &outcome);
                outcome
            }
            Err(error) => {
                tool_span.record_error(error.as_ref());
                return Err(error);
            }
        };
    drop(tool_span);
    Ok(outcome)
}

fn dispatch_parallel_tool_call_for_turn(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: ToolCall,
    telemetry: &AgentTelemetry,
    step_span: &telemetry::ActiveSpan,
    turn_idx: usize,
) -> Result<ToolDispatchOutcome> {
    let tool_span = telemetry.start_tool_span(step_span, &session.id, turn_idx, &call);
    let outcome = match dispatch_parallel_tool_call_recoverably(store, session, &call) {
        Ok(outcome) => {
            record_tool_success(telemetry, &tool_span, &outcome);
            outcome
        }
        Err(error) => {
            tool_span.record_error(error.as_ref());
            return Err(error);
        }
    };
    drop(tool_span);
    Ok(outcome)
}

fn record_tool_success(
    telemetry: &AgentTelemetry,
    tool_span: &telemetry::ActiveSpan,
    outcome: &ToolDispatchOutcome,
) {
    telemetry.record_tool_outcome(tool_span, &outcome.messages, outcome.finished);
    tool_span.set_attribute(KeyValue::new(
        "browser_use.tool.finished_session",
        outcome.finished,
    ));
    tool_span.set_attribute(KeyValue::new(
        "browser_use.tool.message_count",
        outcome.messages.len() as i64,
    ));
    tool_span.set_ok();
}

fn dispatch_tool_call_recoverably<P: ModelProvider>(
    store: &Store,
    provider: &P,
    session: &browser_use_protocol::SessionMeta,
    worker: &mut PythonWorker,
    call: &ToolCall,
    options: &AgentRunOptions,
) -> Result<ToolDispatchOutcome> {
    match dispatch_tool_call(store, provider, session, worker, call, options) {
        Ok(outcome) => Ok(outcome),
        Err(error) if tool_error_is_recoverable(&error) => {
            dispatch_recovered_tool_error(store, session, call, error)
        }
        Err(error) => Err(error),
    }
}

fn dispatch_parallel_tool_call_recoverably(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    match dispatch_parallel_tool_call(store, session, call) {
        Ok(outcome) => Ok(outcome),
        Err(error) if tool_error_is_recoverable(&error) => {
            dispatch_recovered_tool_error(store, session, call, error)
        }
        Err(error) => Err(error),
    }
}

fn dispatch_recovered_tool_error(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
    error: anyhow::Error,
) -> Result<ToolDispatchOutcome> {
    let error = format!("{error:#}");
    store.append_event(
        &session.id,
        "tool.failed",
        serde_json::json!({
            "name": call.name,
            "tool_call_id": call.id,
            "error": error,
            "recovered": true,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_text_message(
            store,
            session,
            call,
            &call.name,
            &format!("{} failed: {error}", call.name),
        )?],
    })
}

fn tool_error_is_recoverable(error: &anyhow::Error) -> bool {
    !format!("{error:#}").contains("agent cancelled")
}

fn dispatch_parallel_tool_call(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    match ToolRegistry::browser_agent().handler_for(&call.name) {
        Some(ToolHandlerKind::ExecCommand) => dispatch_exec_command_tool(store, session, call),
        Some(ToolHandlerKind::ReadFile) => dispatch_read_file_tool(store, session, call),
        Some(ToolHandlerKind::SearchFiles) => dispatch_search_files_tool(store, session, call),
        Some(ToolHandlerKind::ListFiles) => dispatch_list_files_tool(store, session, call),
        Some(ToolHandlerKind::ViewImage) => dispatch_view_image_tool(store, session, call),
        _ => dispatch_unknown_tool(store, session, call),
    }
}

fn tool_call_supports_parallel(registry: &ToolRegistry, call: &ToolCall) -> bool {
    match registry.handler_for(&call.name) {
        Some(
            ToolHandlerKind::ReadFile | ToolHandlerKind::SearchFiles | ToolHandlerKind::ListFiles,
        ) => true,
        Some(ToolHandlerKind::ExecCommand) => {
            tools::command::exec_command_is_known_read_only(&call.arguments)
        }
        _ => false,
    }
}

fn panic_payload_text(payload: Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn dispatch_tool_call<P: ModelProvider>(
    store: &Store,
    provider: &P,
    session: &browser_use_protocol::SessionMeta,
    worker: &mut PythonWorker,
    call: &ToolCall,
    options: &AgentRunOptions,
) -> Result<ToolDispatchOutcome> {
    let registry = ToolRegistry::browser_agent();
    let Some(handler) = registry
        .handler_for(&call.name)
        .or_else(|| (call.name == "python").then_some(ToolHandlerKind::Python))
    else {
        return dispatch_unknown_tool(store, session, call);
    };
    match handler {
        ToolHandlerKind::Done => dispatch_done_tool(store, session, call),
        ToolHandlerKind::Browser => dispatch_browser_tool(store, session, call),
        ToolHandlerKind::BrowserScript => {
            dispatch_browser_script_tool(store, session, call, options.python_tool_timeout_seconds)
        }
        ToolHandlerKind::Python => dispatch_python_tool(
            store,
            session,
            worker,
            call,
            options.python_tool_timeout_seconds,
        ),
        ToolHandlerKind::ExecCommand => dispatch_exec_command_tool(store, session, call),
        ToolHandlerKind::WriteStdin => dispatch_write_stdin_tool(store, session, call),
        ToolHandlerKind::ApplyPatch => dispatch_apply_patch_tool(store, session, call),
        ToolHandlerKind::ReadFile => dispatch_read_file_tool(store, session, call),
        ToolHandlerKind::SearchFiles => dispatch_search_files_tool(store, session, call),
        ToolHandlerKind::ListFiles => dispatch_list_files_tool(store, session, call),
        ToolHandlerKind::ViewImage => dispatch_view_image_tool(store, session, call),
        ToolHandlerKind::UpdatePlan => dispatch_update_plan_tool(store, session, call),
        ToolHandlerKind::SpawnAgent => {
            dispatch_spawn_agent_tool(store, provider, session, call, options)
        }
        ToolHandlerKind::WaitAgent => dispatch_wait_agent_tool(store, session, call),
        ToolHandlerKind::SendInput => {
            dispatch_agent_message_tool(store, provider, session, call, "send_input", true, options)
        }
        ToolHandlerKind::SendMessage => dispatch_agent_message_tool(
            store,
            provider,
            session,
            call,
            "send_message",
            false,
            options,
        ),
        ToolHandlerKind::FollowupTask => dispatch_agent_message_tool(
            store,
            provider,
            session,
            call,
            "followup_task",
            true,
            options,
        ),
        ToolHandlerKind::ListAgents => dispatch_list_agents_tool(store, session, call),
        ToolHandlerKind::CloseAgent => dispatch_close_agent_tool(store, session, call),
    }
}

fn dispatch_exec_command_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let result = tools::command::exec_command(store, session, call)?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_json_message(
            store,
            session,
            call,
            "exec_command",
            result.content,
        )?],
    })
}

fn dispatch_write_stdin_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let result = tools::command::write_stdin(store, session, call)?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_json_message(
            store,
            session,
            call,
            "write_stdin",
            result.content,
        )?],
    })
}

fn dispatch_apply_patch_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let result = tools::files::apply_patch_tool(store, session, call)?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_content_message(
            store,
            session,
            call,
            "apply_patch",
            result.content,
        )?],
    })
}

fn dispatch_read_file_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let result = tools::files::read_file(store, session, call)?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_content_message(
            store,
            session,
            call,
            "read_file",
            result.content,
        )?],
    })
}

fn dispatch_search_files_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let result = tools::files::search_files(store, session, call)?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_content_message(
            store,
            session,
            call,
            "search_files",
            result.content,
        )?],
    })
}

fn dispatch_list_files_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let result = tools::files::list_files(store, session, call)?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_content_message(
            store,
            session,
            call,
            "list_files",
            result.content,
        )?],
    })
}

fn dispatch_view_image_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let result = tools::files::view_image(store, session, call)?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_content_message(
            store,
            session,
            call,
            "view_image",
            result.content,
        )?],
    })
}

fn dispatch_browser_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let cmd = call
        .arguments
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if cmd.is_empty() {
        return dispatch_tool_validation_error(store, session, call, "browser requires cmd");
    }
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "browser",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let output = if let Some(content) = dispatch_browser_preference_command(store, session, cmd)? {
        browser_use_browser::BrowserCommandOutput {
            content,
            events: Vec::new(),
        }
    } else {
        let resolved_cmd = resolve_browser_connect_command(store, cmd)?;
        browser_use_browser::run_browser_command(
            &session.id,
            &session.cwd,
            &session.artifact_root,
            &resolved_cmd,
        )?
    };
    for event in &output.events {
        record_python_browser_event(store, &session.id, event)?;
    }
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({
            "name": "browser",
            "tool_call_id": call.id,
            "output": output.content,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_content_message(
            store,
            session,
            call,
            "browser",
            output.content,
        )?],
    })
}

fn dispatch_browser_preference_command(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    cmd: &str,
) -> Result<Option<Value>> {
    let argv = browser_command_words(cmd);
    let args = strip_browser_prefix(&argv);
    let Some(first) = args.first().map(String::as_str) else {
        return Ok(None);
    };
    match first {
        "preference" | "preferences" => Ok(Some(dispatch_browser_preference(store, &args)?)),
        "profile" | "profiles"
            if args.get(1).is_some_and(|arg| {
                matches!(
                    arg.as_str(),
                    "suggest" | "use" | "remember" | "forget" | "current"
                )
            }) =>
        {
            Ok(Some(dispatch_browser_profile_preference(
                store, session, &args,
            )?))
        }
        _ => Ok(None),
    }
}

fn dispatch_browser_preference(store: &Store, args: &[String]) -> Result<Value> {
    match args.get(1).map(String::as_str) {
        None | Some("--json") | Some("show") => browser_preference_json(store),
        Some("use") => {
            let mode = args.get(2).map(String::as_str).ok_or_else(|| {
                anyhow!("browser preference use requires <local|cloud|managed-headless>")
            })?;
            let normalized = normalize_browser_preference_mode(mode)?;
            store.set_setting(BROWSER_PREF_MODE, normalized)?;
            store.set_setting("browser", browser_display_name(normalized))?;
            Ok(json!({
                "status": "ok",
                "preference": browser_preference_json(store)?,
                "next_step": "browser connect",
            }))
        }
        Some(other) => bail!("unknown browser preference command: {other}"),
    }
}

fn dispatch_browser_profile_preference(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    args: &[String],
) -> Result<Value> {
    match args.get(1).map(String::as_str) {
        Some("current") => browser_preference_json(store),
        Some("use") => {
            let profile_id = args
                .get(2)
                .map(String::as_str)
                .ok_or_else(|| anyhow!("browser profile use requires <profile-id>"))?;
            store.set_setting(BROWSER_PREF_PROFILE, profile_id)?;
            Ok(json!({
                "status": "ok",
                "profile_id": profile_id,
                "next_step": "browser connect",
            }))
        }
        Some("remember") => {
            let domain = option_value_core(args, "--domain")
                .ok_or_else(|| anyhow!("browser profile remember requires --domain <domain>"))?;
            let profile_id = option_value_core(args, "--profile")
                .or_else(|| args.get(2).cloned())
                .ok_or_else(|| {
                    anyhow!("browser profile remember requires --profile <profile-id>")
                })?;
            let mode = option_value_core(args, "--mode")
                .map(|mode| normalize_browser_preference_mode(&mode).map(ToOwned::to_owned))
                .transpose()?
                .or_else(|| store.get_setting(BROWSER_PREF_MODE).ok().flatten())
                .unwrap_or_else(|| "local".to_string());
            let value = json!({
                "domain": normalize_domain(&domain),
                "mode": mode,
                "profile_id": profile_id,
            });
            store.set_setting(
                &browser_domain_profile_key(&domain),
                &serde_json::to_string(&value)?,
            )?;
            store.set_setting(BROWSER_PREF_MODE, value["mode"].as_str().unwrap_or("local"))?;
            store.set_setting(
                BROWSER_PREF_PROFILE,
                value["profile_id"].as_str().unwrap_or(""),
            )?;
            store.set_setting(
                "browser",
                browser_display_name(value["mode"].as_str().unwrap_or("local")),
            )?;
            Ok(json!({
                "status": "ok",
                "remembered": value,
                "next_step": "browser connect",
            }))
        }
        Some("forget") => {
            let domain = option_value_core(args, "--domain")
                .or_else(|| args.get(2).cloned())
                .ok_or_else(|| anyhow!("browser profile forget requires --domain <domain>"))?;
            store.delete_setting(&browser_domain_profile_key(&domain))?;
            Ok(json!({ "status": "ok", "forgot_domain": normalize_domain(&domain) }))
        }
        Some("suggest") => {
            let domain = option_value_core(args, "--domain")
                .or_else(|| args.get(2).cloned())
                .ok_or_else(|| anyhow!("browser profile suggest requires --domain <domain>"))?;
            let remembered = remembered_domain_profile(store, &domain)?;
            let profiles = browser_use_browser::run_browser_command(
                &session.id,
                &session.cwd,
                &session.artifact_root,
                "browser local profiles --json",
            )
            .map(|output| output.content)
            .unwrap_or_else(|error| {
                json!({
                    "status": "failed",
                    "error": format!("{error:#}"),
                    "profiles": [],
                })
            });
            Ok(json!({
                "status": "ok",
                "domain": normalize_domain(&domain),
                "remembered": remembered,
                "local_profiles": profiles.get("profiles").cloned().unwrap_or_else(|| json!([])),
                "next_step": if remembered.is_some() { "browser connect" } else { "Ask the user which profile to use, then run browser profile remember --domain <domain> --profile <profile-id>" },
            }))
        }
        Some(other) => bail!("unknown browser profile command: {other}"),
        None => browser_preference_json(store),
    }
}

fn resolve_browser_connect_command(store: &Store, cmd: &str) -> Result<String> {
    let argv = browser_command_words(cmd);
    let args = strip_browser_prefix(&argv);
    if args.len() == 1 && args.first().is_some_and(|arg| arg == "connect") {
        let mode = store
            .get_setting(BROWSER_PREF_MODE)?
            .unwrap_or_else(|| "local".to_string());
        let command = match normalize_browser_preference_mode(&mode)? {
            "cloud" => "browser remote start",
            "managed-headless" => "browser connect managed --headless",
            "managed-headed" => "browser connect managed --headed",
            "local" => "browser connect local",
            _ => "browser connect local",
        };
        Ok(command.to_string())
    } else {
        Ok(cmd.to_string())
    }
}

fn browser_preference_json(store: &Store) -> Result<Value> {
    let mode = store
        .get_setting(BROWSER_PREF_MODE)?
        .or_else(|| {
            store
                .get_setting("browser")
                .ok()
                .flatten()
                .and_then(|value| display_browser_to_mode(&value).map(ToOwned::to_owned))
        })
        .unwrap_or_else(|| "local".to_string());
    let profile_id = store.get_setting(BROWSER_PREF_PROFILE)?;
    let domain_profiles = store
        .list_settings()?
        .into_iter()
        .filter_map(|(key, value)| {
            key.strip_prefix(BROWSER_DOMAIN_PROFILE_PREFIX)
                .and_then(|domain| {
                    serde_json::from_str::<Value>(&value)
                        .ok()
                        .map(|value| (domain.to_string(), value))
                })
        })
        .map(|(domain, value)| json!({ "domain": domain, "preference": value }))
        .collect::<Vec<_>>();
    Ok(json!({
        "mode": normalize_browser_preference_mode(&mode)?,
        "display": browser_display_name(normalize_browser_preference_mode(&mode)?),
        "profile_id": profile_id,
        "domain_profiles": domain_profiles,
        "connect_command": match normalize_browser_preference_mode(&mode)? {
            "cloud" => "browser remote start",
            "managed-headless" => "browser connect managed --headless",
            "managed-headed" => "browser connect managed --headed",
            _ => "browser connect local",
        },
    }))
}

fn remembered_domain_profile(store: &Store, domain: &str) -> Result<Option<Value>> {
    store
        .get_setting(&browser_domain_profile_key(domain))?
        .map(|raw| serde_json::from_str::<Value>(&raw).map_err(Into::into))
        .transpose()
}

fn browser_command_words(cmd: &str) -> Vec<String> {
    cmd.split_whitespace().map(ToOwned::to_owned).collect()
}

fn strip_browser_prefix(argv: &[String]) -> Vec<String> {
    if argv.first().is_some_and(|arg| arg == "browser") {
        argv[1..].to_vec()
    } else {
        argv.to_vec()
    }
}

fn option_value_core(argv: &[String], flag: &str) -> Option<String> {
    argv.windows(2)
        .find(|window| window[0] == flag)
        .map(|window| window[1].clone())
}

fn normalize_domain(domain: &str) -> String {
    domain
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.")
        .trim_matches('/')
        .to_ascii_lowercase()
}

fn browser_domain_profile_key(domain: &str) -> String {
    format!(
        "{BROWSER_DOMAIN_PROFILE_PREFIX}{}",
        normalize_domain(domain)
    )
}

fn normalize_browser_preference_mode(mode: &str) -> Result<&'static str> {
    let normalized = mode.to_ascii_lowercase().replace(['_', ' '], "-");
    match normalized.as_str() {
        "local" | "local-chrome" => Ok("local"),
        "cloud" | "browser-use-cloud" => Ok("cloud"),
        "headless" | "headless-chromium" | "managed-headless" => Ok("managed-headless"),
        "managed" | "managed-headed" | "headed" => Ok("managed-headed"),
        other => bail!("unknown browser preference mode: {other}"),
    }
}

fn browser_display_name(mode: &str) -> &'static str {
    match mode {
        "cloud" => "Browser Use cloud",
        "managed-headless" => "Headless Chromium",
        "managed-headed" => "Managed Chromium",
        _ => "Local Chrome",
    }
}

fn display_browser_to_mode(display: &str) -> Option<&'static str> {
    match display {
        "Browser Use cloud" => Some("cloud"),
        "Headless Chromium" => Some("managed-headless"),
        "Managed Chromium" => Some("managed-headed"),
        "Local Chrome" => Some("local"),
        _ => None,
    }
}

fn dispatch_browser_script_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
    timeout_seconds: u64,
) -> Result<ToolDispatchOutcome> {
    let code = call
        .arguments
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "browser_script",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let response = browser_use_browser::run_browser_script(
        &session.id,
        &session.cwd,
        &session.artifact_root,
        code,
        timeout_seconds,
    )?;
    record_browser_script_response_events(store, &session.id, &response)?;
    if response.ok {
        store.append_event(
            &session.id,
            "tool.finished",
            serde_json::json!({
                "name": "browser_script",
                "tool_call_id": call.id,
            }),
        )?;
    } else {
        store.append_event(
            &session.id,
            "tool.failed",
            serde_json::json!({
                "name": "browser_script",
                "tool_call_id": call.id,
                "error": response.error,
            }),
        )?;
    }
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_content_message(
            store,
            session,
            call,
            "browser_script",
            browser_script_tool_message_content_value(&response)?,
        )?],
    })
}

fn dispatch_update_plan_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let Some(plan) = call.arguments.get("plan").and_then(Value::as_array) else {
        return dispatch_tool_validation_error(store, session, call, "update_plan requires plan");
    };
    let mut in_progress = 0;
    for item in plan {
        let step = item
            .get("step")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let status = item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if step.is_empty() {
            return dispatch_tool_validation_error(
                store,
                session,
                call,
                "update_plan plan items require step",
            );
        }
        if !matches!(status, "pending" | "in_progress" | "completed") {
            return dispatch_tool_validation_error(
                store,
                session,
                call,
                "update_plan statuses must be pending, in_progress, or completed",
            );
        }
        if status == "in_progress" {
            in_progress += 1;
        }
    }
    if in_progress > 1 {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            "update_plan allows at most one in_progress item",
        );
    }
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "update_plan",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let explanation = call
        .arguments
        .get("explanation")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    store.append_event(
        &session.id,
        "plan.updated",
        serde_json::json!({
            "tool_call_id": call.id,
            "explanation": explanation,
            "plan": plan,
        }),
    )?;
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({
            "name": "update_plan",
            "tool_call_id": call.id,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_json_message(
            store,
            session,
            call,
            "update_plan",
            serde_json::json!({
                "status": "updated",
                "items": plan.len(),
            }),
        )?],
    })
}

fn dispatch_done_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let requested_result = call
        .arguments
        .get("result")
        .and_then(Value::as_str)
        .or_else(|| call.arguments.get("text").and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    let requested_result_file = call
        .arguments
        .get("result_file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let result_file = requested_result_file
        .map(|path| done_result_file_info(session, path))
        .transpose()?;
    if call.arguments.get("use_final_answer").is_some()
        || matches!(
            requested_result.as_str(),
            "__use_final_answer__" | "__final_answer__" | "FINAL_ANSWER"
        )
    {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            "done no longer supports persisted final-answer state. Pass result_file for saved browser_script output, or pass a non-empty result.",
        );
    }
    if requested_result.is_empty() && result_file.is_none() {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            "done requires a non-empty result or result_file",
        );
    }
    if let Some(error) = guard_or_close_active_descendants(store, &session.id, &call.arguments)? {
        return dispatch_tool_validation_error(store, session, call, &error);
    }
    let result = if let Some(result_file) = result_file.as_ref() {
        done_result_file_message(result_file)
    } else {
        requested_result
    };
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "done",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({
            "name": "done",
            "tool_call_id": call.id,
        }),
    )?;
    let mut done_payload = serde_json::json!({ "result": result });
    if let Some(result_file) = result_file.as_ref() {
        let directory = result_file
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let artifact = serde_json::json!({
            "kind": "file",
            "path": result_file.path.display().to_string(),
            "mime": result_file.mime,
            "bytes": result_file.bytes,
            "source": "done.result_file",
        });
        record_tool_artifact(store, &session.id, "done", &artifact)?;
        done_payload["source"] = Value::String("done.result_file".to_string());
        done_payload["result_file"] = Value::String(result_file.requested_path.clone());
        done_payload["result_file_path"] = Value::String(result_file.path.display().to_string());
        done_payload["result_file_url"] = Value::String(file_url(&result_file.path));
        done_payload["result_file_directory"] = Value::String(directory.display().to_string());
        done_payload["result_file_directory_url"] = Value::String(file_url(&directory));
        done_payload["result_file_bytes"] = Value::from(result_file.bytes);
        done_payload["result_file_mime"] = Value::String(result_file.mime.to_string());
    }
    store.append_event(&session.id, "session.done", done_payload)?;
    Ok(ToolDispatchOutcome {
        finished: true,
        messages: Vec::new(),
    })
}

fn done_result_file_info(
    session: &browser_use_protocol::SessionMeta,
    requested_path: &str,
) -> Result<DoneResultFile> {
    let path = resolve_done_result_file(session, requested_path);
    let path = path
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            anyhow!("done result_file does not exist or is not a file: {requested_path}")
        })?;
    let metadata =
        std::fs::metadata(&path).with_context(|| format!("read metadata {}", path.display()))?;
    if metadata.len() == 0 {
        bail!("done result_file is empty: {}", path.display());
    }
    Ok(DoneResultFile {
        requested_path: requested_path.to_string(),
        mime: guess_mime_from_path(&path),
        path,
        bytes: metadata.len(),
    })
}

fn done_result_file_message(result_file: &DoneResultFile) -> String {
    let directory = result_file.path.parent().unwrap_or_else(|| Path::new("."));
    format!(
        "Saved result file.\n\nFile:\n{}\n\nDirectory:\n{}\n\nLocal path:\n{}\nSize: {} bytes\nMIME: {}\n\nThe full result is in the file above. It is not inlined here so the terminal stays responsive.",
        file_url(&result_file.path),
        file_url(directory),
        result_file.path.display(),
        result_file.bytes,
        result_file.mime,
    )
}

fn guess_mime_from_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "json" => "application/json",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "md" => "text/markdown",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "txt" | "log" => "text/plain",
        _ => "application/octet-stream",
    }
}

fn file_url(path: &Path) -> String {
    let path = path.display().to_string().replace('\\', "/");
    let prefix = if path.starts_with('/') {
        "file://"
    } else {
        "file:///"
    };
    format!("{prefix}{}", encode_file_url_path(&path))
}

fn encode_file_url_path(path: &str) -> String {
    let mut out = String::new();
    for byte in path.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b':' | b'.' | b'-' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn resolve_done_result_file(
    session: &browser_use_protocol::SessionMeta,
    requested_path: &str,
) -> Vec<PathBuf> {
    let path = Path::new(requested_path);
    if path.is_absolute() {
        return vec![path.to_path_buf()];
    }
    vec![
        Path::new(&session.cwd).join(path),
        Path::new(&session.artifact_root).join(path),
        path.to_path_buf(),
    ]
}

fn guard_or_close_active_descendants(
    store: &Store,
    session_id: &str,
    arguments: &Value,
) -> Result<Option<String>> {
    let active = active_descendant_agents(store, session_id)?;
    if active.is_empty() {
        return Ok(None);
    }
    if arguments
        .get("finish_and_close_children")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        close_direct_active_child_subtrees(store, session_id)?;
        return Ok(None);
    }
    let names = active
        .iter()
        .map(|agent| {
            agent
                .agent_path
                .clone()
                .unwrap_or_else(|| agent.child_session_id.clone())
        })
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Some(format!(
        "done cannot finish while subagents are still active: {names}. Call wait_agent, close_agent, or pass finish_and_close_children=true."
    )))
}

fn active_descendant_agents(store: &Store, session_id: &str) -> Result<Vec<AgentSummary>> {
    let mut active = Vec::new();
    collect_active_descendant_agents(store, session_id, &mut active)?;
    Ok(active)
}

fn collect_active_descendant_agents(
    store: &Store,
    session_id: &str,
    active: &mut Vec<AgentSummary>,
) -> Result<()> {
    for agent in store.list_child_agents(session_id)? {
        let child_is_active = store
            .load_session(&agent.child_session_id)?
            .is_some_and(|session| session.status.is_active());
        if child_is_active && agent.status != "closed" {
            active.push(agent.clone());
        }
        collect_active_descendant_agents(store, &agent.child_session_id, active)?;
    }
    Ok(())
}

fn close_direct_active_child_subtrees(store: &Store, session_id: &str) -> Result<()> {
    for agent in store.list_child_agents(session_id)? {
        let mut subtree_active = Vec::new();
        collect_active_descendant_agents(store, &agent.child_session_id, &mut subtree_active)?;
        let child_is_active = store
            .load_session(&agent.child_session_id)?
            .is_some_and(|session| session.status.is_active());
        if child_is_active || !subtree_active.is_empty() {
            store.close_child_agent(&agent.child_session_id, "parent finished task")?;
            store.append_event(
                session_id,
                "agent.cancelled",
                serde_json::json!({
                    "child_session_id": agent.child_session_id,
                    "status": "cancelled",
                    "payload": { "reason": "parent finished task" },
                }),
            )?;
        }
    }
    Ok(())
}

fn dispatch_python_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    worker: &mut PythonWorker,
    call: &ToolCall,
    timeout_seconds: u64,
) -> Result<ToolDispatchOutcome> {
    let code = call
        .arguments
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "python",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let mut stream_error = None;
    let response = worker.run_with_events_and_timeout(
        &session.id,
        &session.cwd,
        &session.artifact_root,
        code,
        Some(timeout_seconds as f64),
        |event| {
            if stream_error.is_none() {
                if let Err(err) = record_python_worker_event(store, &session.id, &event) {
                    stream_error = Some(err);
                }
            }
        },
    )?;
    if let Some(err) = stream_error {
        return Err(err);
    }
    let text_artifact = record_python_response_final_event(store, &session.id, &response)?;
    if response.ok {
        store.append_event(
            &session.id,
            "tool.finished",
            serde_json::json!({
                "name": "python",
                "tool_call_id": call.id,
            }),
        )?;
    } else {
        store.append_event(
            &session.id,
            "tool.failed",
            serde_json::json!({
                "name": "python",
                "tool_call_id": call.id,
                "error": response.error,
            }),
        )?;
    }
    let messages = vec![tool_content_message(
        store,
        session,
        call,
        "python",
        python_tool_message_content_value(&response, text_artifact.as_ref())?,
    )?];
    Ok(ToolDispatchOutcome {
        finished: false,
        messages,
    })
}

fn dispatch_spawn_agent_tool<P: ModelProvider>(
    store: &Store,
    provider: &P,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
    options: &AgentRunOptions,
) -> Result<ToolDispatchOutcome> {
    let message = call
        .arguments
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if message.is_empty() {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            "spawn_agent requires message",
        );
    }
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "spawn_agent",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let parent_events = store.events_for_session(&session.id)?;
    let fork_mode = call
        .arguments
        .get("fork_mode")
        .and_then(Value::as_str)
        .unwrap_or("summary");
    let fork_turns = call
        .arguments
        .get("fork_turns")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let inherited_context = inherited_context_for_spawn(&parent_events, fork_mode, fork_turns);
    let parent_agent_path = display_agent_path_for_session(store, &session.id)?;
    let agent_path = match spawn_agent_task_name(&call.arguments, &parent_agent_path) {
        Ok(agent_path) => agent_path,
        Err(error) => return dispatch_tool_validation_error(store, session, call, &error),
    };
    let agent_role = call
        .arguments
        .get("agent_type")
        .or_else(|| call.arguments.get("role"))
        .and_then(Value::as_str);
    let child = store.create_child_session(
        &session.id,
        &session.cwd,
        Some(agent_path.as_str()),
        call.arguments.get("nickname").and_then(Value::as_str),
        agent_role,
    )?;
    store.append_event(
        &child.id,
        "agent.context",
        serde_json::json!({
            "from_session_id": session.id,
            "fork_mode": fork_mode,
            "fork_turns": fork_turns,
            "agent_path": agent_path.clone(),
            "nickname": call.arguments.get("nickname").and_then(Value::as_str),
            "role": agent_role,
            "context": inherited_context,
        }),
    )?;
    store.append_event(
        &child.id,
        "session.input",
        serde_json::json!({ "text": message }),
    )?;
    store.append_event(
        &session.id,
        "agent.spawned",
        serde_json::json!({
            "child_session_id": child.id,
            "agent_path": agent_path.clone(),
            "nickname": call.arguments.get("nickname").and_then(Value::as_str),
            "role": agent_role,
            "fork_mode": fork_mode,
        }),
    )?;
    let async_status = if let Some(runner) = options.child_agent_runner.clone() {
        start_child_agent_background(store, &session.id, &child.id, runner)?;
        Some("running")
    } else {
        None
    };
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({
            "name": "spawn_agent",
            "tool_call_id": call.id,
        }),
    )?;
    let child_result = if let Some(status) = async_status {
        serde_json::json!({
            "child_session_id": child.id,
            "status": status,
            "result": Value::Null,
            "failure": Value::Null,
            "background": true,
        })
    } else {
        let run_error =
            run_existing_session_with_provider(store, provider, &child.id, options.clone())
                .err()
                .map(|error| format!("{error:#}"));
        update_parent_from_child_run(store, &session.id, &child.id, run_error)?
    };
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_json_message(
            store,
            session,
            call,
            "spawn_agent",
            serde_json::json!({
                "child_session_id": child.id,
                "task_name": agent_path,
                "status": child_result.get("status").cloned().unwrap_or(Value::Null),
                "result": child_result.get("result").cloned().unwrap_or(Value::Null),
                "failure": child_result.get("failure").cloned().unwrap_or(Value::Null),
            }),
        )?],
    })
}

fn update_parent_from_child_run(
    store: &Store,
    parent_id: &str,
    child_id: &str,
    run_error: Option<String>,
) -> Result<Value> {
    let child = store
        .load_session(child_id)?
        .with_context(|| format!("unknown child session id: {child_id}"))?;
    let child_events = store.events_for_session(child_id)?;
    let result = result_from_events(&child_events);
    let failure = failure_from_events(&child_events).or(run_error);
    let status = child.status.as_str().to_string();
    let event_type = match status.as_str() {
        "done" => "agent.completed",
        "failed" => "agent.failed",
        "cancelled" => "agent.cancelled",
        _ => "agent.updated",
    };
    let edge_status = match status.as_str() {
        "done" | "failed" | "cancelled" => status.as_str(),
        _ => "open",
    };
    store.set_child_agent_status(child_id, edge_status)?;
    let payload = serde_json::json!({
        "child_session_id": child_id,
        "status": status,
        "result": result,
        "failure": failure,
    });
    store.append_event(
        parent_id,
        event_type,
        serde_json::json!({
            "child_session_id": child_id,
            "status": status,
            "payload": payload,
        }),
    )?;
    Ok(payload)
}

fn start_child_agent_background(
    store: &Store,
    parent_id: &str,
    child_id: &str,
    runner: ChildAgentRunner,
) -> Result<()> {
    let state_dir = store.state_dir().to_path_buf();
    let notifier = store.notifier();
    let parent_id = parent_id.to_string();
    let child_id = child_id.to_string();
    thread::Builder::new()
        .name(format!("browser-use-child-agent-{child_id}"))
        .spawn(move || {
            let run_error = runner
                .run(ChildAgentRunRequest {
                    parent_session_id: parent_id.clone(),
                    child_session_id: child_id.clone(),
                })
                .err()
                .map(|error| format!("{error:#}"));
            match Store::open_with_optional_notifier(&state_dir, notifier) {
                Ok(store) => {
                    let _ = update_parent_from_child_run(&store, &parent_id, &child_id, run_error);
                }
                Err(error) => {
                    eprintln!("failed to open store for child agent completion: {error:#}");
                }
            }
        })
        .context("spawn child agent thread")?;
    Ok(())
}

fn inherited_context_for_spawn(
    parent_events: &[browser_use_protocol::EventRecord],
    fork_mode: &str,
    fork_turns: usize,
) -> Value {
    match fork_mode {
        "none" => serde_json::json!({ "mode": "none" }),
        "last_n" => {
            let mut context = sanitized_agent_context_from_events(parent_events);
            let mut recent_turns = parent_events
                .iter()
                .filter(|event| {
                    matches!(
                        event.event_type.as_str(),
                        "session.input" | "session.followup"
                    )
                })
                .rev()
                .take(fork_turns)
                .filter_map(|event| {
                    event
                        .payload
                        .get("text")
                        .and_then(Value::as_str)
                        .map(|text| {
                            serde_json::json!({
                                "type": event.event_type,
                                "text": text,
                            })
                        })
                })
                .collect::<Vec<_>>();
            recent_turns.reverse();
            context["recent_turns"] = Value::Array(recent_turns);
            context["mode"] = Value::String("last_n".to_string());
            context
        }
        "all" => {
            let mut context = sanitized_agent_context_from_events(parent_events);
            context["mode"] = Value::String("all_sanitized".to_string());
            context
        }
        _ => {
            let mut context = sanitized_agent_context_from_events(parent_events);
            context["mode"] = Value::String("summary".to_string());
            context
        }
    }
}

fn spawn_agent_task_name(arguments: &Value, parent_agent_path: &str) -> Result<String, String> {
    if let Some(task_name) = arguments.get("task_name").and_then(Value::as_str) {
        return canonical_agent_path_from_task_name(task_name, parent_agent_path);
    }
    if let Some(path) = arguments.get("path").and_then(Value::as_str) {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return Err("spawn_agent requires task_name".to_string());
        }
        return Ok(canonical_agent_path(trimmed));
    }
    Err("spawn_agent requires task_name".to_string())
}

fn canonical_agent_path_from_task_name(
    task_name: &str,
    parent_agent_path: &str,
) -> Result<String, String> {
    let task_name = task_name.trim();
    validate_agent_task_name(task_name)?;
    let parent_agent_path = canonical_agent_path(parent_agent_path);
    if parent_agent_path == "/root" {
        Ok(format!("/root/{task_name}"))
    } else {
        Ok(format!("{parent_agent_path}/{task_name}"))
    }
}

fn validate_agent_task_name(task_name: &str) -> Result<(), String> {
    if task_name.is_empty() {
        return Err("task_name must not be empty".to_string());
    }
    if matches!(task_name, "root" | "." | "..") {
        return Err(format!("task_name `{task_name}` is reserved"));
    }
    if task_name.contains('/') {
        return Err("task_name must not contain `/`".to_string());
    }
    if !task_name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Err(
            "task_name must use only lowercase letters, digits, and underscores".to_string(),
        );
    }
    Ok(())
}

fn canonical_agent_path(path: &str) -> String {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed == "root" {
        return "/root".to_string();
    }
    let segments = trimmed
        .split('/')
        .filter_map(|segment| {
            let normalized = segment
                .trim()
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                        ch.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .trim_matches('-')
                .to_string();
            (!normalized.is_empty()).then_some(normalized)
        })
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return "/root".to_string();
    }
    if segments.first().is_some_and(|segment| segment == "root") {
        format!("/{}", segments.join("/"))
    } else {
        format!("/root/{}", segments.join("/"))
    }
}

fn canonical_agent_reference(reference: &str, current_agent_path: &str) -> String {
    let trimmed = reference.trim();
    if trimmed.starts_with('/') || trimmed == "root" || trimmed.starts_with("root/") {
        return canonical_agent_path(trimmed);
    }
    let current_agent_path = canonical_agent_path(current_agent_path);
    if current_agent_path == "/root" {
        canonical_agent_path(trimmed)
    } else {
        let relative = canonical_agent_path(trimmed);
        let relative = relative.strip_prefix("/root/").unwrap_or(relative.as_str());
        format!("{current_agent_path}/{relative}")
    }
}

fn resolve_child_agent(
    store: &Store,
    parent_session_id: &str,
    reference: &str,
) -> Result<Option<AgentSummary>> {
    let agents = store.list_child_agents(parent_session_id)?;
    if let Some(agent) = agents
        .iter()
        .find(|agent| agent.child_session_id == reference)
        .cloned()
    {
        return Ok(Some(agent));
    }
    let parent_agent_path = display_agent_path_for_session(store, parent_session_id)?;
    let canonical = canonical_agent_reference(reference, &parent_agent_path);
    Ok(agents.into_iter().find(|agent| {
        agent.agent_path.as_deref() == Some(reference)
            || agent.agent_path.as_deref() == Some(canonical.as_str())
    }))
}

fn display_agent_path_for_session(store: &Store, session_id: &str) -> Result<String> {
    if let Some(path) = store.agent_path_for_session(session_id)? {
        if !path.trim().is_empty() {
            return Ok(path);
        }
    }
    let session = store
        .load_session(session_id)?
        .with_context(|| format!("unknown session id: {session_id}"))?;
    if session.parent_id.is_none() {
        Ok("/root".to_string())
    } else {
        Ok(session_id.to_string())
    }
}

fn dispatch_wait_agent_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let child_ref = call
        .arguments
        .get("target")
        .or_else(|| call.arguments.get("child_session_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|target| !target.is_empty());
    let watched_agents = if let Some(child_ref) = child_ref {
        let Some(child_summary) = resolve_child_agent(store, &session.id, child_ref)? else {
            return dispatch_tool_validation_error(
                store,
                session,
                call,
                "wait_agent can only inspect children of the current session",
            );
        };
        vec![child_summary]
    } else {
        store.list_child_agents(&session.id)?
    };
    let timeout_ms = call
        .arguments
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(300_000)
        .min(300_000);
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "wait_agent",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    store.append_event(
        &session.id,
        "agent.wait.started",
        serde_json::json!({
            "tool_call_id": call.id,
            "target": child_ref,
            "targets": watched_agents.iter().map(|agent| {
                serde_json::json!({
                    "child_session_id": agent.child_session_id,
                    "task_name": agent.agent_path,
                    "nickname": agent.agent_nickname,
                    "role": agent.agent_role,
                })
            }).collect::<Vec<_>>(),
            "timeout_ms": timeout_ms,
        }),
    )?;
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    let timed_out = loop {
        let active_count = count_active_watched_agents(store, &watched_agents)?;
        let wait_condition_met = if child_ref.is_some() {
            active_count == 0
        } else {
            active_count < watched_agents.len()
        };
        if timeout_ms == 0 || wait_condition_met || watched_agents.is_empty() {
            break false;
        }
        if started.elapsed() >= timeout {
            break true;
        }
        thread::sleep(Duration::from_millis(50).min(timeout.saturating_sub(started.elapsed())));
    };
    let waited_ms = started.elapsed().as_millis() as u64;
    let agents = watched_agents
        .iter()
        .map(|agent| agent_status_payload(store, agent))
        .collect::<Result<Vec<_>>>()?;
    let active_count = count_active_watched_agents(store, &watched_agents)?;
    store.append_event(
        &session.id,
        "agent.wait.finished",
        serde_json::json!({
            "tool_call_id": call.id,
            "target": child_ref,
            "timed_out": timed_out,
            "waited_ms": waited_ms,
            "active_count": active_count,
        }),
    )?;
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({
            "name": "wait_agent",
            "tool_call_id": call.id,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_json_message(
            store,
            session,
            call,
            "wait_agent",
            wait_agent_response_payload(child_ref, agents, timed_out, waited_ms),
        )?],
    })
}

fn count_active_watched_agents(store: &Store, agents: &[AgentSummary]) -> Result<usize> {
    let mut active = 0;
    for agent in agents {
        if store
            .load_session(&agent.child_session_id)?
            .is_some_and(|session| session.status.is_active())
        {
            active += 1;
        }
    }
    Ok(active)
}

fn wait_agent_response_payload(
    child_ref: Option<&str>,
    agents: Vec<Value>,
    timed_out: bool,
    waited_ms: u64,
) -> Value {
    let mut payload = serde_json::json!({
        "timed_out": timed_out,
        "waited_ms": waited_ms,
        "agents": agents,
    });
    if child_ref.is_some() {
        if let Some(agent) = payload
            .get("agents")
            .and_then(Value::as_array)
            .and_then(|agents| agents.first())
            .cloned()
        {
            payload["child_session_id"] = agent
                .get("child_session_id")
                .cloned()
                .unwrap_or(Value::Null);
            payload["task_name"] = agent.get("task_name").cloned().unwrap_or(Value::Null);
            payload["status"] = agent.get("status").cloned().unwrap_or(Value::Null);
            payload["result"] = agent.get("result").cloned().unwrap_or(Value::Null);
            payload["failure"] = agent.get("failure").cloned().unwrap_or(Value::Null);
            payload["messages"] = agent.get("messages").cloned().unwrap_or(Value::Null);
        }
    }
    payload
}

fn agent_status_payload(store: &Store, agent: &AgentSummary) -> Result<Value> {
    let child = store
        .load_session(&agent.child_session_id)?
        .with_context(|| format!("unknown child session id: {}", agent.child_session_id))?;
    let child_events = store.events_for_session(&agent.child_session_id)?;
    let mut messages = Vec::new();
    for message in store.messages_for_agent(&agent.child_session_id)? {
        messages.push(serde_json::json!({
            "id": message.id,
            "author_session_id": message.author_session_id,
            "target_session_id": message.target_session_id,
            "author_path": display_agent_path_for_session(store, &message.author_session_id)?,
            "recipient_path": display_agent_path_for_session(store, &message.target_session_id)?,
            "content": message.content,
            "trigger_turn": message.trigger_turn,
            "created_ms": message.created_ms,
        }));
    }
    Ok(serde_json::json!({
        "child_session_id": agent.child_session_id,
        "task_name": agent.agent_path,
        "nickname": agent.agent_nickname,
        "role": agent.agent_role,
        "edge_status": agent.status,
        "status": child.status.as_str(),
        "result": result_from_events(&child_events),
        "failure": failure_from_events(&child_events),
        "messages": messages,
        "updated_ms": child.updated_ms,
    }))
}

fn dispatch_agent_message_tool<P: ModelProvider>(
    store: &Store,
    provider: &P,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
    tool_name: &str,
    trigger_turn: bool,
    options: &AgentRunOptions,
) -> Result<ToolDispatchOutcome> {
    let Some(child_ref) = call
        .arguments
        .get("target")
        .or_else(|| call.arguments.get("child_session_id"))
        .and_then(Value::as_str)
    else {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            &format!("{tool_name} requires target"),
        );
    };
    let Some(child_summary) = resolve_child_agent(store, &session.id, child_ref)? else {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            &format!("{tool_name} can only message children of the current session"),
        );
    };
    let child_session_id = child_summary.child_session_id.clone();
    let agent_path = child_summary.agent_path.clone();
    let message = call
        .arguments
        .get("message")
        .or_else(|| call.arguments.get("input"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if message.is_empty() {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            &format!("{tool_name} requires message"),
        );
    }
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": tool_name,
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let child = store
        .load_session(&child_session_id)?
        .with_context(|| format!("unknown child session id: {child_session_id}"))?;
    debug_assert_eq!(child.parent_id.as_deref(), Some(session.id.as_str()));
    let child_was_active = child.status.is_active();
    let mail = store.send_agent_message(&session.id, &child_session_id, message, trigger_turn)?;
    let author_path = display_agent_path_for_session(store, &session.id)?;
    let recipient_path = match agent_path.clone() {
        Some(path) => path,
        None => display_agent_path_for_session(store, &child_session_id)?,
    };
    if trigger_turn {
        store.append_event(
            &child_session_id,
            "session.followup",
            serde_json::json!({ "text": message }),
        )?;
    }
    store.append_event(
        &session.id,
        "agent.message",
        serde_json::json!({
            "id": mail.id,
            "author_session_id": session.id,
            "target_session_id": child_session_id,
            "author_path": author_path.clone(),
            "recipient_path": recipient_path.clone(),
            "child_session_id": child_session_id,
            "content": message,
            "trigger_turn": trigger_turn,
        }),
    )?;
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({
            "name": tool_name,
            "tool_call_id": call.id,
        }),
    )?;
    let child_result = if trigger_turn {
        if let Some(runner) = options.child_agent_runner.clone() {
            if !child_was_active {
                start_child_agent_background(store, &session.id, &child_session_id, runner)?;
            }
            Some(serde_json::json!({
                "child_session_id": child_session_id,
                "status": "running",
                "result": Value::Null,
                "failure": Value::Null,
                "background": true,
            }))
        } else {
            let run_error = run_existing_session_with_provider(
                store,
                provider,
                &child_session_id,
                options.clone(),
            )
            .err()
            .map(|error| format!("{error:#}"));
            Some(update_parent_from_child_run(
                store,
                &session.id,
                &child_session_id,
                run_error,
            )?)
        }
    } else {
        None
    };
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_json_message(
            store,
            session,
            call,
            tool_name,
            serde_json::json!({
                "message_id": mail.id,
                "child_session_id": child_session_id,
                "task_name": agent_path,
                "author_path": author_path,
                "recipient_path": recipient_path,
                "trigger_turn": trigger_turn,
                "status": child_result
                    .as_ref()
                    .and_then(|result| result.get("status"))
                    .cloned()
                    .unwrap_or(Value::Null),
                "result": child_result
                    .as_ref()
                    .and_then(|result| result.get("result"))
                    .cloned()
                    .unwrap_or(Value::Null),
                "failure": child_result
                    .as_ref()
                    .and_then(|result| result.get("failure"))
                    .cloned()
                    .unwrap_or(Value::Null),
            }),
        )?],
    })
}

fn dispatch_list_agents_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "list_agents",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let path_prefix = call
        .arguments
        .get("path_prefix")
        .and_then(Value::as_str)
        .map(|prefix| {
            display_agent_path_for_session(store, &session.id)
                .map(|current| canonical_agent_reference(prefix, &current))
        })
        .transpose()?;
    let agents = store
        .list_child_agents(&session.id)?
        .into_iter()
        .filter(|agent| match path_prefix.as_deref() {
            Some(prefix) => agent
                .agent_path
                .as_deref()
                .is_some_and(|path| path == prefix || path.starts_with(&format!("{prefix}/"))),
            None => true,
        })
        .map(|agent| {
            serde_json::json!({
                "child_session_id": agent.child_session_id,
                "status": agent.status,
                "task_name": agent.agent_path,
                "nickname": agent.agent_nickname,
                "role": agent.agent_role,
                "updated_ms": agent.updated_ms,
            })
        })
        .collect::<Vec<_>>();
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({
            "name": "list_agents",
            "tool_call_id": call.id,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_json_message(
            store,
            session,
            call,
            "list_agents",
            serde_json::json!({ "agents": agents }),
        )?],
    })
}

fn dispatch_close_agent_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let Some(child_ref) = call
        .arguments
        .get("target")
        .or_else(|| call.arguments.get("child_session_id"))
        .and_then(Value::as_str)
    else {
        return dispatch_tool_validation_error(store, session, call, "close_agent requires target");
    };
    let Some(child_summary) = resolve_child_agent(store, &session.id, child_ref)? else {
        return dispatch_tool_validation_error(
            store,
            session,
            call,
            "close_agent can only close children of the current session",
        );
    };
    let child_session_id = child_summary.child_session_id.clone();
    let agent_path = child_summary.agent_path.clone();
    let reason = call
        .arguments
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("closed by parent agent");
    store.append_event(
        &session.id,
        "tool.started",
        serde_json::json!({
            "name": "close_agent",
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    let child = store
        .load_session(&child_session_id)?
        .with_context(|| format!("unknown child session id: {child_session_id}"))?;
    debug_assert_eq!(child.parent_id.as_deref(), Some(session.id.as_str()));
    store.close_child_agent(&child_session_id, reason)?;
    store.append_event(
        &session.id,
        "agent.cancelled",
        serde_json::json!({
            "child_session_id": child_session_id,
            "status": "cancelled",
            "payload": { "reason": reason },
        }),
    )?;
    store.append_event(
        &session.id,
        "tool.finished",
        serde_json::json!({
            "name": "close_agent",
            "tool_call_id": call.id,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_json_message(
            store,
            session,
            call,
            "close_agent",
            serde_json::json!({
                "child_session_id": child_session_id,
                "task_name": agent_path,
                "status": "cancelled",
            }),
        )?],
    })
}

fn dispatch_tool_validation_error(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
    error: &str,
) -> Result<ToolDispatchOutcome> {
    store.append_event(
        &session.id,
        "tool.failed",
        serde_json::json!({
            "name": call.name,
            "tool_call_id": call.id,
            "error": error,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_text_message(store, session, call, &call.name, error)?],
    })
}

fn dispatch_unknown_tool(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
) -> Result<ToolDispatchOutcome> {
    let error = format!("unknown tool: {}", call.name);
    store.append_event(
        &session.id,
        "tool.failed",
        serde_json::json!({
            "name": call.name,
            "tool_call_id": call.id,
            "error": error,
        }),
    )?;
    Ok(ToolDispatchOutcome {
        finished: false,
        messages: vec![tool_text_message(store, session, call, &call.name, &error)?],
    })
}

fn tool_json_message(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
    name: &str,
    content: Value,
) -> Result<Value> {
    let content = serde_json::to_string(&content)?;
    tool_text_message(store, session, call, name, &content)
}

fn tool_text_message(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
    name: &str,
    content: &str,
) -> Result<Value> {
    let content = spill_large_tool_text(store, &session.id, Some(&call.id), name, content)?;
    Ok(serde_json::json!({
        "role": "tool",
        "tool_call_id": call.id,
        "name": name,
        "content": content,
    }))
}

fn tool_content_message(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
    name: &str,
    content: Value,
) -> Result<Value> {
    let content = spill_large_tool_content(store, &session.id, Some(&call.id), name, content)?;
    Ok(serde_json::json!({
        "role": "tool",
        "tool_call_id": call.id,
        "name": name,
        "content": content,
    }))
}

fn spill_large_tool_content(
    store: &Store,
    session_id: &str,
    call_id: Option<&str>,
    tool_name: &str,
    content: Value,
) -> Result<Value> {
    match content {
        Value::String(text) => Ok(Value::String(spill_large_tool_text(
            store, session_id, call_id, tool_name, &text,
        )?)),
        Value::Array(items) => {
            spill_large_tool_content_items(store, session_id, call_id, tool_name, items)
        }
        other => {
            let serialized = serde_json::to_string(&other)?;
            if approx_token_count(&serialized) <= MAX_TOOL_OUTPUT_TEXT_TOKENS {
                Ok(other)
            } else {
                Ok(Value::String(spill_large_tool_text(
                    store,
                    session_id,
                    call_id,
                    tool_name,
                    &serialized,
                )?))
            }
        }
    }
}

fn spill_large_tool_content_items(
    store: &Store,
    session_id: &str,
    call_id: Option<&str>,
    tool_name: &str,
    items: Vec<Value>,
) -> Result<Value> {
    let mut text_segments = Vec::new();
    let mut non_text_items = Vec::new();
    for item in &items {
        if let Some(text) = item
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| item.as_str())
        {
            text_segments.push(text.to_string());
        } else {
            non_text_items.push(item.clone());
        }
    }
    let combined_text = text_segments.join("\n");
    if approx_token_count(&combined_text) <= MAX_TOOL_OUTPUT_TEXT_TOKENS {
        return Ok(Value::Array(items));
    }

    let preview = spill_large_tool_text(store, session_id, call_id, tool_name, &combined_text)?;
    let mut compacted = vec![serde_json::json!({
        "type": "output_text",
        "text": preview,
    })];
    compacted.extend(non_text_items);
    Ok(Value::Array(compacted))
}

fn spill_large_tool_text(
    store: &Store,
    session_id: &str,
    call_id: Option<&str>,
    tool_name: &str,
    text: &str,
) -> Result<String> {
    if approx_token_count(text) <= MAX_TOOL_OUTPUT_TEXT_TOKENS {
        return Ok(text.to_string());
    }
    let artifact = write_tool_output_artifact(store, session_id, tool_name, call_id, text)?;
    Ok(spilled_tool_output_preview(text, &artifact))
}

fn spilled_tool_output_preview(text: &str, artifact: &Value) -> String {
    let path = artifact
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let footer = format!("\n\nFull tool output saved to: {path}");
    let footer_tokens = approx_token_count(&footer);
    let preview_budget = MAX_TOOL_OUTPUT_TEXT_TOKENS
        .saturating_sub(footer_tokens)
        .max(32);
    format!(
        "{}{footer}",
        truncate_for_context_tokens(text, preview_budget)
    )
}

fn write_tool_output_artifact(
    store: &Store,
    session_id: &str,
    tool_name: &str,
    call_id: Option<&str>,
    text: &str,
) -> Result<Value> {
    let session = store
        .load_session(session_id)?
        .with_context(|| format!("unknown session id: {session_id}"))?;
    let output_dir = Path::new(&session.artifact_root).join("tool-output");
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("create {}", output_dir.display()))?;
    let tool_component = sanitize_artifact_filename_component(tool_name);
    let call_component = call_id
        .map(sanitize_artifact_filename_component)
        .filter(|component| !component.is_empty());
    let unique = TOOL_OUTPUT_ARTIFACT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let filename = match call_component {
        Some(call_component) => format!(
            "{tool_component}-{call_component}-{}-{unique}.txt",
            now_ms()
        ),
        None => format!("{tool_component}-output-{}-{unique}.txt", now_ms()),
    };
    let path = output_dir.join(filename);
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    let artifact = serde_json::json!({
        "kind": "tool-output",
        "path": path.display().to_string(),
        "mime": "text/plain",
        "bytes": std::fs::metadata(&path).ok().and_then(|metadata| i64::try_from(metadata.len()).ok()),
        "original_chars": text.chars().count(),
        "original_tokens_estimate": approx_token_count(text),
        "truncated_tokens": MAX_TOOL_OUTPUT_TEXT_TOKENS,
        "tool_name": tool_name,
        "tool_call_id": call_id,
    });
    let event = store.append_event(
        session_id,
        "tool.output_spilled",
        serde_json::json!({
            "name": tool_name,
            "tool_call_id": call_id,
            "artifact": artifact,
        }),
    )?;
    store.record_artifact(
        session_id,
        Some(event.seq),
        "tool-output",
        &path,
        Some("text/plain"),
        artifact.clone(),
    )?;
    Ok(artifact)
}

fn sanitize_artifact_filename_component(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars().take(80) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "tool".to_string()
    } else {
        out
    }
}

fn python_tool_message_content(
    response: &RunPythonResponse,
    text_artifact: Option<&Value>,
) -> String {
    if response.ok {
        let mut parts = Vec::new();
        if !response.text.trim().is_empty() {
            let text = response.text.trim();
            let text = if approx_token_count(text) > MAX_TOOL_OUTPUT_TEXT_TOKENS {
                text_artifact
                    .map(|artifact| spilled_tool_output_preview(text, artifact))
                    .unwrap_or_else(|| text.to_string())
            } else {
                text.to_string()
            };
            parts.push(text);
        }
        if !response.data.is_null() {
            parts.push(format!("data: {}", response.data));
        }
        if parts.is_empty() {
            "python tool completed".to_string()
        } else {
            parts.join("\n")
        }
    } else {
        format!(
            "python tool failed: {}",
            response
                .error
                .as_deref()
                .unwrap_or("unknown python worker error")
        )
    }
}

fn python_tool_message_content_value(
    response: &RunPythonResponse,
    text_artifact: Option<&Value>,
) -> Result<Value> {
    let text = python_tool_message_content(response, text_artifact);
    let Some(image_parts) = python_tool_image_output_parts(response)? else {
        return Ok(Value::String(text));
    };
    let mut parts = vec![serde_json::json!({
        "type": "output_text",
        "text": text,
    })];
    parts.extend(image_parts);
    Ok(Value::Array(parts))
}

fn python_tool_image_output_parts(response: &RunPythonResponse) -> Result<Option<Vec<Value>>> {
    if !response.ok || response.images.is_empty() {
        return Ok(None);
    }
    let mut parts = Vec::new();
    for image in &response.images {
        let Some(path) = image.get("path").and_then(Value::as_str) else {
            continue;
        };
        let bytes = std::fs::read(path).with_context(|| format!("read image artifact {path}"))?;
        let mime_type = image
            .get("mime_type")
            .and_then(Value::as_str)
            .or_else(|| image.get("mime").and_then(Value::as_str))
            .unwrap_or("image/png");
        parts.push(serde_json::json!({
            "type": "input_image",
            "image_url": format!("data:{mime_type};base64,{}", general_purpose::STANDARD.encode(bytes)),
            "detail": image
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("auto"),
        }));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(parts))
}

fn browser_script_tool_message_content(
    response: &browser_use_browser::BrowserScriptOutput,
) -> String {
    if response.ok {
        let mut parts = Vec::new();
        if !response.text.trim().is_empty() {
            parts.push(response.text.trim().to_string());
        }
        if !response.data.is_null() && response.data != serde_json::json!({}) {
            parts.push(format!("data: {}", response.data));
        }
        if parts.is_empty() {
            "browser_script completed".to_string()
        } else {
            parts.join("\n")
        }
    } else {
        format!(
            "browser_script failed: {}",
            response
                .error
                .as_deref()
                .unwrap_or("unknown browser_script error")
        )
    }
}

fn browser_script_tool_message_content_value(
    response: &browser_use_browser::BrowserScriptOutput,
) -> Result<Value> {
    let text = browser_script_tool_message_content(response);
    let Some(image_parts) = browser_script_image_output_parts(response)? else {
        return Ok(Value::String(text));
    };
    let mut parts = vec![serde_json::json!({
        "type": "output_text",
        "text": text,
    })];
    parts.extend(image_parts);
    Ok(Value::Array(parts))
}

fn browser_script_image_output_parts(
    response: &browser_use_browser::BrowserScriptOutput,
) -> Result<Option<Vec<Value>>> {
    if !response.ok || response.images.is_empty() {
        return Ok(None);
    }
    let mut parts = Vec::new();
    for image in &response.images {
        let Some(path) = image.get("path").and_then(Value::as_str) else {
            continue;
        };
        let bytes = std::fs::read(path).with_context(|| format!("read image artifact {path}"))?;
        let mime_type = image
            .get("mime_type")
            .and_then(Value::as_str)
            .or_else(|| image.get("mime").and_then(Value::as_str))
            .unwrap_or("image/png");
        parts.push(serde_json::json!({
            "type": "input_image",
            "image_url": format!("data:{mime_type};base64,{}", general_purpose::STANDARD.encode(bytes)),
            "detail": image
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("auto"),
        }));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(parts))
}

pub fn record_python_response_events(
    store: &Store,
    session_id: &str,
    response: &RunPythonResponse,
) -> Result<Option<Value>> {
    record_python_response_events_inner(store, session_id, response, true)
}

pub fn record_python_response_final_event(
    store: &Store,
    session_id: &str,
    response: &RunPythonResponse,
) -> Result<Option<Value>> {
    record_python_response_events_inner(store, session_id, response, false)
}

fn record_browser_script_response_events(
    store: &Store,
    session_id: &str,
    response: &browser_use_browser::BrowserScriptOutput,
) -> Result<()> {
    for browser_event in &response.browser_events {
        record_python_browser_event(store, session_id, browser_event)?;
    }
    let image_paths = response
        .images
        .iter()
        .filter_map(|image| image.get("path").and_then(Value::as_str))
        .collect::<std::collections::HashSet<_>>();
    for image in &response.images {
        record_tool_image(store, session_id, "browser_script", image)?;
    }
    for artifact in &response.artifacts {
        let Some(path) = artifact.get("path").and_then(Value::as_str) else {
            continue;
        };
        if image_paths.contains(path) {
            continue;
        }
        record_tool_artifact(store, session_id, "browser_script", artifact)?;
    }
    store.append_event(
        session_id,
        "tool.output",
        serde_json::json!({
            "name": "browser_script",
            "ok": response.ok,
            "text": response.text,
            "data": response.data,
            "images": response.images,
            "artifacts": response.artifacts,
            "error": response.error,
        }),
    )?;
    Ok(())
}

pub fn record_python_worker_event(
    store: &Store,
    session_id: &str,
    event: &PythonWorkerEvent,
) -> Result<()> {
    match event.event.as_str() {
        "output" => record_python_output(store, session_id, &event.payload),
        "browser" => record_python_browser_event(store, session_id, &event.payload),
        "image" => record_python_image(store, session_id, &event.payload),
        "artifact" => record_python_artifact(store, session_id, &event.payload),
        _ => Ok(()),
    }
}

fn record_python_response_events_inner(
    store: &Store,
    session_id: &str,
    response: &RunPythonResponse,
    include_host_records: bool,
) -> Result<Option<Value>> {
    if include_host_records {
        for output in &response.outputs {
            record_python_output(store, session_id, output)?;
        }

        for browser_event in &response.browser_events {
            record_python_browser_event(store, session_id, browser_event)?;
        }

        let image_paths = response
            .images
            .iter()
            .filter_map(|image| image.get("path").and_then(Value::as_str))
            .collect::<std::collections::HashSet<_>>();
        for image in &response.images {
            record_python_image(store, session_id, image)?;
        }

        for artifact in &response.artifacts {
            let Some(path) = artifact.get("path").and_then(Value::as_str) else {
                continue;
            };
            if image_paths.contains(path) {
                continue;
            }
            record_python_artifact(store, session_id, artifact)?;
        }
    }

    let (text, text_artifact) = spill_large_text_output(store, session_id, &response.text)?;
    let mut payload = serde_json::json!({
        "name": "python",
        "ok": response.ok,
        "text": text,
        "data": response.data,
        "images": response.images,
        "artifacts": response.artifacts,
        "browser_harness_available": response.browser_harness_available,
        "browser_harness_error": response.browser_harness_error,
    });
    if let Some(artifact) = text_artifact.as_ref() {
        payload["text_truncated"] = Value::Bool(true);
        payload["text_artifact"] = artifact.clone();
    }
    store.append_event(session_id, "tool.output", payload)?;
    Ok(text_artifact)
}

fn spill_large_text_output(
    store: &Store,
    session_id: &str,
    text: &str,
) -> Result<(String, Option<Value>)> {
    if approx_token_count(text) <= MAX_TOOL_OUTPUT_TEXT_TOKENS {
        return Ok((text.to_string(), None));
    }
    let artifact = write_tool_output_artifact(store, session_id, "python", None, text)?;
    Ok((
        truncate_for_context_tokens(text, MAX_TOOL_OUTPUT_TEXT_TOKENS),
        Some(artifact),
    ))
}

fn record_python_output(store: &Store, session_id: &str, output: &Value) -> Result<()> {
    store.append_event(
        session_id,
        "tool.output",
        serde_json::json!({
            "name": "python",
            "stream": true,
            "text": output.get("text").and_then(Value::as_str).unwrap_or_default(),
        }),
    )?;
    Ok(())
}

fn record_python_browser_event(
    store: &Store,
    session_id: &str,
    browser_event: &Value,
) -> Result<()> {
    if let Some(event_type) = browser_event.get("type").and_then(Value::as_str) {
        let payload = browser_event
            .get("payload")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        store.append_event(session_id, event_type, payload)?;
    }
    Ok(())
}

fn record_python_image(store: &Store, session_id: &str, image: &Value) -> Result<()> {
    record_tool_image(store, session_id, "python", image)
}

fn record_tool_image(store: &Store, session_id: &str, name: &str, image: &Value) -> Result<()> {
    let event = store.append_event(
        session_id,
        "tool.image",
        serde_json::json!({
            "name": name,
            "image": image,
        }),
    )?;
    if let Some(path) = image.get("path").and_then(Value::as_str) {
        store.record_artifact(
            session_id,
            Some(event.seq),
            "image",
            path,
            image.get("mime_type").and_then(Value::as_str),
            image.clone(),
        )?;
    }
    Ok(())
}

fn record_python_artifact(store: &Store, session_id: &str, artifact: &Value) -> Result<()> {
    record_tool_artifact(store, session_id, "python", artifact)
}

fn record_tool_artifact(
    store: &Store,
    session_id: &str,
    name: &str,
    artifact: &Value,
) -> Result<()> {
    let Some(path) = artifact.get("path").and_then(Value::as_str) else {
        return Ok(());
    };
    let kind = artifact
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("file");
    let event = store.append_event(
        session_id,
        "artifact.created",
        serde_json::json!({
            "name": name,
            "artifact": artifact,
        }),
    )?;
    store.record_artifact(
        session_id,
        Some(event.seq),
        kind,
        path,
        artifact.get("mime").and_then(Value::as_str),
        artifact.clone(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_preference_commands_persist_mode_and_domain_profile() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, temp.path())?;

        let output =
            dispatch_browser_preference_command(&store, &session, "browser preference use cloud")?
                .expect("preference command");
        assert_eq!(output["status"], "ok");
        assert_eq!(
            store.get_setting(BROWSER_PREF_MODE)?.as_deref(),
            Some("cloud")
        );
        assert_eq!(
            store.get_setting("browser")?.as_deref(),
            Some("Browser Use cloud")
        );
        assert_eq!(
            resolve_browser_connect_command(&store, "browser connect")?,
            "browser remote start"
        );

        let output = dispatch_browser_preference_command(
            &store,
            &session,
            "browser profile remember --domain https://www.gusto.com/ --profile google-chrome:Profile-2 --mode local",
        )?
        .expect("profile command");
        assert_eq!(output["status"], "ok");
        assert_eq!(output["remembered"]["domain"], "gusto.com");
        assert_eq!(
            remembered_domain_profile(&store, "gusto.com")?
                .and_then(|value| value["profile_id"].as_str().map(ToOwned::to_owned))
                .as_deref(),
            Some("google-chrome:Profile-2")
        );
        assert_eq!(
            resolve_browser_connect_command(&store, "connect")?,
            "browser connect local"
        );
        Ok(())
    }

    #[test]
    fn fake_agent_can_run_python_and_finish() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session_id = run_fake_agent(
            &store,
            "check python",
            temp.path(),
            FakeAgentOptions {
                python_code: Some("print('hello from python')\nresult = {'ok': True}"),
            },
        )?;
        let session = store.load_session(&session_id)?.expect("session");
        assert_eq!(session.status.as_str(), "done");
        let events = store.events_for_session(&session_id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "tool.started"));
        assert!(events.iter().any(
            |event| event.event_type == "model.tool_call" && event.payload["name"] == "python"
        ));
        assert!(events.iter().any(|event| event.event_type == "tool.output"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "model.tool_call" && event.payload["name"] == "done"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.done"));
        Ok(())
    }

    #[test]
    fn provider_config_runs_existing_session_without_ui_provider_construction() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, temp.path())?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "run from config"}),
        )?;
        run_existing_session_from_config(
            &store,
            &session.id,
            ProviderRunConfig::new(ProviderBackend::Fake, "fake")
                .with_fake_result("configured fake result"),
        )?;
        let events = store.events_for_session(&session.id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "session.done"
                && event.payload["result"] == "configured fake result"
        }));
        Ok(())
    }

    #[test]
    fn provider_messages_replay_assistant_tool_outputs_and_images() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let image_path = temp.path().join("shot.png");
        std::fs::write(&image_path, b"fake image bytes")?;
        let store = Store::open(temp.path().join("state"))?;
        let session = store.create_session(None, temp.path())?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect"}),
        )?;
        store.append_event(
            &session.id,
            "model.delta",
            serde_json::json!({"text": "Looking."}),
        )?;
        store.append_event(
            &session.id,
            "model.tool_call",
            serde_json::json!({
                "id": "call_py",
                "name": "python",
                "arguments": {"code": "screenshot()"},
            }),
        )?;
        store.append_event(
            &session.id,
            "tool.output",
            serde_json::json!({
                "name": "python",
                "tool_call_id": "call_py",
                "ok": true,
                "text": "saw page",
                "data": null,
                "images": [{
                    "path": image_path.display().to_string(),
                    "mime_type": "image/png",
                    "detail": "auto",
                }],
            }),
        )?;
        store.append_event(
            &session.id,
            "model.tool_call",
            serde_json::json!({
                "id": "call_plan",
                "name": "update_plan",
                "arguments": {"plan": [{"step": "done", "status": "completed"}]},
            }),
        )?;
        store.append_event(
            &session.id,
            "tool.finished",
            serde_json::json!({
                "name": "update_plan",
                "tool_call_id": "call_plan",
            }),
        )?;

        let messages = provider_messages_from_events(&store.events_for_session(&session.id)?);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_py");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_py");
        assert!(message_has_input_image(&messages[2]));
        assert_eq!(messages[3]["role"], "assistant");
        assert_eq!(messages[3]["tool_calls"][0]["id"], "call_plan");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[4]["content"], "plan updated");
        Ok(())
    }

    #[test]
    fn provider_done_tool_finishes_session() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![vec![
            ModelEvent::ToolCall {
                call: ToolCall {
                    id: "done_1".to_string(),
                    name: "done".to_string(),
                    arguments: serde_json::json!({"result": "final answer"}),
                },
            },
            ModelEvent::Done,
        ]]);
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "finish directly",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events.iter().any(|event| event.event_type == "model.config"
            && event.payload["provider"] == "scripted"
            && event.payload["model"] == "scripted"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "tool.finished" && event.payload["name"] == "done"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.done"
                && event.payload["result"] == "final answer"));
        let runs = store.runs_for_session(&session_id)?;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "done");
        assert!(runs[0].ended_ms.is_some());
        Ok(())
    }

    #[test]
    fn provider_loop_ignores_repeated_full_text_delta() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![vec![
            ModelEvent::TextDelta {
                text: "Your callback has been scheduled.".to_string(),
            },
            ModelEvent::TextDelta {
                text: "Your callback has been scheduled.".to_string(),
            },
            ModelEvent::Done,
        ]]);
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "finish with text",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let events = store.events_for_session(&session_id)?;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "model.delta")
                .count(),
            1
        );
        assert!(events.iter().any(|event| {
            event.event_type == "session.done"
                && event.payload["result"] == "Your callback has been scheduled."
        }));
        Ok(())
    }

    #[test]
    fn provider_loop_records_live_stream_deltas() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![vec![
            ModelEvent::TextDelta {
                text: "live ".to_string(),
            },
            ModelEvent::TextDelta {
                text: "live answer".to_string(),
            },
            ModelEvent::Done,
        ]]);

        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "finish with streaming text",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let events = store.events_for_session(&session_id)?;
        let stream_text = events
            .iter()
            .filter(|event| event.event_type == "model.stream_delta")
            .filter_map(|event| event.payload.get("text").and_then(Value::as_str))
            .collect::<String>();
        assert_eq!(stream_text, "live answer");
        Ok(())
    }

    #[test]
    fn provider_loop_records_live_thinking_deltas() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![vec![
            ModelEvent::ThinkingDelta {
                text: "checking ".to_string(),
                label: Some("reasoning summary".to_string()),
            },
            ModelEvent::ThinkingDelta {
                text: "checking context".to_string(),
                label: Some("reasoning summary".to_string()),
            },
            ModelEvent::TextDelta {
                text: "final answer".to_string(),
            },
            ModelEvent::Done,
        ]]);

        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "finish with thinking text",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let events = store.events_for_session(&session_id)?;
        let thinking_text = events
            .iter()
            .filter(|event| event.event_type == "model.thinking_delta")
            .filter_map(|event| event.payload.get("text").and_then(Value::as_str))
            .collect::<String>();
        assert_eq!(thinking_text, "checking context");
        assert!(events.iter().any(|event| {
            event.event_type == "model.thinking_delta"
                && event.payload["label"] == "reasoning summary"
        }));
        Ok(())
    }

    #[test]
    fn provider_loop_respects_external_cancel_before_finalizing() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, temp.path())?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "cancel me"}),
        )?;
        let provider = CancellingProvider {
            state_dir: temp.path().to_path_buf(),
            session_id: session.id.clone(),
        };

        let result = run_existing_session_with_provider(
            &store,
            &provider,
            &session.id,
            AgentRunOptions::default(),
        );

        assert!(result.is_err());
        let session = store.load_session(&session.id)?.context("session")?;
        assert_eq!(session.status, SessionStatus::Cancelled);
        let events = store.events_for_session(&session.id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.cancelled"));
        assert!(!events
            .iter()
            .any(|event| event.event_type == "session.done"));
        let runs = store.runs_for_session(&session.id)?;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "cancelled");
        Ok(())
    }

    struct CancellingProvider {
        state_dir: std::path::PathBuf,
        session_id: String,
    }

    impl ModelProvider for CancellingProvider {
        fn provider_name(&self) -> &'static str {
            "cancelling"
        }

        fn model_name(&self) -> &str {
            "cancelling"
        }

        fn start_turn(&self, _turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
            Store::open(&self.state_dir)?.request_cancel(&self.session_id, "test cancel")?;
            Ok(vec![
                ModelEvent::TextDelta {
                    text: "this should not become the final answer".to_string(),
                },
                ModelEvent::Done,
            ])
        }
    }

    #[test]
    fn provider_stream_errors_mark_session_failed_and_finish_run() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = FailingProvider;

        let result = run_agent_with_provider(
            &store,
            &provider,
            "provider should fail",
            temp.path(),
            AgentRunOptions::default(),
        );

        assert!(result.is_err());
        let session = store.list_sessions()?.remove(0);
        assert_eq!(session.status, SessionStatus::Failed);
        let events = store.events_for_session(&session.id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "session.failed"
                && event.payload["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("provider stream exploded"))
        }));
        let runs = store.runs_for_session(&session.id)?;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "failed");
        assert!(runs[0].ended_ms.is_some());
        Ok(())
    }

    #[test]
    fn codex_stream_disconnects_are_retried_with_five_attempt_budget() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = FlakyProvider::default();

        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "retry provider",
            temp.path(),
            AgentRunOptions::default(),
        )?;

        let events = store.events_for_session(&session_id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "model.turn.retry"
                && event.payload["message"] == "Reconnecting... 1/5"
                && event.payload["provider"] == "codex"
        }));
        assert!(events.iter().any(|event| {
            event.event_type == "session.done" && event.payload["result"] == "retry recovered"
        }));
        Ok(())
    }

    struct FailingProvider;

    impl ModelProvider for FailingProvider {
        fn provider_name(&self) -> &'static str {
            "failing"
        }

        fn model_name(&self) -> &str {
            "failing"
        }

        fn start_turn(&self, _turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
            anyhow::bail!("provider stream exploded")
        }
    }

    #[derive(Default)]
    struct FlakyProvider {
        attempts: std::sync::Mutex<usize>,
    }

    impl ModelProvider for FlakyProvider {
        fn provider_name(&self) -> &'static str {
            "codex"
        }

        fn model_name(&self) -> &str {
            "flaky"
        }

        fn start_turn(&self, _turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
            let mut attempts = self.attempts.lock().expect("attempt lock");
            *attempts += 1;
            if *attempts == 1 {
                anyhow::bail!("read Codex SSE line\n\nCaused by:\n    operation timed out");
            }
            Ok(vec![
                ModelEvent::TextDelta {
                    text: "retry recovered".to_string(),
                },
                ModelEvent::Done,
            ])
        }
    }

    #[test]
    fn provider_loop_records_python_tool_timeout_and_continues() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "python_timeout".to_string(),
                        name: "python".to_string(),
                        arguments: serde_json::json!({
                            "code": "import time\ntime.sleep(5)",
                        }),
                    },
                },
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_after_timeout".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({"result": "timeout recovered"}),
                    },
                },
                ModelEvent::Done,
            ],
        ]);
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "timeout then recover",
            temp.path(),
            AgentRunOptions::default().with_python_tool_timeout_seconds(1),
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "tool.failed"
                && event.payload["name"] == "python"
                && event.payload["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("timed out"))
        }));
        assert!(events.iter().any(|event| {
            event.event_type == "session.done" && event.payload["result"] == "timeout recovered"
        }));
        Ok(())
    }

    #[test]
    fn provider_can_use_exec_command_tool() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "exec_1".to_string(),
                        name: "exec_command".to_string(),
                        arguments: serde_json::json!({
                            "cmd": "printf codex-tool",
                            "yield_time_ms": 5000,
                        }),
                    },
                },
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_after_exec".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({"result": "command complete"}),
                    },
                },
                ModelEvent::Done,
            ],
        ]);
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "run a command",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "command.started"));
        assert!(events.iter().any(|event| {
            event.event_type == "command.output" && event.payload["text"] == "codex-tool"
        }));
        assert!(events
            .iter()
            .any(|event| event.event_type == "command.finished"));
        assert!(events.iter().any(|event| {
            event.event_type == "session.done" && event.payload["result"] == "command complete"
        }));
        Ok(())
    }

    #[test]
    fn provider_can_use_file_tools() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let patch = r#"*** Begin Patch
*** Add File: note.txt
+alpha
+bravo
*** End Patch"#;
        let provider = ScriptedProvider::new(vec![
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "patch_1".to_string(),
                        name: "apply_patch".to_string(),
                        arguments: serde_json::json!({ "patch": patch }),
                    },
                },
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "read_1".to_string(),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({ "path": "note.txt" }),
                    },
                },
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "search_1".to_string(),
                        name: "search_files".to_string(),
                        arguments: serde_json::json!({ "query": "bravo", "path": "." }),
                    },
                },
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "list_1".to_string(),
                        name: "list_files".to_string(),
                        arguments: serde_json::json!({ "query": "note" }),
                    },
                },
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_after_files".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({"result": "files complete"}),
                    },
                },
                ModelEvent::Done,
            ],
        ]);
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "use file tools",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "patch.file_changed" && event.payload["kind"] == "added"
        }));
        assert!(events.iter().any(|event| event.event_type == "file.read"));
        assert!(events.iter().any(|event| event.event_type == "file.search"));
        assert!(events.iter().any(|event| event.event_type == "file.list"));
        assert!(temp.path().join("note.txt").exists());
        Ok(())
    }

    #[test]
    fn provider_loop_recovers_from_tool_errors() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "missing_read".to_string(),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({ "path": "missing.txt" }),
                    },
                },
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_after_missing".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({"result": "recovered"}),
                    },
                },
                ModelEvent::Done,
            ],
        ]);
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "recover from a missing file",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "tool.failed"
                && event.payload["tool_call_id"] == "missing_read"
                && event.payload["recovered"] == true
        }));
        assert!(events.iter().any(|event| {
            event.event_type == "session.done" && event.payload["result"] == "recovered"
        }));
        assert!(!events
            .iter()
            .any(|event| event.event_type == "session.failed"));
        Ok(())
    }

    #[test]
    fn parallel_tool_batch_preserves_model_message_order() -> Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::write(temp.path().join("first.txt"), "first")?;
        std::fs::write(temp.path().join("second.txt"), "second")?;
        let store = Store::open(temp.path().join("state"))?;
        let provider = ParallelReadOrderProvider::default();
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "read both files",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "tool.batch_started"
                && event.payload["tool_call_ids"][0] == "read_second"
                && event.payload["tool_call_ids"][1] == "read_first"
        }));
        assert!(events.iter().any(|event| {
            event.event_type == "session.done" && event.payload["result"] == "parallel reads ok"
        }));
        Ok(())
    }

    #[test]
    fn provider_can_update_plan() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "plan_1".to_string(),
                        name: "update_plan".to_string(),
                        arguments: serde_json::json!({
                            "explanation": "starting",
                            "plan": [
                                { "step": "Inspect", "status": "completed" },
                                { "step": "Patch", "status": "in_progress" },
                                { "step": "Verify", "status": "pending" }
                            ],
                        }),
                    },
                },
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_after_plan".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({"result": "planned"}),
                    },
                },
                ModelEvent::Done,
            ],
        ]);
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "make a plan",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "plan.updated"
                && event.payload["plan"][1]["status"] == "in_progress"
        }));
        Ok(())
    }

    #[test]
    fn provider_messages_are_compacted_when_context_gets_large() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![
            vec![
                ModelEvent::TextDelta {
                    text: "x".repeat(900),
                },
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "python_small".to_string(),
                        name: "python".to_string(),
                        arguments: serde_json::json!({"code": "result = {'ok': True}"}),
                    },
                },
                ModelEvent::Done,
            ],
            vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_after_compact".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({"result": "compacted ok"}),
                    },
                },
                ModelEvent::Done,
            ],
        ]);
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "compact this",
            temp.path(),
            AgentRunOptions {
                max_turns: 4,
                max_context_chars: 500,
                browser_mode: None,
                python_tool_timeout_seconds: 120,
                python_env: Vec::new(),
                child_agent_runner: None,
            },
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.compaction_started"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.compacted"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.done"
                && event.payload["result"] == "compacted ok"));
        Ok(())
    }

    #[test]
    fn provider_context_overflow_forces_compaction_and_retries_once() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ContextOverflowRecoveringProvider::default();
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            &format!("compact after provider overflow {}", "x".repeat(2048)),
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "model.turn.context_overflow"));
        assert!(events.iter().any(|event| {
            event.event_type == "session.compaction_started"
                && event.payload["reason"] == "provider_context_overflow"
        }));
        assert!(events.iter().any(|event| {
            event.event_type == "session.done" && event.payload["result"] == "overflow recovered"
        }));
        Ok(())
    }

    #[test]
    fn compaction_keeps_only_pending_tool_call_after_summary() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, temp.path())?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({ "text": "compact with pending call" }),
        )?;
        let mut messages = vec![
            serde_json::json!({
                "role": "user",
                "content": "x".repeat(2000),
            }),
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "old_call",
                    "name": "python",
                    "arguments": { "code": "print('old')" },
                }],
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "old_call",
                "content": "old output",
            }),
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "pending_call",
                    "name": "python",
                    "arguments": { "code": "print('pending')" },
                }],
            }),
        ];

        maybe_compact_messages(&store, &session.id, &mut messages, 500)?;

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["tool_calls"][0]["id"], "pending_call");
        assert!(!messages
            .iter()
            .any(|message| message.get("role").and_then(Value::as_str) == Some("tool")));
        Ok(())
    }

    #[test]
    fn compacted_context_keeps_browser_agent_contract() -> Result<()> {
        let message = compacted_context_system_message(&serde_json::json!({
            "task": "look at a browser page",
            "browser": {
                "url": "https://example.com",
                "status": "connected",
            },
        }))?;

        assert!(
            message.contains("You are still the same browser-use agent after context compaction")
        );
        assert!(message.contains("Raw CDP is the center"));
        assert!(message.contains("Do not call `screenshot` repeatedly on an unchanged viewport"));
        assert!(message.contains("The user does not see the pixels inline in the terminal"));
        assert!(message.contains("Loaded Browser-Harness Interaction Skills"));
        assert!(message.contains("interaction-skills/screenshots.md"));
        assert!(message.contains("interaction-skills/tabs.md"));
        assert!(message.contains("Compacted prior browser-agent context"));
        assert!(message.contains("https://example.com"));
        Ok(())
    }

    #[test]
    fn compaction_ignores_image_data_urls_for_text_budget() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, temp.path())?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({ "text": "inspect screenshot" }),
        )?;
        let mut messages = vec![
            serde_json::json!({
                "role": "user",
                "content": "inspect screenshot",
            }),
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "shot_call",
                    "name": "python",
                    "arguments": { "code": "screenshot('current')" },
                }],
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "shot_call",
                "name": "python",
                "content": [
                    { "type": "output_text", "text": "screenshot captured" },
                    {
                        "type": "input_image",
                        "image_url": format!("data:image/png;base64,{}", "a".repeat(50_000)),
                        "detail": "auto",
                    },
                ],
            }),
        ];

        maybe_compact_messages(&store, &session.id, &mut messages, 10_000)?;

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["content"][1]["type"], "input_image");
        let events = store.events_for_session(&session.id)?;
        assert!(!events
            .iter()
            .any(|event| event.event_type == "session.compaction_started"));
        Ok(())
    }

    #[test]
    fn compaction_preserves_recent_tool_result_after_summary() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, temp.path())?;
        store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({ "text": "compact after tool" }),
        )?;
        let mut messages = vec![
            serde_json::json!({
                "role": "user",
                "content": "x".repeat(2000),
            }),
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "latest_tool",
                    "name": "python",
                    "arguments": { "code": "print('large')" },
                }],
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "latest_tool",
                "name": "python",
                "content": "y".repeat(2000),
            }),
        ];

        maybe_compact_messages(&store, &session.id, &mut messages, 500)?;

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["tool_calls"][0]["id"], "latest_tool");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "latest_tool");
        assert!(messages[2]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("[truncated]"));
        Ok(())
    }

    #[test]
    fn history_normalization_adds_missing_tool_outputs() -> Result<()> {
        let mut messages = vec![
            serde_json::json!({
                "role": "user",
                "content": "do work",
            }),
            serde_json::json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "call_missing",
                        "name": "python",
                        "arguments": {"code": "print('lost')"},
                    },
                    {
                        "id": "call_done",
                        "name": "update_plan",
                        "arguments": {"plan": [{"step": "done", "status": "completed"}]},
                    }
                ],
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "call_done",
                "name": "update_plan",
                "content": "plan updated",
            }),
        ];

        normalize_provider_messages(&mut messages);

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["tool_call_id"], "call_done");
        assert_eq!(messages[3]["tool_call_id"], "call_missing");
        assert!(messages[3]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("history normalization"));
        Ok(())
    }

    #[test]
    fn history_normalization_converts_orphan_tool_output_to_context() -> Result<()> {
        let mut messages = vec![serde_json::json!({
            "role": "tool",
            "tool_call_id": "orphan_call",
            "name": "python",
            "content": [
                {
                    "type": "output_text",
                    "text": "orphan output",
                },
                {
                    "type": "input_image",
                    "image_url": "data:image/png;base64,ZmFrZQ==",
                    "detail": "auto",
                }
            ],
        })];

        normalize_provider_messages(&mut messages);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        let content = messages[0]["content"].as_array().context("content array")?;
        assert!(content[0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("orphan output"));
        assert_eq!(content[1]["type"], "input_image");
        Ok(())
    }

    #[test]
    fn done_can_use_result_file_from_artifact_root() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let session = store.create_session(None, temp.path())?;
        std::fs::write(
            Path::new(&session.artifact_root).join("answer.json"),
            r#"{"items":[{"id":1},{"id":2}]}"#,
        )?;

        let outcome = dispatch_done_tool(
            &store,
            &session,
            &ToolCall {
                id: "done_result_file".to_string(),
                name: "done".to_string(),
                arguments: serde_json::json!({
                    "result_file": "answer.json",
                }),
            },
        )?;
        assert!(outcome.finished);
        let events = store.events_for_session(&session.id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "session.done"
                && event.payload["source"] == "done.result_file"
                && event.payload["result_file"] == "answer.json"
        }));
        let done = events
            .iter()
            .find(|event| event.event_type == "session.done")
            .context("missing session.done")?;
        let result = done.payload["result"].as_str().context("result text")?;
        assert!(result.contains("Saved result file."));
        assert!(result.contains("answer.json"));
        assert!(!result.contains("\"id\":2"));
        assert!(done.payload["result_file_path"]
            .as_str()
            .is_some_and(|path| path.ends_with("answer.json")));
        assert!(done.payload["result_file_url"]
            .as_str()
            .is_some_and(|path| path.starts_with("file://")));
        assert!(done.payload["result_file_directory_url"]
            .as_str()
            .is_some_and(|path| path.starts_with("file://")));
        assert!(
            done.payload["result_file_bytes"]
                .as_u64()
                .unwrap_or_default()
                > 0
        );
        assert_eq!(done.payload["result_file_mime"], "application/json");
        assert!(events.iter().any(|event| {
            event.event_type == "artifact.created"
                && event.payload["name"] == "done"
                && event.payload["artifact"]["source"] == "done.result_file"
                && event.payload["artifact"]["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("answer.json"))
        }));
        Ok(())
    }

    #[test]
    fn provider_loop_emits_deadline_warning_on_final_turn() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = ScriptedProvider::new(vec![
            vec![ModelEvent::Done],
            vec![ModelEvent::ToolCall {
                call: ToolCall {
                    id: "done_final".to_string(),
                    name: "done".to_string(),
                    arguments: serde_json::json!({"result": "finished on final turn"}),
                },
            }],
        ]);
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "deadline warning",
            temp.path(),
            AgentRunOptions {
                max_turns: 2,
                max_context_chars: 80_000,
                browser_mode: None,
                python_tool_timeout_seconds: 120,
                python_env: Vec::new(),
                child_agent_runner: None,
            },
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.deadline_warning"
                && event.payload["remaining_turns"] == 1
                && event.payload["max_turns"] == 2));
        assert!(events.iter().any(|event| event.event_type == "session.done"
            && event.payload["result"] == "finished on final turn"));
        Ok(())
    }

    #[test]
    fn provider_can_use_agent_delegation_tools_without_copying_child_events() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let provider = AgentToolProvider::default();
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "research flights",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let children = store.list_child_agents(&session_id)?;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].status, "closed");
        assert_eq!(
            children[0].agent_path.as_deref(),
            Some("/root/flight_search")
        );

        let parent_events = store.events_for_session(&session_id)?;
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "agent.spawned"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "agent.completed"
                && event.payload["payload"]["result"] == "child inspected constraints"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "agent.cancelled"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "agent.message"
                && event.payload["trigger_turn"] == false));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "agent.message"
                && event.payload["trigger_turn"] == true
                && event.payload["author_path"] == "/root"
                && event.payload["recipient_path"] == "/root/flight_search"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "model.tool_call"
                && event.payload["name"] == "spawn_agent"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "model.tool_call"
                && event.payload["name"] == "send_message"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "model.tool_call"
                && event.payload["name"] == "send_input"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "model.tool_call"
                && event.payload["name"] == "followup_task"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "model.tool_call"
                && event.payload["name"] == "wait_agent"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "model.tool_call"
                && event.payload["name"] == "list_agents"));
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "model.tool_call"
                && event.payload["name"] == "close_agent"));

        let child_events = store.events_for_session(&children[0].child_session_id)?;
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "agent.context"));
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "agent.context"
                && event.payload["fork_mode"] == "summary"
                && event.payload["context"]["mode"] == "summary"));
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "session.input"
                && event.payload["text"] == "inspect flight search constraints"));
        assert!(child_events
            .iter()
            .any(|event| event.event_type == "session.followup"
                && event.payload["text"] == "run the focused follow-up"));
        assert_eq!(
            child_events
                .iter()
                .filter(|event| event.event_type == "session.done"
                    && event.payload["result"] == "child inspected constraints")
                .count(),
            3
        );
        let mailbox = store.messages_for_agent(&children[0].child_session_id)?;
        assert_eq!(mailbox.len(), 3);
        assert!(!mailbox[0].trigger_turn);
        assert!(mailbox[1].trigger_turn);
        assert!(mailbox[2].trigger_turn);
        Ok(())
    }

    #[test]
    fn spawn_agent_with_child_runner_returns_before_child_finishes_and_wait_blocks() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let state_dir = store.state_dir().to_path_buf();
        let runner = ChildAgentRunner::new(move |request| {
            let store = Store::open(&state_dir)?;
            std::thread::sleep(Duration::from_millis(120));
            store.append_event(
                &request.child_session_id,
                "session.done",
                serde_json::json!({"result": "repo mapped"}),
            )?;
            Ok(())
        });
        let provider = BlockingExplorerParentProvider::default();
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "understand this repo",
            temp.path(),
            AgentRunOptions::default().with_child_agent_runner(runner),
        )?;
        let parent_events = store.events_for_session(&session_id)?;
        let spawn_finished_seq = parent_events
            .iter()
            .find(|event| {
                event.event_type == "tool.finished" && event.payload["name"] == "spawn_agent"
            })
            .map(|event| event.seq)
            .context("spawn_agent finished event")?;
        let completed_seq = parent_events
            .iter()
            .find(|event| event.event_type == "agent.completed")
            .map(|event| event.seq)
            .context("agent.completed event")?;
        assert!(spawn_finished_seq < completed_seq);
        assert!(parent_events.iter().any(|event| {
            event.event_type == "agent.wait.started" && event.payload["target"] == "repo_explorer"
        }));
        assert!(parent_events.iter().any(|event| {
            event.event_type == "session.done" && event.payload["result"] == "parent used child"
        }));
        let children = store.list_child_agents(&session_id)?;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].status, "done");
        Ok(())
    }

    #[test]
    fn done_rejects_active_child_agents_unless_explicitly_closing_them() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let parent = store.create_session(None, temp.path())?;
        let child = store.create_child_session(
            &parent.id,
            temp.path(),
            Some("/root/repo_explorer"),
            Some("repo_explorer"),
            Some("explorer"),
        )?;
        store.append_event(
            &child.id,
            "session.input",
            serde_json::json!({"text": "still running"}),
        )?;
        let rejected = dispatch_done_tool(
            &store,
            &parent,
            &ToolCall {
                id: "done_blocked".to_string(),
                name: "done".to_string(),
                arguments: serde_json::json!({"result": "parent result"}),
            },
        )?;
        assert!(!rejected.finished);
        let content = rejected.messages[0]["content"]
            .as_str()
            .context("tool content")?;
        assert!(content.contains("subagents are still active"));
        assert_eq!(
            store.load_session(&parent.id)?.expect("parent").status,
            SessionStatus::Created
        );

        let closed = dispatch_done_tool(
            &store,
            &parent,
            &ToolCall {
                id: "done_close_children".to_string(),
                name: "done".to_string(),
                arguments: serde_json::json!({
                    "result": "parent result",
                    "finish_and_close_children": true,
                }),
            },
        )?;
        assert!(closed.finished);
        assert_eq!(
            store.load_session(&parent.id)?.expect("parent").status,
            SessionStatus::Done
        );
        assert_eq!(
            store.load_session(&child.id)?.expect("child").status,
            SessionStatus::Cancelled
        );
        Ok(())
    }

    #[test]
    fn wait_agent_without_target_reports_all_direct_children() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let parent = store.create_session(None, temp.path())?;
        let running = store.create_child_session(
            &parent.id,
            temp.path(),
            Some("/root/running_child"),
            None,
            None,
        )?;
        store.append_event(
            &running.id,
            "session.input",
            serde_json::json!({"text": "still running"}),
        )?;
        let done = store.create_child_session(
            &parent.id,
            temp.path(),
            Some("/root/done_child"),
            None,
            None,
        )?;
        store.append_event(
            &done.id,
            "session.done",
            serde_json::json!({"result": "done child result"}),
        )?;
        store.set_child_agent_status(&done.id, "done")?;

        let outcome = dispatch_wait_agent_tool(
            &store,
            &parent,
            &ToolCall {
                id: "wait_all".to_string(),
                name: "wait_agent".to_string(),
                arguments: serde_json::json!({"timeout_ms": 0}),
            },
        )?;
        let content = outcome.messages[0]["content"]
            .as_str()
            .context("tool content")?;
        let data: Value = serde_json::from_str(content)?;
        assert_eq!(data["agents"].as_array().context("agents")?.len(), 2);
        assert!(data["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["status"] == "running"));
        assert!(data["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["result"] == "done child result"));
        Ok(())
    }

    #[test]
    fn wait_agent_without_target_wakes_when_any_child_finishes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let parent = store.create_session(None, temp.path())?;
        let first = store.create_child_session(
            &parent.id,
            temp.path(),
            Some("/root/first_child"),
            None,
            None,
        )?;
        store.append_event(
            &first.id,
            "session.input",
            serde_json::json!({"text": "first running"}),
        )?;
        let second = store.create_child_session(
            &parent.id,
            temp.path(),
            Some("/root/second_child"),
            None,
            None,
        )?;
        store.append_event(
            &second.id,
            "session.input",
            serde_json::json!({"text": "second running"}),
        )?;
        let state_dir = store.state_dir().to_path_buf();
        let second_id = second.id.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            let store = Store::open(&state_dir).expect("open store");
            store
                .append_event(
                    &second_id,
                    "session.done",
                    serde_json::json!({"result": "second finished"}),
                )
                .expect("finish child");
            store
                .set_child_agent_status(&second_id, "done")
                .expect("update edge");
        });

        let outcome = dispatch_wait_agent_tool(
            &store,
            &parent,
            &ToolCall {
                id: "wait_any".to_string(),
                name: "wait_agent".to_string(),
                arguments: serde_json::json!({"timeout_ms": 5_000}),
            },
        )?;
        let content = outcome.messages[0]["content"]
            .as_str()
            .context("tool content")?;
        let data: Value = serde_json::from_str(content)?;
        assert_eq!(data["timed_out"], false);
        assert!(data["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["result"] == "second finished"));
        assert!(data["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["status"] == "running"));
        Ok(())
    }

    #[test]
    fn child_runner_retargets_managed_python_env_to_child_session() {
        let mut options = AgentRunOptions::default().with_python_env(vec![
            ("BU_NAME".to_string(), "but-tui-parent".to_string()),
            (
                "BH_RUNTIME_DIR".to_string(),
                "/tmp/but-tui-parent".to_string(),
            ),
            ("BH_TMP_DIR".to_string(), "/tmp/but-tui-parent".to_string()),
            ("UNCHANGED".to_string(), "1".to_string()),
        ]);
        retarget_managed_child_python_env(&mut options, "child/123");
        let value = |key: &str| {
            options
                .python_env
                .iter()
                .find(|(candidate, _)| candidate == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(value("BU_NAME"), Some("but-tui-child-123"));
        assert_eq!(value("BH_RUNTIME_DIR"), Some("/tmp/but-tui-child-123"));
        assert_eq!(value("BH_TMP_DIR"), Some("/tmp/but-tui-child-123"));
        assert_eq!(value("UNCHANGED"), Some("1"));
    }

    #[test]
    fn wait_agent_can_return_after_timeout_for_active_child() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let parent = store.create_session(None, temp.path())?;
        let child = store.create_child_session(
            &parent.id,
            temp.path(),
            Some("/root/helper"),
            None,
            None,
        )?;
        store.append_event(
            &child.id,
            "session.input",
            serde_json::json!({"text": "still running"}),
        )?;
        let outcome = dispatch_wait_agent_tool(
            &store,
            &parent,
            &ToolCall {
                id: "wait_active".to_string(),
                name: "wait_agent".to_string(),
                arguments: serde_json::json!({
                    "target": "/root/helper",
                    "timeout_ms": 1,
                }),
            },
        )?;
        let content = outcome.messages[0]["content"]
            .as_str()
            .context("tool content")?;
        let data: Value = serde_json::from_str(content)?;
        assert_eq!(data["status"], "running");
        assert_eq!(data["timed_out"], true);

        let parent_events = store.events_for_session(&parent.id)?;
        assert!(parent_events
            .iter()
            .any(|event| event.event_type == "tool.finished"
                && event.payload["name"] == "wait_agent"));
        Ok(())
    }

    #[test]
    fn agent_task_names_resolve_relative_to_current_agent() {
        assert_eq!(
            canonical_agent_path_from_task_name("worker", "/root/researcher")
                .expect("valid task name"),
            "/root/researcher/worker"
        );
        assert_eq!(
            canonical_agent_reference("worker", "/root/researcher"),
            "/root/researcher/worker"
        );
        assert_eq!(
            canonical_agent_reference("/root/other", "/root/researcher"),
            "/root/other"
        );
        assert_eq!(
            canonical_agent_path_from_task_name("BadName", "/root")
                .expect_err("uppercase task names should be rejected"),
            "task_name must use only lowercase letters, digits, and underscores"
        );
    }

    #[test]
    fn spawn_context_supports_none_and_last_n_modes() {
        let events = vec![
            browser_use_protocol::EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s".to_string(),
                ts_ms: 1,
                event_type: "session.input".to_string(),
                payload: serde_json::json!({"text": "first"}),
            },
            browser_use_protocol::EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s".to_string(),
                ts_ms: 2,
                event_type: "session.followup".to_string(),
                payload: serde_json::json!({"text": "second"}),
            },
            browser_use_protocol::EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s".to_string(),
                ts_ms: 3,
                event_type: "session.followup".to_string(),
                payload: serde_json::json!({"text": "third"}),
            },
            browser_use_protocol::EventRecord {
                seq: 4,
                id: "e4".to_string(),
                session_id: "s".to_string(),
                ts_ms: 4,
                event_type: "tool.output".to_string(),
                payload: serde_json::json!({"text": "raw output"}),
            },
        ];
        let none = inherited_context_for_spawn(&events, "none", 0);
        assert_eq!(none["mode"], "none");
        assert!(none.get("recent_turns").is_none());

        let last = inherited_context_for_spawn(&events, "last_n", 2);
        assert_eq!(last["mode"], "last_n");
        assert_eq!(last["recent_turns"].as_array().unwrap().len(), 2);
        assert_eq!(last["recent_turns"][0]["text"], "second");
        assert_eq!(last["recent_turns"][1]["text"], "third");
        assert!(!last.to_string().contains("raw output"));
    }

    #[test]
    fn existing_session_provider_run_uses_sanitized_agent_context() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        let parent = store.create_session(None, temp.path())?;
        store.append_event(
            &parent.id,
            "session.input",
            serde_json::json!({ "text": "parent task" }),
        )?;
        store.append_event(
            &parent.id,
            "tool.output",
            serde_json::json!({ "name": "python", "text": "raw output that must stay out" }),
        )?;
        let parent_events = store.events_for_session(&parent.id)?;
        let context = sanitized_agent_context_from_events(&parent_events);
        let child =
            store.create_child_session(&parent.id, temp.path(), Some("child"), None, None)?;
        store.append_event(
            &child.id,
            "agent.context",
            serde_json::json!({
                "from_session_id": parent.id,
                "context": context,
            }),
        )?;
        store.append_event(
            &child.id,
            "session.input",
            serde_json::json!({ "text": "child task" }),
        )?;
        let provider = ContextAssertingProvider;
        let session_id = run_existing_session_with_provider(
            &store,
            &provider,
            &child.id,
            AgentRunOptions::default(),
        )?;
        assert_eq!(session_id, child.id);
        let events = store.events_for_session(&child.id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.done"
                && event.payload["result"] == "context ok"));
        Ok(())
    }

    #[test]
    fn python_image_outputs_are_forwarded_to_next_model_turn() -> Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::write(temp.path().join("shot.png"), b"not-really-a-png")?;
        let store = Store::open(temp.path().join("state"))?;
        let provider = ImageInspectingProvider::default();
        let session_id = run_agent_with_provider(
            &store,
            &provider,
            "look at page",
            temp.path(),
            AgentRunOptions::default(),
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events.iter().any(|event| event.event_type == "tool.image"));
        assert!(events.iter().any(|event| event.event_type == "session.done"
            && event.payload["result"] == "image context ok"));
        Ok(())
    }

    #[test]
    fn generic_tool_text_spill_preserves_image_parts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path().join("state"))?;
        let session = store.create_session(None, temp.path())?;
        let huge_len = token_budget_to_char_budget(MAX_TOOL_OUTPUT_TEXT_TOKENS) + 1024;
        let huge_text = "x".repeat(huge_len);
        let call = ToolCall {
            id: "view_huge".to_string(),
            name: "view_image".to_string(),
            arguments: serde_json::json!({}),
        };

        let message = tool_content_message(
            &store,
            &session,
            &call,
            "view_image",
            serde_json::json!([
                {
                    "type": "output_text",
                    "text": huge_text,
                },
                {
                    "type": "input_image",
                    "image_url": "data:image/png;base64,ZmFrZQ==",
                    "detail": "auto",
                }
            ]),
        )?;

        let content = message["content"].as_array().context("content array")?;
        assert_eq!(content.len(), 2);
        let preview = content[0]["text"].as_str().context("preview text")?;
        assert!(preview.contains("[truncated]"));
        assert!(preview.contains("Full tool output saved to:"));
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/png;base64,ZmFrZQ==");
        let artifact_path = preview
            .lines()
            .find_map(|line| line.strip_prefix("Full tool output saved to: "))
            .context("artifact path")?;
        let spilled = std::fs::read_to_string(artifact_path)?;
        assert_eq!(spilled.len(), huge_len);
        assert!(store
            .artifacts_for_session(&session.id)?
            .iter()
            .any(|artifact| artifact.kind == "tool-output"));
        Ok(())
    }

    #[test]
    fn json_tool_outputs_spill_to_artifact() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path().join("state"))?;
        let session = store.create_session(None, temp.path())?;
        let call = ToolCall {
            id: "exec_huge".to_string(),
            name: "exec_command".to_string(),
            arguments: serde_json::json!({}),
        };

        let message = tool_json_message(
            &store,
            &session,
            &call,
            "exec_command",
            serde_json::json!({
                "output": "z".repeat(token_budget_to_char_budget(MAX_TOOL_OUTPUT_TEXT_TOKENS) + 1024),
                "metadata": {"truncated": true},
            }),
        )?;

        let content = message["content"].as_str().context("tool content")?;
        assert!(content.contains("[truncated]"));
        assert!(content.contains("Full tool output saved to:"));
        let artifact_path = content
            .lines()
            .find_map(|line| line.strip_prefix("Full tool output saved to: "))
            .context("artifact path")?;
        let spilled = std::fs::read_to_string(artifact_path)?;
        assert!(spilled.contains("\"metadata\":{\"truncated\":true}"));
        assert!(spilled.contains(&"z".repeat(1024)));
        Ok(())
    }

    #[derive(Default)]
    struct ParallelReadOrderProvider {
        step: std::sync::Mutex<usize>,
    }

    impl ModelProvider for ParallelReadOrderProvider {
        fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
            let mut step = self.step.lock().expect("step lock");
            let events = if *step == 0 {
                vec![
                    ModelEvent::ToolCall {
                        call: ToolCall {
                            id: "read_second".to_string(),
                            name: "read_file".to_string(),
                            arguments: serde_json::json!({ "path": "second.txt" }),
                        },
                    },
                    ModelEvent::ToolCall {
                        call: ToolCall {
                            id: "read_first".to_string(),
                            name: "read_file".to_string(),
                            arguments: serde_json::json!({ "path": "first.txt" }),
                        },
                    },
                    ModelEvent::Done,
                ]
            } else {
                let tool_call_ids = turn
                    .messages
                    .iter()
                    .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
                    .filter_map(|message| message.get("tool_call_id").and_then(Value::as_str))
                    .collect::<Vec<_>>();
                assert_eq!(
                    tool_call_ids.as_slice(),
                    ["read_second", "read_first"],
                    "tool outputs must be returned in model call order"
                );
                vec![
                    ModelEvent::ToolCall {
                        call: ToolCall {
                            id: "done_parallel_reads".to_string(),
                            name: "done".to_string(),
                            arguments: serde_json::json!({
                                "result": "parallel reads ok",
                            }),
                        },
                    },
                    ModelEvent::Done,
                ]
            };
            *step += 1;
            Ok(events)
        }
    }

    #[derive(Default)]
    struct ContextOverflowRecoveringProvider {
        step: std::sync::Mutex<usize>,
    }

    impl ModelProvider for ContextOverflowRecoveringProvider {
        fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
            let mut step = self.step.lock().expect("step lock");
            if *step == 0 {
                *step += 1;
                bail!("context_length_exceeded: input is too long");
            }
            let joined = turn
                .messages
                .iter()
                .map(message_content_text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(joined.contains("Compacted prior browser-agent context"));
            assert!(joined.contains("compact after provider overflow"));
            *step += 1;
            Ok(vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_after_overflow".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({
                            "result": "overflow recovered",
                        }),
                    },
                },
                ModelEvent::Done,
            ])
        }
    }

    #[derive(Default)]
    struct ImageInspectingProvider {
        step: std::sync::Mutex<usize>,
    }

    impl ModelProvider for ImageInspectingProvider {
        fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
            let mut step = self.step.lock().expect("step lock");
            let event = if *step == 0 {
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "python_image".to_string(),
                        name: "python".to_string(),
                        arguments: serde_json::json!({
                            "code": "emit_image('shot.png', label='page')",
                        }),
                    },
                }
            } else {
                assert!(turn.messages.iter().any(message_has_input_image));
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_image".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({
                            "result": "image context ok",
                        }),
                    },
                }
            };
            *step += 1;
            Ok(vec![event, ModelEvent::Done])
        }
    }

    fn message_has_input_image(message: &Value) -> bool {
        message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|part| {
                part.get("type").and_then(Value::as_str) == Some("input_image")
                    && part
                        .get("image_url")
                        .and_then(Value::as_str)
                        .is_some_and(|url| url.starts_with("data:image/png;base64,"))
            })
    }

    struct ContextAssertingProvider;

    impl ModelProvider for ContextAssertingProvider {
        fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
            let joined = turn
                .messages
                .iter()
                .filter_map(|message| message.get("content").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(joined.contains("Inherited compact context"));
            assert!(joined.contains("parent task"));
            assert!(joined.contains("child task"));
            assert!(!joined.contains("raw output that must stay out"));
            Ok(vec![
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_existing".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({"result": "context ok"}),
                    },
                },
                ModelEvent::Done,
            ])
        }
    }

    #[derive(Default)]
    struct AgentToolProvider {
        step: std::sync::Mutex<usize>,
    }

    impl ModelProvider for AgentToolProvider {
        fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
            if turn.messages.iter().any(|message| {
                message
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.contains("inspect flight search constraints"))
            }) {
                return Ok(vec![
                    ModelEvent::ToolCall {
                        call: ToolCall {
                            id: "child_done".to_string(),
                            name: "done".to_string(),
                            arguments: serde_json::json!({
                                "result": "child inspected constraints",
                            }),
                        },
                    },
                    ModelEvent::Done,
                ]);
            }
            let mut step = self.step.lock().expect("step lock");
            let event = match *step {
                0 => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "spawn_1".to_string(),
                        name: "spawn_agent".to_string(),
                        arguments: serde_json::json!({
                            "message": "inspect flight search constraints",
                            "task_name": "flight_search",
                            "nickname": "Flight helper",
                            "agent_type": "researcher",
                        }),
                    },
                },
                1 => {
                    assert_eq!(
                        task_name_from_tool_messages(&turn).as_deref(),
                        Some("/root/flight_search")
                    );
                    ModelEvent::ToolCall {
                        call: ToolCall {
                            id: "send_1".to_string(),
                            name: "send_message".to_string(),
                            arguments: serde_json::json!({
                                "target": "/root/flight_search",
                                "message": "keep this in your notes",
                            }),
                        },
                    }
                }
                2 => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "send_input_1".to_string(),
                        name: "send_input".to_string(),
                        arguments: serde_json::json!({
                            "target": "flight_search",
                            "message": "run the codex-style input",
                        }),
                    },
                },
                3 => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "followup_1".to_string(),
                        name: "followup_task".to_string(),
                        arguments: serde_json::json!({
                            "target": "flight_search",
                            "message": "run the focused follow-up",
                        }),
                    },
                },
                4 => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "wait_1".to_string(),
                        name: "wait_agent".to_string(),
                        arguments: serde_json::json!({
                            "target": "/root/flight_search",
                        }),
                    },
                },
                5 => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "list_1".to_string(),
                        name: "list_agents".to_string(),
                        arguments: serde_json::json!({"path_prefix": "/root"}),
                    },
                },
                6 => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "close_1".to_string(),
                        name: "close_agent".to_string(),
                        arguments: serde_json::json!({
                            "target": "flight_search",
                            "reason": "parent has enough context",
                        }),
                    },
                },
                _ => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "done_1".to_string(),
                        name: "done".to_string(),
                        arguments: serde_json::json!({
                            "result": "delegation path verified",
                        }),
                    },
                },
            };
            *step += 1;
            Ok(vec![event, ModelEvent::Done])
        }
    }

    fn task_name_from_tool_messages(turn: &ProviderTurn) -> Option<String> {
        turn.messages.iter().rev().find_map(|message| {
            if message.get("name").and_then(Value::as_str) != Some("spawn_agent") {
                return None;
            }
            let content = message.get("content").and_then(Value::as_str)?;
            serde_json::from_str::<Value>(content)
                .ok()?
                .get("task_name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
    }

    #[derive(Default)]
    struct BlockingExplorerParentProvider {
        step: std::sync::Mutex<usize>,
    }

    impl ModelProvider for BlockingExplorerParentProvider {
        fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
            let mut step = self.step.lock().expect("step lock");
            let event = match *step {
                0 => ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "spawn_blocking_explorer".to_string(),
                        name: "spawn_agent".to_string(),
                        arguments: serde_json::json!({
                            "message": "map the repository",
                            "task_name": "repo_explorer",
                            "agent_type": "explorer",
                        }),
                    },
                },
                1 => {
                    let spawn = tool_response_json(&turn, "spawn_agent")
                        .context("spawn_agent tool response")?;
                    assert_eq!(spawn["status"], "running");
                    ModelEvent::ToolCall {
                        call: ToolCall {
                            id: "wait_blocking_explorer".to_string(),
                            name: "wait_agent".to_string(),
                            arguments: serde_json::json!({
                                "target": "repo_explorer",
                                "timeout_ms": 5_000,
                            }),
                        },
                    }
                }
                2 => {
                    let wait =
                        tool_response_json(&turn, "wait_agent").context("wait_agent response")?;
                    assert_eq!(wait["status"], "done");
                    assert_eq!(wait["result"], "repo mapped");
                    ModelEvent::ToolCall {
                        call: ToolCall {
                            id: "done_after_blocking_explorer".to_string(),
                            name: "done".to_string(),
                            arguments: serde_json::json!({"result": "parent used child"}),
                        },
                    }
                }
                _ => ModelEvent::Done,
            };
            *step += 1;
            Ok(vec![event, ModelEvent::Done])
        }
    }

    fn tool_response_json(turn: &ProviderTurn, name: &str) -> Option<Value> {
        turn.messages.iter().rev().find_map(|message| {
            if message.get("name").and_then(Value::as_str) != Some(name) {
                return None;
            }
            let content = message.get("content").and_then(Value::as_str)?;
            serde_json::from_str(content).ok()
        })
    }

    #[test]
    fn fake_agent_records_python_host_outputs_and_artifacts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input.txt");
        std::fs::write(&input, "hello")?;
        let store = Store::open(temp.path().join("state"))?;
        let session_id = run_fake_agent(
            &store,
            "copy artifact",
            temp.path(),
            FakeAgentOptions {
                python_code: Some(
                    "emit_output('copied')\ncopy_artifact('input.txt')\nemit_browser_live_url('https://live.example')\nprint('done')",
                ),
            },
        )?;
        let events = store.events_for_session(&session_id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "artifact.created"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "browser.live_url"));
        let artifacts = store.artifacts_for_session(&session_id)?;
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].bytes, Some(5));
        Ok(())
    }

    #[test]
    fn large_python_output_spills_to_artifact() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path().join("state"))?;
        let session_id = run_fake_agent(
            &store,
            "spill output",
            temp.path(),
            FakeAgentOptions {
                python_code: Some("print('x' * 20000)"),
            },
        )?;
        let events = store.events_for_session(&session_id)?;
        let output = events
            .iter()
            .rev()
            .find(|event| {
                event.event_type == "tool.output"
                    && event.payload.get("stream").is_none()
                    && event.payload["name"] == "python"
            })
            .context("final python output event")?;
        assert_eq!(output.payload["text_truncated"], true);
        let path = output.payload["text_artifact"]["path"]
            .as_str()
            .context("text artifact path")?;
        assert!(std::path::Path::new(path).exists());
        assert!(output.payload["text"]
            .as_str()
            .unwrap_or_default()
            .contains("[truncated]"));
        let artifacts = store.artifacts_for_session(&session_id)?;
        assert_eq!(
            artifacts
                .iter()
                .filter(|artifact| artifact.kind == "tool-output")
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn rustls_crypto_provider_install_is_idempotent() {
        install_process_crypto_provider();
        install_process_crypto_provider();
    }
}
