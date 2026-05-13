use anyhow::{bail, Context, Result};
use base64::{
    engine::general_purpose::{self, URL_SAFE_NO_PAD},
    Engine as _,
};
use browser_use_protocol::{ModelEvent, ModelUsage, ToolCall, ToolSpec};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

#[derive(Clone, Debug, Default)]
pub struct ProviderTurn {
    pub messages: Vec<Value>,
    pub tools: Vec<ToolSpec>,
}

pub trait ModelProvider {
    fn provider_name(&self) -> &'static str {
        "unknown"
    }

    fn model_name(&self) -> &str {
        "unknown"
    }

    fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>>;

    fn stream_turn(
        &self,
        turn: ProviderTurn,
        on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
    ) -> Result<()> {
        for event in self.start_turn(turn)? {
            on_event(event)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FakeProvider {
    events: Vec<ModelEvent>,
}

impl FakeProvider {
    pub fn new(events: Vec<ModelEvent>) -> Self {
        Self { events }
    }

    pub fn with_text(text: impl Into<String>) -> Self {
        Self {
            events: vec![
                ModelEvent::TextDelta { text: text.into() },
                ModelEvent::Done,
            ],
        }
    }
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::with_text("ok")
    }
}

impl ModelProvider for FakeProvider {
    fn provider_name(&self) -> &'static str {
        "fake"
    }

    fn model_name(&self) -> &str {
        "fake"
    }

    fn start_turn(&self, _turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
        Ok(self.events.clone())
    }
}

#[derive(Debug)]
pub struct ScriptedProvider {
    turns: Mutex<VecDeque<Vec<ModelEvent>>>,
}

impl ScriptedProvider {
    pub fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Mutex::new(VecDeque::from(turns)),
        }
    }
}

impl ModelProvider for ScriptedProvider {
    fn provider_name(&self) -> &'static str {
        "scripted"
    }

    fn model_name(&self) -> &str {
        "scripted"
    }

    fn start_turn(&self, _turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
        Ok(self
            .turns
            .lock()
            .expect("scripted provider lock")
            .pop_front()
            .unwrap_or_else(|| vec![ModelEvent::Done]))
    }
}

#[derive(Clone, Debug)]
pub struct OpenAIResponsesProvider {
    api_key: String,
    model: String,
    base_url: String,
    instructions: String,
    client: reqwest::blocking::Client,
}

impl OpenAIResponsesProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_base_url(api_key, model, "https://api.openai.com/v1")
    }

    pub fn with_base_url(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            instructions: default_instructions(),
            client: reqwest::blocking::Client::new(),
        }
    }

    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let api_key = std::env::var("LLM_BROWSER_OPENAI_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .context("set LLM_BROWSER_OPENAI_API_KEY or OPENAI_API_KEY")?;
        let base_url = std::env::var("LLM_BROWSER_OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        Ok(Self::with_base_url(api_key, model, base_url))
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }
}

impl ModelProvider for OpenAIResponsesProvider {
    fn provider_name(&self) -> &'static str {
        "openai"
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
        let input = messages_to_responses_input(&turn.messages)?;
        let tools = tool_specs_to_responses_tools(&turn.tools);
        let mut body = json!({
            "model": self.model,
            "input": input,
            "instructions": self.instructions,
            "store": false,
            "parallel_tool_calls": true,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }

        let response = self
            .client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .context("send OpenAI Responses request")?;
        let status = response.status();
        let body: Value = response.json().context("parse OpenAI Responses JSON")?;
        if !status.is_success() {
            bail!(
                "OpenAI Responses request failed ({status}): {}",
                openai_error_message(&body)
            );
        }
        parse_responses_output(&body, &self.model)
    }
}

#[derive(Clone, Debug)]
pub struct OpenAICompatibleChatProvider {
    api_key: String,
    model: String,
    base_url: String,
    instructions: String,
    client: reqwest::blocking::Client,
}

impl OpenAICompatibleChatProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_base_url(api_key, model, "https://openrouter.ai/api/v1")
    }

    pub fn with_base_url(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            instructions: default_instructions(),
            client: reqwest::blocking::Client::new(),
        }
    }

    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let api_key = std::env::var("LLM_BROWSER_OPENAI_COMPAT_API_KEY")
            .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
            .context("set LLM_BROWSER_OPENAI_COMPAT_API_KEY or OPENROUTER_API_KEY")?;
        let base_url = std::env::var("LLM_BROWSER_OPENAI_COMPAT_BASE_URL")
            .or_else(|_| std::env::var("OPENROUTER_BASE_URL"))
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
        Ok(Self::with_base_url(api_key, model, base_url))
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }
}

impl ModelProvider for OpenAICompatibleChatProvider {
    fn provider_name(&self) -> &'static str {
        "openai-compatible"
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
        let mut messages = vec![json!({
            "role": "system",
            "content": self.instructions,
        })];
        messages.extend(messages_to_chat_messages(&turn.messages)?);
        let tools = tool_specs_to_chat_tools(&turn.tools);
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "parallel_tool_calls": true,
        });
        if self.base_url.contains("openrouter.ai") || include_openai_compatible_usage() {
            body["usage"] = json!({ "include": true });
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = json!("auto");
        }
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .context("send OpenAI-compatible chat request")?;
        let status = response.status();
        let body: Value = response
            .json()
            .context("parse OpenAI-compatible chat JSON")?;
        if !status.is_success() {
            bail!(
                "OpenAI-compatible chat request failed ({status}): {}",
                openai_error_message(&body)
            );
        }
        parse_chat_completion_output(&body, &self.model)
    }
}

#[derive(Clone, Debug)]
pub struct AnthropicMessagesProvider {
    credential: AnthropicCredential,
    model: String,
    base_url: String,
    instructions: String,
    client: reqwest::blocking::Client,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AnthropicCredential {
    ApiKey(String),
    AuthToken(String),
}

const CLAUDE_CODE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_CODE_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const CLAUDE_CODE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLAUDE_CODE_CALLBACK_HOST: &str = "127.0.0.1";
pub const CLAUDE_CODE_CALLBACK_PORT: u16 = 53692;
pub const CLAUDE_CODE_CALLBACK_PATH: &str = "/callback";
pub const CLAUDE_CODE_REDIRECT_URI: &str = "http://localhost:53692/callback";
const CLAUDE_CODE_SCOPES: &str =
    "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const CLAUDE_CODE_VERSION: &str = "2.1.75";
const ANTHROPIC_BETA_FEATURES: &[&str] = &[
    "fine-grained-tool-streaming-2025-05-14",
    "interleaved-thinking-2025-05-14",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeCodeOAuthCredential {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_ms: i64,
}

#[derive(Debug, Deserialize)]
struct ClaudeCodeTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaudeCodeAuthorization {
    pub code: Option<String>,
    pub state: Option<String>,
}

impl AnthropicMessagesProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_base_url(api_key, model, "https://api.anthropic.com/v1")
    }

    pub fn with_base_url(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::with_credential(AnthropicCredential::ApiKey(api_key.into()), model, base_url)
    }

    pub fn with_auth_token(
        auth_token: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::with_credential(
            AnthropicCredential::AuthToken(auth_token.into()),
            model,
            base_url,
        )
    }

    fn with_credential(
        credential: AnthropicCredential,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            credential,
            model: model.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            instructions: default_instructions(),
            client: reqwest::blocking::Client::new(),
        }
    }

    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let base_url = std::env::var("LLM_BROWSER_ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com/v1".to_string());
        if let Ok(api_key) = std::env::var("LLM_BROWSER_ANTHROPIC_API_KEY")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        {
            if !api_key.trim().is_empty() {
                return Ok(Self::with_base_url(api_key, model, base_url));
            }
        }
        if let Ok(auth_token) = std::env::var("LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN")
            .or_else(|_| std::env::var("CLAUDE_CODE_OAUTH_TOKEN"))
            .or_else(|_| std::env::var("LLM_BROWSER_ANTHROPIC_OAUTH_TOKEN"))
            .or_else(|_| std::env::var("ANTHROPIC_OAUTH_TOKEN"))
            .or_else(|_| std::env::var("ANTHROPIC_AUTH_TOKEN"))
        {
            if !auth_token.trim().is_empty() {
                return Ok(Self::with_auth_token(auth_token, model, base_url));
            }
        }
        bail!("set LLM_BROWSER_ANTHROPIC_API_KEY, ANTHROPIC_API_KEY, CLAUDE_CODE_OAUTH_TOKEN, or ANTHROPIC_AUTH_TOKEN")
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }

    fn is_oauth(&self) -> bool {
        match &self.credential {
            AnthropicCredential::AuthToken(_) => true,
            AnthropicCredential::ApiKey(value) => is_claude_code_oauth_token(value),
        }
    }
}

impl ModelProvider for AnthropicMessagesProvider {
    fn provider_name(&self) -> &'static str {
        "anthropic"
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
        let is_oauth = self.is_oauth();
        let tools = tool_specs_to_anthropic_tools(&turn.tools, is_oauth);
        let mut body = json!({
            "model": self.model,
            "max_tokens": 16000,
            "system": anthropic_system_blocks(&self.instructions, is_oauth),
            "messages": messages_to_anthropic_messages(&turn.messages, is_oauth)?,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = json!({"type": "auto"});
        }
        let mut request = self
            .client
            .post(format!("{}/messages", self.base_url))
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-dangerous-direct-browser-access", "true");
        request = match &self.credential {
            AnthropicCredential::ApiKey(api_key) if !is_oauth => request
                .header("x-api-key", api_key)
                .header("anthropic-beta", ANTHROPIC_BETA_FEATURES.join(",")),
            AnthropicCredential::ApiKey(auth_token)
            | AnthropicCredential::AuthToken(auth_token) => {
                let mut beta = vec!["claude-code-20250219", "oauth-2025-04-20"];
                beta.extend_from_slice(ANTHROPIC_BETA_FEATURES);
                request
                    .bearer_auth(auth_token)
                    .header("anthropic-beta", beta.join(","))
                    .header("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}"))
                    .header("x-app", "cli")
            }
        };
        let response = request
            .json(&body)
            .send()
            .context("send Anthropic Messages request")?;
        let status = response.status();
        let body: Value = response.json().context("parse Anthropic Messages JSON")?;
        if !status.is_success() {
            bail!(
                "Anthropic Messages request failed ({status}): {}",
                anthropic_error_message(&body)
            );
        }
        parse_anthropic_messages_output(&body, &self.model, &turn.tools, is_oauth)
    }
}

pub fn claude_code_oauth_pkce() -> (String, String) {
    let mut verifier_bytes = Vec::with_capacity(32);
    verifier_bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    verifier_bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub fn claude_code_oauth_authorize_url(verifier: &str, challenge: &str) -> String {
    form_url(
        CLAUDE_CODE_AUTHORIZE_URL,
        &[
            ("code", "true"),
            ("client_id", CLAUDE_CODE_CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", CLAUDE_CODE_REDIRECT_URI),
            ("scope", CLAUDE_CODE_SCOPES),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", verifier),
        ],
    )
}

pub fn parse_claude_code_authorization_input(value: &str) -> ClaudeCodeAuthorization {
    let mut stripped = value.trim();
    if stripped.is_empty() {
        return ClaudeCodeAuthorization::default();
    }
    if let Some((_, query)) = stripped.split_once('?') {
        stripped = query.split('#').next().unwrap_or(query);
    }
    if stripped.contains("code=") || stripped.contains("state=") {
        let mut authorization = ClaudeCodeAuthorization::default();
        for (key, value) in parse_form_pairs(stripped) {
            match key.as_str() {
                "code" => authorization.code = Some(value),
                "state" => authorization.state = Some(value),
                _ => {}
            }
        }
        return authorization;
    }
    if let Some((code, state)) = stripped.split_once('#') {
        return ClaudeCodeAuthorization {
            code: Some(code.trim().to_string()),
            state: Some(state.trim().to_string()),
        };
    }
    ClaudeCodeAuthorization {
        code: Some(stripped.to_string()),
        state: None,
    }
}

pub fn exchange_claude_code_authorization_code(
    code: &str,
    state: &str,
    verifier: &str,
) -> Result<ClaudeCodeOAuthCredential> {
    post_claude_code_oauth_token(json!({
        "grant_type": "authorization_code",
        "client_id": CLAUDE_CODE_CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": CLAUDE_CODE_REDIRECT_URI,
        "code_verifier": verifier,
    }))
}

pub fn refresh_claude_code_oauth(refresh_token: &str) -> Result<ClaudeCodeOAuthCredential> {
    if refresh_token.trim().is_empty() {
        bail!("missing Claude Code refresh token");
    }
    post_claude_code_oauth_token(json!({
        "grant_type": "refresh_token",
        "client_id": CLAUDE_CODE_CLIENT_ID,
        "refresh_token": refresh_token.trim(),
    }))
}

pub fn is_claude_code_oauth_token(token: &str) -> bool {
    token.starts_with("sk-ant-oat") || token.contains("sk-ant-oat")
}

fn post_claude_code_oauth_token(body: Value) -> Result<ClaudeCodeOAuthCredential> {
    let client = reqwest::blocking::Client::new();
    let response = client
        .post(CLAUDE_CODE_TOKEN_URL)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .json(&body)
        .send()
        .context("send Anthropic OAuth token request")?;
    let status = response.status();
    let text = response
        .text()
        .context("read Anthropic OAuth token response")?;
    if !status.is_success() {
        bail!(
            "Anthropic OAuth token request failed ({status}): {}",
            truncate_error_body(&text)
        );
    }
    let payload: ClaudeCodeTokenResponse =
        serde_json::from_str(&text).context("parse Anthropic OAuth token response")?;
    let access_token = payload
        .access_token
        .filter(|value| !value.trim().is_empty())
        .context("Anthropic OAuth response missing access_token")?;
    let refresh_token = payload
        .refresh_token
        .filter(|value| !value.trim().is_empty())
        .context("Anthropic OAuth response missing refresh_token")?;
    let expires_in = payload
        .expires_in
        .filter(|value| *value > 0)
        .context("Anthropic OAuth response missing expires_in")?;
    Ok(ClaudeCodeOAuthCredential {
        access_token,
        refresh_token,
        expires_ms: unix_ms_now() + expires_in.saturating_mul(1000) - 5 * 60 * 1000,
    })
}

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn form_url(base: &str, params: &[(&str, &str)]) -> String {
    let query = params
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{query}")
}

fn parse_form_pairs(value: &str) -> Vec<(String, String)> {
    value
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((percent_decode(key)?, percent_decode(value)?))
        })
        .collect()
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            b' ' => encoded.push('+'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn percent_decode(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut iter = value.as_bytes().iter().copied();
    while let Some(byte) = iter.next() {
        match byte {
            b'+' => bytes.push(b' '),
            b'%' => {
                let hi = iter.next()?;
                let lo = iter.next()?;
                let hex = [hi, lo];
                let hex = std::str::from_utf8(&hex).ok()?;
                bytes.push(u8::from_str_radix(hex, 16).ok()?);
            }
            _ => bytes.push(byte),
        }
    }
    String::from_utf8(bytes).ok()
}

fn truncate_error_body(value: &str) -> String {
    let mut out = value.chars().take(1000).collect::<String>();
    if value.chars().count() > 1000 {
        out.push_str("...");
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAuth {
    pub access_token: String,
    pub account_id: String,
}

#[derive(Clone, Debug)]
pub struct CodexResponsesProvider {
    auth: CodexAuth,
    model: String,
    base_url: String,
    instructions: String,
    client: reqwest::blocking::Client,
}

impl CodexResponsesProvider {
    pub fn new(auth: CodexAuth, model: impl Into<String>) -> Self {
        Self::with_base_url(auth, model, "https://chatgpt.com/backend-api")
    }

    pub fn with_base_url(
        auth: CodexAuth,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            auth,
            model: model.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            instructions: default_instructions(),
            client: reqwest::blocking::Client::new(),
        }
    }

    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let auth = load_codex_auth()?;
        let base_url = std::env::var("LLM_BROWSER_CODEX_BASE_URL")
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string());
        Ok(Self::with_base_url(auth, model, base_url))
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }
}

impl ModelProvider for CodexResponsesProvider {
    fn provider_name(&self) -> &'static str {
        "codex"
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn start_turn(&self, turn: ProviderTurn) -> Result<Vec<ModelEvent>> {
        parse_codex_sse(self.send_turn_request(turn)?, &self.model)
    }

    fn stream_turn(
        &self,
        turn: ProviderTurn,
        on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
    ) -> Result<()> {
        parse_codex_sse_stream(self.send_turn_request(turn)?, &self.model, on_event)
    }
}

impl CodexResponsesProvider {
    fn send_turn_request(&self, turn: ProviderTurn) -> Result<reqwest::blocking::Response> {
        let input = messages_to_responses_input(&turn.messages)?;
        let tools = tool_specs_to_responses_tools(&turn.tools);
        let mut body = json!({
            "model": self.model,
            "input": input,
            "instructions": self.instructions,
            "store": false,
            "stream": true,
            "text": { "verbosity": "low" },
            "tool_choice": "auto",
            "parallel_tool_calls": true,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }

        let response = self
            .client
            .post(codex_responses_url(&self.base_url))
            .bearer_auth(&self.auth.access_token)
            .header("chatgpt-account-id", &self.auth.account_id)
            .header("originator", "browser-use-terminal")
            .header("OpenAI-Beta", "responses=experimental")
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .context("send Codex Responses request")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            bail!(
                "Codex Responses request failed ({status}): {}",
                body.chars().take(1000).collect::<String>()
            );
        }
        Ok(response)
    }
}

fn codex_responses_url(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

pub fn load_codex_auth() -> Result<CodexAuth> {
    if let Ok(access_token) = std::env::var("LLM_BROWSER_CODEX_ACCESS_TOKEN") {
        let account_id = std::env::var("LLM_BROWSER_CODEX_ACCOUNT_ID")
            .context("set LLM_BROWSER_CODEX_ACCOUNT_ID with LLM_BROWSER_CODEX_ACCESS_TOKEN")?;
        return Ok(CodexAuth {
            access_token,
            account_id,
        });
    }
    let path = codex_auth_path().context("could not resolve Codex auth path")?;
    load_codex_auth_file(path)
}

fn codex_auth_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("LLM_BROWSER_CODEX_AUTH_FILE") {
        return Some(PathBuf::from(path));
    }
    if let Ok(home) = std::env::var("CODEX_HOME") {
        return Some(PathBuf::from(home).join("auth.json"));
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".codex").join("auth.json"))
}

pub fn load_codex_auth_file(path: impl AsRef<Path>) -> Result<CodexAuth> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read Codex auth file {}", path.display()))?;
    let file: CodexAuthFile =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let access_token = file
        .tokens
        .as_ref()
        .and_then(|tokens| tokens.access_token.clone())
        .or(file.access_token)
        .context("Codex auth missing access token")?;
    let account_id = file
        .tokens
        .as_ref()
        .and_then(|tokens| tokens.account_id.clone())
        .or(file.account_id)
        .or(file.chatgpt_account_id)
        .or_else(|| {
            file.tokens
                .as_ref()
                .and_then(|tokens| tokens.id_token.as_deref())
                .and_then(account_id_from_id_token)
        })
        .context("Codex auth missing account id")?;
    Ok(CodexAuth {
        access_token,
        account_id,
    })
}

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexAuthTokens>,
    access_token: Option<String>,
    account_id: Option<String>,
    chatgpt_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthTokens {
    access_token: Option<String>,
    account_id: Option<String>,
    id_token: Option<String>,
}

fn account_id_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let decoded = general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    value
        .get("chatgpt_account_id")
        .or_else(|| value.get("account_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn parse_codex_sse(response: reqwest::blocking::Response, model: &str) -> Result<Vec<ModelEvent>> {
    let mut events = Vec::new();
    parse_codex_sse_stream(response, model, &mut |event| {
        events.push(event);
        Ok(())
    })?;
    Ok(events)
}

fn parse_codex_sse_stream(
    response: reqwest::blocking::Response,
    model: &str,
    on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
) -> Result<()> {
    let mut data_lines = Vec::new();
    let mut seen_tool_calls = std::collections::HashSet::new();
    let mut emitted_done = false;
    for line in BufReader::new(response).lines() {
        let line = line.context("read Codex SSE line")?;
        if line.is_empty() {
            flush_sse_event(
                &mut data_lines,
                &mut seen_tool_calls,
                model,
                on_event,
                &mut emitted_done,
            )?;
        } else if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim().to_string());
        }
    }
    flush_sse_event(
        &mut data_lines,
        &mut seen_tool_calls,
        model,
        on_event,
        &mut emitted_done,
    )?;
    if !emitted_done {
        emit_codex_model_event(ModelEvent::Done, on_event, &mut emitted_done)?;
    }
    Ok(())
}

fn flush_sse_event(
    data_lines: &mut Vec<String>,
    seen_tool_calls: &mut std::collections::HashSet<String>,
    model: &str,
    on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
    emitted_done: &mut bool,
) -> Result<()> {
    if data_lines.is_empty() {
        return Ok(());
    }
    let data = data_lines.join("\n");
    data_lines.clear();
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return Ok(());
    }
    let event: Value = serde_json::from_str(&data).context("parse Codex SSE JSON")?;
    match event.get("type").and_then(Value::as_str) {
        Some("response.output_text.delta") => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                emit_codex_model_event(
                    ModelEvent::TextDelta {
                        text: delta.to_string(),
                    },
                    on_event,
                    emitted_done,
                )?;
            }
        }
        Some("response.output_item.done") => {
            if let Some(item) = event.get("item") {
                let mut item_events = Vec::new();
                maybe_push_codex_output_item(item, seen_tool_calls, &mut item_events)?;
                for item_event in item_events {
                    emit_codex_model_event(item_event, on_event, emitted_done)?;
                }
            }
        }
        Some("response.completed") | Some("response.done") | Some("response.incomplete") => {
            if let Some(response) = event.get("response") {
                if let Some(items) = response.get("output").and_then(Value::as_array) {
                    for item in items {
                        let mut item_events = Vec::new();
                        maybe_push_codex_output_item(item, seen_tool_calls, &mut item_events)?;
                        for item_event in item_events {
                            emit_codex_model_event(item_event, on_event, emitted_done)?;
                        }
                    }
                }
                if let Some(usage) = parse_usage(response.get("usage"), model) {
                    emit_codex_model_event(ModelEvent::Usage { usage }, on_event, emitted_done)?;
                }
            }
            emit_codex_model_event(ModelEvent::Done, on_event, emitted_done)?;
        }
        Some("error") => bail!("Codex stream error: {event}"),
        _ => {}
    }
    Ok(())
}

fn emit_codex_model_event(
    event: ModelEvent,
    on_event: &mut dyn FnMut(ModelEvent) -> Result<()>,
    emitted_done: &mut bool,
) -> Result<()> {
    if matches!(event, ModelEvent::Done) {
        *emitted_done = true;
    }
    on_event(event)
}

fn maybe_push_codex_output_item(
    item: &Value,
    seen_tool_calls: &mut std::collections::HashSet<String>,
    events: &mut Vec<ModelEvent>,
) -> Result<()> {
    if item.get("type").and_then(Value::as_str) == Some("function_call") {
        let call_id = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if seen_tool_calls.insert(call_id) {
            parse_response_output_item(item, events)?;
        }
    } else {
        parse_response_output_item(item, events)?;
    }
    Ok(())
}

fn default_instructions() -> String {
    let mut instructions = include_str!("../../../prompts/browser-agent-system.md")
        .trim()
        .to_string();
    instructions.push_str("\n\n## Loaded Browser-Harness Interaction Skills");
    instructions.push_str(
        "\n\nThese are the same interaction-skill playbooks from browser-harness. Apply the relevant section when the page mechanic appears.",
    );
    for (path, content) in browser_harness_interaction_skills() {
        instructions.push_str("\n\n### ");
        instructions.push_str(path);
        instructions.push_str("\n\n");
        instructions.push_str(content.trim());
    }
    instructions
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

fn tool_specs_to_responses_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect()
}

fn tool_specs_to_chat_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect()
}

fn tool_specs_to_anthropic_tools(tools: &[ToolSpec], is_oauth: bool) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "name": anthropic_request_tool_name(&tool.name, is_oauth),
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect()
}

fn messages_to_responses_input(messages: &[Value]) -> Result<Vec<Value>> {
    let mut input = Vec::new();
    let mut seen_tool_calls = HashSet::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "tool" => {
                let call_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .context("tool message missing tool_call_id")?;
                if seen_tool_calls.contains(call_id) {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": tool_output_text(message),
                    }));
                    if let Some(visual_context) = responses_visual_context_message(message, call_id)
                    {
                        input.push(visual_context);
                    }
                } else if let Some(orphan_context) =
                    responses_orphan_tool_context_message(message, call_id)
                {
                    input.push(orphan_context);
                }
            }
            "system" => {
                let content = message_content_as_text(message);
                if !content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": format!("System context:\n{content}"),
                        }],
                    }));
                }
            }
            _ => {
                let content = message_content_parts(message, role);
                if !content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": role,
                        "content": content,
                    }));
                }
                if role == "assistant" {
                    for call in message
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if let Some(call_id) = call
                            .get("id")
                            .or_else(|| call.get("call_id"))
                            .and_then(Value::as_str)
                        {
                            seen_tool_calls.insert(call_id.to_string());
                        }
                        input.push(tool_call_to_responses_input(call)?);
                    }
                }
            }
        }
    }
    Ok(input)
}

fn responses_orphan_tool_context_message(message: &Value, call_id: &str) -> Option<Value> {
    let text = tool_output_text(message);
    let images = tool_output_images(message);
    if text.trim().is_empty() && images.is_empty() {
        return None;
    }
    let mut content = vec![json!({
        "type": "input_text",
        "text": format!(
            "Tool output retained as context after history compaction. Original tool call {call_id} ({}):\n{}",
            tool_name(message),
            text,
        ),
    })];
    content.extend(images);
    Some(json!({
        "type": "message",
        "role": "user",
        "content": content,
    }))
}

fn messages_to_chat_messages(messages: &[Value]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "tool" => {
                let call_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .context("tool message missing tool_call_id")?;
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": tool_output_text(message),
                }));
                if let Some(visual_context) = chat_visual_context_message(message, call_id) {
                    out.push(visual_context);
                }
            }
            "assistant" => {
                let mut item = json!({
                    "role": "assistant",
                    "content": message_content_as_text(message),
                });
                let tool_calls = message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(chat_tool_call)
                    .collect::<Result<Vec<_>>>()?;
                if !tool_calls.is_empty() {
                    item["tool_calls"] = Value::Array(tool_calls);
                }
                out.push(item);
            }
            "system" => out.push(json!({
                "role": "system",
                "content": message_content_as_text(message),
            })),
            _ => out.push(json!({
                "role": "user",
                "content": chat_content(message),
            })),
        }
    }
    Ok(out)
}

fn chat_content(message: &Value) -> Value {
    match message.get("content") {
        Some(Value::Array(parts)) => Value::Array(
            parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("input_image") => {
                        let image_url = part.get("image_url").and_then(Value::as_str)?;
                        Some(json!({
                            "type": "image_url",
                            "image_url": { "url": image_url },
                        }))
                    }
                    Some("input_text") | Some("output_text") | Some("text") | None => part
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(|text| json!({ "type": "text", "text": text })),
                    _ => None,
                })
                .collect(),
        ),
        _ => Value::String(message_content_as_text(message)),
    }
}

fn chat_tool_call(call: &Value) -> Result<Value> {
    let call_id = call
        .get("id")
        .or_else(|| call.get("call_id"))
        .and_then(Value::as_str)
        .context("assistant tool call missing id")?;
    let name = call
        .get("name")
        .and_then(Value::as_str)
        .context("assistant tool call missing name")?;
    let arguments = call.get("arguments").cloned().unwrap_or_else(|| json!({}));
    Ok(json!({
        "id": call_id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": json_string(arguments)?,
        }
    }))
}

fn messages_to_anthropic_messages(messages: &[Value], is_oauth: bool) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "assistant" => out.push(json!({
                "role": "assistant",
                "content": anthropic_assistant_content(message, is_oauth)?,
            })),
            "tool" => {
                let call_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .context("tool message missing tool_call_id")?;
                out.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": tool_output_text(message),
                    }],
                }));
                if let Some(visual_context) = anthropic_visual_context_message(message, call_id) {
                    out.push(visual_context);
                }
            }
            "system" => out.push(json!({
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": format!("System context:\n{}", message_content_as_text(message)),
                }],
            })),
            _ => out.push(json!({
                "role": "user",
                "content": anthropic_user_content(message),
            })),
        }
    }
    Ok(out)
}

fn anthropic_user_content(message: &Value) -> Vec<Value> {
    match message.get("content") {
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("input_image") => {
                    let image_url = part.get("image_url").and_then(Value::as_str)?;
                    data_url_source(image_url).map(|(media_type, data)| {
                        json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": data,
                            }
                        })
                    })
                }
                Some("input_text") | Some("output_text") | Some("text") | None => part
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| json!({ "type": "text", "text": text })),
                _ => None,
            })
            .collect(),
        _ => vec![json!({
            "type": "text",
            "text": message_content_as_text(message),
        })],
    }
}

fn anthropic_assistant_content(message: &Value, is_oauth: bool) -> Result<Vec<Value>> {
    let mut blocks = Vec::new();
    let text = message_content_as_text(message);
    if !text.is_empty() {
        blocks.push(json!({ "type": "text", "text": text }));
    }
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        blocks.push(anthropic_tool_use_block(call, is_oauth)?);
    }
    Ok(blocks)
}

fn anthropic_tool_use_block(call: &Value, is_oauth: bool) -> Result<Value> {
    let call_id = call
        .get("id")
        .or_else(|| call.get("call_id"))
        .and_then(Value::as_str)
        .context("assistant tool call missing id")?;
    let name = call
        .get("name")
        .and_then(Value::as_str)
        .context("assistant tool call missing name")?;
    let input = call.get("arguments").cloned().unwrap_or_else(|| json!({}));
    Ok(json!({
        "type": "tool_use",
        "id": call_id,
        "name": anthropic_request_tool_name(name, is_oauth),
        "input": input,
    }))
}

fn anthropic_system_blocks(instructions: &str, is_oauth: bool) -> Value {
    let mut blocks = Vec::new();
    if is_oauth {
        blocks.push(json!({
            "type": "text",
            "text": "You are Claude Code, Anthropic's official CLI for Claude.",
            "cache_control": { "type": "ephemeral" },
        }));
    }
    blocks.push(json!({
        "type": "text",
        "text": instructions,
        "cache_control": { "type": "ephemeral" },
    }));
    Value::Array(blocks)
}

fn anthropic_request_tool_name(name: &str, is_oauth: bool) -> String {
    if !is_oauth {
        return name.to_string();
    }
    match name.to_ascii_lowercase().as_str() {
        "read" => "Read".to_string(),
        "write" => "Write".to_string(),
        "edit" => "Edit".to_string(),
        "shell" | "bash" => "Bash".to_string(),
        "grep" => "Grep".to_string(),
        "glob" => "Glob".to_string(),
        "todo_write" | "todowrite" => "TodoWrite".to_string(),
        "web_fetch" | "webfetch" => "WebFetch".to_string(),
        "web_search" | "websearch" => "WebSearch".to_string(),
        _ => name.to_string(),
    }
}

fn anthropic_response_tool_name(name: &str, tools: &[ToolSpec], is_oauth: bool) -> String {
    if !is_oauth {
        return name.to_string();
    }
    let lower = name.to_ascii_lowercase();
    for tool in tools {
        if lower == tool.name.to_ascii_lowercase()
            || lower == anthropic_request_tool_name(&tool.name, true).to_ascii_lowercase()
        {
            return tool.name.clone();
        }
    }
    if lower == "bash" {
        return "shell".to_string();
    }
    name.to_string()
}

fn data_url_source(image_url: &str) -> Option<(String, String)> {
    let rest = image_url.strip_prefix("data:")?;
    let (header, data) = rest.split_once(',')?;
    let media_type = header.split(';').next()?.to_string();
    Some((media_type, data.to_string()))
}

fn input_text_type_for_role(role: &str) -> &'static str {
    if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    }
}

fn message_content_parts(message: &Value, role: &str) -> Vec<Value> {
    match message.get("content") {
        Some(Value::String(content)) if !content.is_empty() => vec![json!({
            "type": input_text_type_for_role(role),
            "text": content,
        })],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| normalize_content_part(part, role))
            .collect(),
        Some(other) if !other.is_null() => vec![json!({
            "type": input_text_type_for_role(role),
            "text": other.to_string(),
        })],
        _ => Vec::new(),
    }
}

fn normalize_content_part(part: &Value, role: &str) -> Option<Value> {
    match part.get("type").and_then(Value::as_str) {
        Some("input_image") => {
            let image_url = part.get("image_url").and_then(Value::as_str)?;
            let mut out = json!({
                "type": "input_image",
                "image_url": image_url,
            });
            if let Some(detail) = part.get("detail").and_then(Value::as_str) {
                out["detail"] = json!(detail);
            }
            Some(out)
        }
        Some("input_text") | Some("output_text") | Some("text") | None => part
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| {
                json!({
                    "type": input_text_type_for_role(role),
                    "text": text,
                })
            }),
        _ => None,
    }
}

fn message_content_as_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(content)) => content.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

fn tool_output_text(message: &Value) -> String {
    let Some(Value::Array(parts)) = message.get("content") else {
        return message_content_as_text(message);
    };
    let mut text_parts = Vec::new();
    let mut image_count = 0;
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_image") => image_count += 1,
            Some("output_text") | Some("input_text") | Some("text") | None => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text_parts.push(text.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    if image_count > 0 {
        text_parts.push(format!(
            "[{image_count} screenshot image(s) attached in the following visual context message]"
        ));
    }
    text_parts.join("\n")
}

fn tool_output_images(message: &Value) -> Vec<Value> {
    match message.get("content") {
        Some(Value::Array(parts)) => parts.iter().filter_map(normalize_tool_image_part).collect(),
        _ => Vec::new(),
    }
}

fn normalize_tool_image_part(part: &Value) -> Option<Value> {
    if part.get("type").and_then(Value::as_str) != Some("input_image") {
        return None;
    }
    let image_url = part.get("image_url").and_then(Value::as_str)?;
    let mut out = json!({
        "type": "input_image",
        "image_url": image_url,
    });
    if let Some(detail) = part.get("detail").and_then(Value::as_str) {
        out["detail"] = json!(detail);
    }
    Some(out)
}

fn visual_context_text(call_id: &str, tool_name: &str) -> String {
    format!(
        "Visual context from tool call {call_id} ({tool_name}). Use these screenshots to verify the browser state before continuing. Do not call screenshot again unless the page changed or you need a different visual region."
    )
}

fn tool_name(message: &Value) -> &str {
    message
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
}

fn responses_visual_context_message(message: &Value, call_id: &str) -> Option<Value> {
    let images = tool_output_images(message);
    if images.is_empty() {
        return None;
    }
    let mut content = vec![json!({
        "type": "input_text",
        "text": visual_context_text(call_id, tool_name(message)),
    })];
    content.extend(images);
    Some(json!({
        "type": "message",
        "role": "user",
        "content": content,
    }))
}

fn chat_visual_context_message(message: &Value, call_id: &str) -> Option<Value> {
    let images = tool_output_images(message);
    if images.is_empty() {
        return None;
    }
    let mut content = vec![json!({
        "type": "input_text",
        "text": visual_context_text(call_id, tool_name(message)),
    })];
    content.extend(images);
    let visual_message = json!({ "content": content });
    Some(json!({
        "role": "user",
        "content": chat_content(&visual_message),
    }))
}

fn anthropic_visual_context_message(message: &Value, call_id: &str) -> Option<Value> {
    let images = tool_output_images(message);
    if images.is_empty() {
        return None;
    }
    let mut content = vec![json!({
        "type": "input_text",
        "text": visual_context_text(call_id, tool_name(message)),
    })];
    content.extend(images);
    let visual_message = json!({ "content": content });
    Some(json!({
        "role": "user",
        "content": anthropic_user_content(&visual_message),
    }))
}

fn tool_call_to_responses_input(call: &Value) -> Result<Value> {
    let call_id = call
        .get("id")
        .or_else(|| call.get("call_id"))
        .and_then(Value::as_str)
        .context("assistant tool call missing id")?;
    let name = call
        .get("name")
        .and_then(Value::as_str)
        .context("assistant tool call missing name")?;
    let arguments = call.get("arguments").cloned().unwrap_or_else(|| json!({}));
    Ok(json!({
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": json_string(arguments)?,
    }))
}

fn json_string(value: Value) -> Result<String> {
    match value {
        Value::String(raw) => Ok(raw),
        other => serde_json::to_string(&other).context("serialize tool call arguments"),
    }
}

fn parse_responses_output(body: &Value, model: &str) -> Result<Vec<ModelEvent>> {
    let mut events = Vec::new();
    if let Some(items) = body.get("output").and_then(Value::as_array) {
        for item in items {
            parse_response_output_item(item, &mut events)?;
        }
    }
    if events
        .iter()
        .all(|event| !matches!(event, ModelEvent::TextDelta { .. }))
    {
        if let Some(text) = body.get("output_text").and_then(Value::as_str) {
            if !text.is_empty() {
                events.push(ModelEvent::TextDelta {
                    text: text.to_string(),
                });
            }
        }
    }
    if let Some(usage) = parse_usage(body.get("usage"), model) {
        events.push(ModelEvent::Usage { usage });
    }
    events.push(ModelEvent::Done);
    Ok(events)
}

fn parse_chat_completion_output(body: &Value, model: &str) -> Result<Vec<ModelEvent>> {
    let mut events = Vec::new();
    let Some(message) = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
    else {
        bail!("OpenAI-compatible chat response missing choices[0].message");
    };
    if let Some(content) = message.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            events.push(ModelEvent::TextDelta {
                text: content.to_string(),
            });
        }
    }
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(function) = call.get("function") else {
            continue;
        };
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .context("chat tool call missing function.name")?;
        let call_id = call
            .get("id")
            .and_then(Value::as_str)
            .context("chat tool call missing id")?;
        let arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .unwrap_or_else(|| json!({}));
        events.push(ModelEvent::ToolCall {
            call: ToolCall {
                id: call_id.to_string(),
                name: name.to_string(),
                arguments,
            },
        });
    }
    if let Some(usage) = parse_chat_usage(body.get("usage"), model) {
        events.push(ModelEvent::Usage { usage });
    }
    events.push(ModelEvent::Done);
    Ok(events)
}

fn parse_anthropic_messages_output(
    body: &Value,
    model: &str,
    tools: &[ToolSpec],
    is_oauth: bool,
) -> Result<Vec<ModelEvent>> {
    let mut events = Vec::new();
    for block in body
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    events.push(ModelEvent::TextDelta {
                        text: text.to_string(),
                    });
                }
            }
            Some("tool_use") => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .context("Anthropic tool_use missing name")?;
                let call_id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .context("Anthropic tool_use missing id")?;
                events.push(ModelEvent::ToolCall {
                    call: ToolCall {
                        id: call_id.to_string(),
                        name: anthropic_response_tool_name(name, tools, is_oauth),
                        arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
                    },
                });
            }
            _ => {}
        }
    }
    if let Some(usage) = parse_usage(body.get("usage"), model) {
        events.push(ModelEvent::Usage { usage });
    }
    events.push(ModelEvent::Done);
    Ok(events)
}

fn parse_response_output_item(item: &Value, events: &mut Vec<ModelEvent>) -> Result<()> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => {
            if let Some(parts) = item.get("content").and_then(Value::as_array) {
                for part in parts {
                    if matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("output_text") | Some("text")
                    ) {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            events.push(ModelEvent::TextDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                }
            }
        }
        Some("function_call") => {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .context("function_call missing name")?;
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .context("function_call missing call_id")?;
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .unwrap_or_else(|| json!({}));
            events.push(ModelEvent::ToolCall {
                call: ToolCall {
                    id: call_id.to_string(),
                    name: name.to_string(),
                    arguments,
                },
            });
        }
        _ => {}
    }
    Ok(())
}

fn parse_usage(usage: Option<&Value>, model: &str) -> Option<ModelUsage> {
    let usage = usage?;
    let native_cost = usage
        .get("cost")
        .or_else(|| usage.get("total_cost"))
        .or_else(|| usage.get("cost_usd"))
        .and_then(value_f64);
    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(Value::as_i64);
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(Value::as_i64);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_i64)
        .or_else(|| Some(input_tokens? + output_tokens?));
    let usage = ModelUsage {
        input_tokens,
        input_cached_tokens: usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .or_else(|| {
                usage
                    .get("prompt_tokens_details")
                    .and_then(|details| details.get("cached_tokens"))
            })
            .or_else(|| usage.get("cache_read_input_tokens"))
            .and_then(Value::as_i64),
        input_cache_creation_tokens: usage
            .get("cache_creation_input_tokens")
            .or_else(|| usage.get("prompt_cache_creation_tokens"))
            .and_then(Value::as_i64),
        output_tokens,
        total_tokens,
        input_cost_usd: None,
        input_cached_cost_usd: None,
        input_cache_creation_cost_usd: None,
        output_cost_usd: None,
        cost_usd: native_cost,
        cost_source: native_cost.map(|_| "native".to_string()),
    };
    Some(add_usage_cost(model, usage))
}

fn value_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<f64>().ok()))
}

fn parse_chat_usage(usage: Option<&Value>, model: &str) -> Option<ModelUsage> {
    parse_usage(usage, model)
}

#[derive(Clone, Debug, Default)]
struct ModelPricing {
    input_cost_per_token: Option<f64>,
    output_cost_per_token: Option<f64>,
    cache_read_input_token_cost: Option<f64>,
    cache_creation_input_token_cost: Option<f64>,
}

fn add_usage_cost(model: &str, mut usage: ModelUsage) -> ModelUsage {
    if usage.cost_usd.is_some() {
        usage
            .cost_source
            .get_or_insert_with(|| "native".to_string());
        return usage;
    }
    if !calculate_cost_enabled() {
        return usage;
    }
    let Some(pricing) = model_pricing(model) else {
        return usage;
    };
    let input_tokens = usage.input_tokens.unwrap_or(0).max(0);
    let cached_tokens = usage.input_cached_tokens.unwrap_or(0).max(0);
    let cache_creation_tokens = usage.input_cache_creation_tokens.unwrap_or(0).max(0);
    let output_tokens = usage.output_tokens.unwrap_or(0).max(0);
    let uncached_input_tokens = input_tokens.saturating_sub(cached_tokens);

    usage.input_cost_usd = pricing
        .input_cost_per_token
        .map(|price| uncached_input_tokens as f64 * price);
    usage.input_cached_cost_usd = if cached_tokens > 0 {
        pricing
            .cache_read_input_token_cost
            .map(|price| cached_tokens as f64 * price)
    } else {
        None
    };
    usage.input_cache_creation_cost_usd = if cache_creation_tokens > 0 {
        pricing
            .cache_creation_input_token_cost
            .map(|price| cache_creation_tokens as f64 * price)
    } else {
        None
    };
    usage.output_cost_usd = pricing
        .output_cost_per_token
        .map(|price| output_tokens as f64 * price);

    let total = usage.input_cost_usd.unwrap_or(0.0)
        + usage.input_cached_cost_usd.unwrap_or(0.0)
        + usage.input_cache_creation_cost_usd.unwrap_or(0.0)
        + usage.output_cost_usd.unwrap_or(0.0);
    if total > 0.0 {
        usage.cost_usd = Some(total);
        usage.cost_source = Some("estimated".to_string());
    }
    usage
}

fn include_openai_compatible_usage() -> bool {
    std::env::var("LLM_BROWSER_OPENAI_COMPAT_INCLUDE_USAGE")
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn calculate_cost_enabled() -> bool {
    std::env::var("BU_USE_CALCULATE_COST")
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn model_pricing(model: &str) -> Option<ModelPricing> {
    custom_model_pricing(model).or_else(|| pricing_from_litellm(model))
}

fn custom_model_pricing(model: &str) -> Option<ModelPricing> {
    let pricing = match model {
        "accounts/fireworks/models/glm-4p7" => ModelPricing {
            input_cost_per_token: Some(0.60 / 1_000_000.0),
            output_cost_per_token: Some(2.20 / 1_000_000.0),
            ..Default::default()
        },
        "accounts/fireworks/models/glm-4p7-flash" => ModelPricing {
            input_cost_per_token: Some(0.07 / 1_000_000.0),
            output_cost_per_token: Some(0.40 / 1_000_000.0),
            ..Default::default()
        },
        "accounts/fireworks/models/glm-5" => ModelPricing {
            input_cost_per_token: Some(1.00 / 1_000_000.0),
            output_cost_per_token: Some(3.20 / 1_000_000.0),
            cache_read_input_token_cost: Some(0.20 / 1_000_000.0),
            ..Default::default()
        },
        "accounts/fireworks/models/kimi-k2p5" | "kimi-k2.5" => ModelPricing {
            input_cost_per_token: Some(0.60 / 1_000_000.0),
            output_cost_per_token: Some(3.00 / 1_000_000.0),
            cache_read_input_token_cost: Some(0.10 / 1_000_000.0),
            ..Default::default()
        },
        "accounts/fireworks/models/minimax-m2p5" => ModelPricing {
            input_cost_per_token: Some(0.30 / 1_000_000.0),
            output_cost_per_token: Some(1.20 / 1_000_000.0),
            cache_read_input_token_cost: Some(0.029 / 1_000_000.0),
            ..Default::default()
        },
        _ => return None,
    };
    Some(pricing)
}

fn pricing_from_litellm(model: &str) -> Option<ModelPricing> {
    let pricing_data = litellm_pricing_data()?;
    let model_data = find_model_pricing_data(pricing_data, model)?;
    Some(ModelPricing {
        input_cost_per_token: model_data
            .get("input_cost_per_token")
            .and_then(Value::as_f64),
        output_cost_per_token: model_data
            .get("output_cost_per_token")
            .and_then(Value::as_f64),
        cache_read_input_token_cost: model_data
            .get("cache_read_input_token_cost")
            .and_then(Value::as_f64),
        cache_creation_input_token_cost: model_data
            .get("cache_creation_input_token_cost")
            .and_then(Value::as_f64),
    })
}

fn find_model_pricing_data<'a>(
    pricing_data: &'a HashMap<String, Value>,
    model: &str,
) -> Option<&'a Value> {
    if let Some(data) = pricing_data.get(model) {
        return Some(data);
    }
    if let Some(mapped) = model_to_litellm(model) {
        if let Some(data) = pricing_data.get(mapped) {
            return Some(data);
        }
    }
    for prefix in ["anthropic/", "openai/", "google/", "azure/", "bedrock/"] {
        let prefixed = format!("{prefix}{model}");
        if let Some(data) = pricing_data.get(&prefixed) {
            return Some(data);
        }
    }
    if let Some((_, bare)) = model.split_once('/') {
        if let Some(data) = pricing_data.get(bare) {
            return Some(data);
        }
    }
    None
}

fn model_to_litellm(model: &str) -> Option<&'static str> {
    match model {
        "gemini-flash-latest" => Some("gemini/gemini-flash-latest"),
        _ => None,
    }
}

fn litellm_pricing_data() -> Option<&'static HashMap<String, Value>> {
    static PRICING_DATA: OnceLock<Option<HashMap<String, Value>>> = OnceLock::new();
    PRICING_DATA.get_or_init(load_litellm_pricing_data).as_ref()
}

fn load_litellm_pricing_data() -> Option<HashMap<String, Value>> {
    let cache_path = pricing_cache_dir().join("model_prices_and_context_window.json");
    if cache_path.exists() && cache_file_fresh(&cache_path) {
        if let Ok(raw) = fs::read_to_string(&cache_path) {
            if let Ok(data) = serde_json::from_str::<HashMap<String, Value>>(&raw) {
                return Some(data);
            }
        }
    }

    let response = reqwest::blocking::Client::new()
        .get("https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json")
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let raw = response.text().ok()?;
    let data = serde_json::from_str::<HashMap<String, Value>>(&raw).ok()?;
    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(cache_path, raw);
    Some(data)
}

fn pricing_cache_dir() -> PathBuf {
    if let Ok(path) = std::env::var("XDG_CACHE_HOME") {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            return path.join("bu_use").join("token_cost");
        }
    }
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".cache")
        .join("bu_use")
        .join("token_cost")
}

fn cache_file_fresh(path: &Path) -> bool {
    let Ok(modified) = path.metadata().and_then(|metadata| metadata.modified()) else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::MAX)
        < Duration::from_secs(24 * 60 * 60)
}

fn openai_error_message(body: &Value) -> String {
    body.get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| body.to_string())
}

fn anthropic_error_message(body: &Value) -> String {
    body.get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn fake_provider_returns_scripted_events() -> Result<()> {
        let provider = FakeProvider::with_text("hello");
        let events = provider.start_turn(ProviderTurn::default())?;
        assert_eq!(
            events,
            vec![
                ModelEvent::TextDelta {
                    text: "hello".to_string()
                },
                ModelEvent::Done
            ]
        );
        Ok(())
    }

    #[test]
    fn scripted_provider_returns_one_turn_at_a_time() -> Result<()> {
        let provider = ScriptedProvider::new(vec![
            vec![ModelEvent::TextDelta {
                text: "first".to_string(),
            }],
            vec![ModelEvent::TextDelta {
                text: "second".to_string(),
            }],
        ]);
        assert_eq!(provider.start_turn(ProviderTurn::default())?.len(), 1);
        let second = provider.start_turn(ProviderTurn::default())?;
        assert_eq!(
            second,
            vec![ModelEvent::TextDelta {
                text: "second".to_string()
            }]
        );
        assert_eq!(
            provider.start_turn(ProviderTurn::default())?,
            vec![ModelEvent::Done]
        );
        Ok(())
    }

    #[test]
    fn responses_input_preserves_user_image_parts() -> Result<()> {
        let input = messages_to_responses_input(&[json!({
            "role": "user",
            "content": [
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "data:image/png;base64,abc", "detail": "high"}
            ]
        })])?;
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert_eq!(
            input[0]["content"][1]["image_url"],
            "data:image/png;base64,abc"
        );
        assert_eq!(input[0]["content"][1]["detail"], "high");
        Ok(())
    }

    #[test]
    fn responses_input_moves_tool_output_images_to_visual_context_message() -> Result<()> {
        let input = messages_to_responses_input(&[
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "name": "python",
                    "arguments": {"code": "screenshot('x')"}
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_1",
                "name": "python",
                "content": [
                    {"type": "output_text", "text": "screenshot"},
                    {"type": "input_image", "image_url": "data:image/png;base64,abc", "detail": "auto"}
                ]
            }),
        ])?;
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_1");
        assert!(input[1]["output"]
            .as_str()
            .unwrap_or_default()
            .contains("following visual context message"));
        assert_eq!(input[2]["type"], "message");
        assert_eq!(input[2]["role"], "user");
        assert_eq!(input[2]["content"][0]["type"], "input_text");
        assert_eq!(input[2]["content"][1]["type"], "input_image");
        assert_eq!(
            input[2]["content"][1]["image_url"],
            "data:image/png;base64,abc"
        );
        assert_eq!(input[2]["content"][1]["detail"], "auto");
        Ok(())
    }

    #[test]
    fn responses_input_converts_orphan_tool_output_to_context_message() -> Result<()> {
        let input = messages_to_responses_input(&[
            json!({
                "role": "system",
                "content": "compacted context"
            }),
            json!({
                "role": "tool",
                "tool_call_id": "missing_call",
                "name": "python",
                "content": [
                    {"type": "output_text", "text": "screenshot"},
                    {"type": "input_image", "image_url": "data:image/png;base64,abc", "detail": "auto"}
                ]
            }),
        ])?;
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[1]["content"][0]["type"], "input_text");
        assert!(input[1]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Tool output retained as context"));
        assert_eq!(input[1]["content"][1]["type"], "input_image");
        assert!(!input.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                && item.get("call_id").and_then(Value::as_str) == Some("missing_call")
        }));
        Ok(())
    }

    #[test]
    fn responses_input_converts_system_messages_to_user_context() -> Result<()> {
        let input = messages_to_responses_input(&[json!({
            "role": "system",
            "content": "compact summary"
        })])?;
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(
            input[0]["content"][0]["text"],
            "System context:\ncompact summary"
        );
        Ok(())
    }

    #[test]
    fn loads_codex_auth_file_with_nested_tokens() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("auth.json");
        std::fs::write(
            &path,
            json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": "access-123",
                    "account_id": "account-123",
                    "refresh_token": "refresh-123"
                }
            })
            .to_string(),
        )?;
        assert_eq!(
            load_codex_auth_file(path)?,
            CodexAuth {
                access_token: "access-123".to_string(),
                account_id: "account-123".to_string(),
            }
        );
        Ok(())
    }

    #[test]
    fn codex_responses_provider_parses_sse_text_tool_call_and_usage() -> Result<()> {
        let sse = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Working\\n\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_123\",\"name\":\"done\",\"arguments\":\"{\\\"result\\\":\\\"ok\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":4,\"total_tokens\":7}}}\n\n",
        );
        let (base_url, handle) = spawn_mock_server(sse.to_string(), "text/event-stream")?;
        let provider = CodexResponsesProvider::with_base_url(
            CodexAuth {
                access_token: "chatgpt-token".to_string(),
                account_id: "account-123".to_string(),
            },
            "gpt-test",
            format!("{}/backend-api", base_url.trim_end_matches("/v1")),
        );
        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: vec![ToolSpec {
                name: "done".to_string(),
                description: "finish".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "result": { "type": "string" } },
                    "required": ["result"],
                    "additionalProperties": false
                }),
            }],
        })?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Working\n".to_string(),
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "call_123".to_string(),
                name: "done".to_string(),
                arguments: json!({"result": "ok"}),
            },
        }));
        assert!(events.contains(&ModelEvent::Usage {
            usage: ModelUsage {
                input_tokens: Some(3),
                output_tokens: Some(4),
                total_tokens: Some(7),
                cost_usd: None,
                ..Default::default()
            },
        }));
        assert!(events.contains(&ModelEvent::Done));
        Ok(())
    }

    #[test]
    fn openai_responses_provider_parses_text_tool_call_and_usage() -> Result<()> {
        let (base_url, handle) = spawn_mock_server(
            json!({
                "id": "resp_123",
                "object": "response",
                "output": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {
                                "type": "output_text",
                                "text": "Need the browser.\n"
                            }
                        ]
                    },
                    {
                        "type": "function_call",
                        "call_id": "call_123",
                        "name": "done",
                        "arguments": "{\"result\":\"ok\"}",
                        "status": "completed"
                    }
                ],
                "usage": {
                    "input_tokens": 11,
                    "output_tokens": 7,
                    "total_tokens": 18
                }
            })
            .to_string(),
            "application/json",
        )?;
        let provider = OpenAIResponsesProvider::with_base_url("test-key", "gpt-test", base_url);
        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({
                "role": "user",
                "content": "finish"
            })],
            tools: vec![ToolSpec {
                name: "done".to_string(),
                description: "finish".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "result": { "type": "string" }
                    },
                    "required": ["result"],
                    "additionalProperties": false
                }),
            }],
        })?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Need the browser.\n".to_string()
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "call_123".to_string(),
                name: "done".to_string(),
                arguments: json!({"result": "ok"}),
            }
        }));
        assert!(events.contains(&ModelEvent::Usage {
            usage: ModelUsage {
                input_tokens: Some(11),
                output_tokens: Some(7),
                total_tokens: Some(18),
                cost_usd: None,
                ..Default::default()
            }
        }));
        assert!(events.contains(&ModelEvent::Done));
        Ok(())
    }

    #[test]
    fn openai_compatible_chat_provider_parses_tool_call_and_usage() -> Result<()> {
        let (base_url, handle) = spawn_mock_server(
            json!({
                "id": "chatcmpl_123",
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "Need a tool.\n",
                        "tool_calls": [{
                            "id": "call_123",
                            "type": "function",
                            "function": {
                                "name": "done",
                                "arguments": "{\"result\":\"ok\"}"
                            }
                        }]
                    }
                }],
                "usage": {
                    "prompt_tokens": 5,
                    "completion_tokens": 6,
                    "total_tokens": 11,
                    "cost": 0.0123
                }
            })
            .to_string(),
            "application/json",
        )?;
        let provider =
            OpenAICompatibleChatProvider::with_base_url("test-key", "openrouter/test", base_url);
        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: vec![ToolSpec {
                name: "done".to_string(),
                description: "finish".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "result": { "type": "string" } },
                    "required": ["result"],
                    "additionalProperties": false
                }),
            }],
        })?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Need a tool.\n".to_string()
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "call_123".to_string(),
                name: "done".to_string(),
                arguments: json!({"result": "ok"}),
            }
        }));
        assert!(events.contains(&ModelEvent::Usage {
            usage: ModelUsage {
                input_tokens: Some(5),
                output_tokens: Some(6),
                total_tokens: Some(11),
                cost_usd: Some(0.0123),
                cost_source: Some("native".to_string()),
                ..Default::default()
            }
        }));
        Ok(())
    }

    #[test]
    fn anthropic_messages_provider_parses_text_tool_use_and_usage() -> Result<()> {
        let (base_url, handle) = spawn_mock_server(
            json!({
                "id": "msg_123",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "Working.\n" },
                    {
                        "type": "tool_use",
                        "id": "toolu_123",
                        "name": "done",
                        "input": { "result": "ok" }
                    }
                ],
                "usage": {
                    "input_tokens": 7,
                    "output_tokens": 8
                }
            })
            .to_string(),
            "application/json",
        )?;
        let provider =
            AnthropicMessagesProvider::with_base_url("anthropic-key", "claude-test", base_url);
        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: vec![ToolSpec {
                name: "done".to_string(),
                description: "finish".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "result": { "type": "string" } },
                    "required": ["result"],
                    "additionalProperties": false
                }),
            }],
        })?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::TextDelta {
            text: "Working.\n".to_string()
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "toolu_123".to_string(),
                name: "done".to_string(),
                arguments: json!({"result": "ok"}),
            }
        }));
        assert!(events.contains(&ModelEvent::Usage {
            usage: ModelUsage {
                input_tokens: Some(7),
                output_tokens: Some(8),
                total_tokens: Some(15),
                cost_usd: None,
                ..Default::default()
            }
        }));
        Ok(())
    }

    #[test]
    fn anthropic_messages_provider_accepts_oauth_auth_token() -> Result<()> {
        let (base_url, handle) = spawn_mock_server(
            json!({
                "id": "msg_123",
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "OAuth ok." },
                    {
                        "type": "tool_use",
                        "id": "toolu_bash",
                        "name": "Bash",
                        "input": { "cmd": "pwd" }
                    }
                ],
                "usage": { "input_tokens": 1, "output_tokens": 2 }
            })
            .to_string(),
            "application/json",
        )?;
        let provider = AnthropicMessagesProvider::with_auth_token(
            "claude-oauth-token",
            "claude-test",
            base_url,
        );
        let events = provider.start_turn(ProviderTurn {
            messages: vec![json!({"role": "user", "content": "finish"})],
            tools: vec![ToolSpec {
                name: "shell".to_string(),
                description: "run shell".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "cmd": { "type": "string" } },
                    "required": ["cmd"],
                    "additionalProperties": false
                }),
            }],
        })?;
        handle.join().expect("mock server thread");
        assert!(events.contains(&ModelEvent::TextDelta {
            text: "OAuth ok.".to_string()
        }));
        assert!(events.contains(&ModelEvent::ToolCall {
            call: ToolCall {
                id: "toolu_bash".to_string(),
                name: "shell".to_string(),
                arguments: json!({"cmd": "pwd"}),
            }
        }));
        assert!(events.contains(&ModelEvent::Usage {
            usage: ModelUsage {
                input_tokens: Some(1),
                output_tokens: Some(2),
                total_tokens: Some(3),
                cost_usd: None,
                ..Default::default()
            }
        }));
        Ok(())
    }

    #[test]
    fn claude_code_oauth_url_and_callback_parser_match_main_contract() {
        let (verifier, challenge) = claude_code_oauth_pkce();
        let url = claude_code_oauth_authorize_url(&verifier, &challenge);
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("user%3Asessions%3Aclaude_code"));
        let parsed = parse_claude_code_authorization_input(&format!(
            "http://localhost:53692/callback?code=abc123&state={verifier}"
        ));
        assert_eq!(parsed.code.as_deref(), Some("abc123"));
        assert_eq!(parsed.state.as_deref(), Some(verifier.as_str()));
        let parsed = parse_claude_code_authorization_input("abc123#state456");
        assert_eq!(parsed.code.as_deref(), Some("abc123"));
        assert_eq!(parsed.state.as_deref(), Some("state456"));
    }

    #[test]
    fn default_instructions_preserve_bitter_cdp_browser_harness_contract() {
        let instructions = default_instructions();
        for expected in [
            "bitter lesson",
            "Raw CDP is the center",
            "source of truth",
            "new_tab(url)",
            "not `goto_url(url)`",
            "Prefer coordinate clicks",
            "Chrome hit-testing handles iframes",
            "screenshot(\"label\")",
            "input_image",
            "agent_helpers.py",
            "Browser interaction tool",
            "Loaded Browser-Harness Interaction Skills",
            "interaction-skills/screenshots.md",
            "interaction-skills/tabs.md",
            "interaction-skills/dialogs.md",
            "Do not build manager layers",
            "Do not import or install Playwright",
        ] {
            assert!(
                instructions.contains(expected),
                "missing {expected:?} from default instructions:\n{instructions}"
            );
        }
    }

    fn spawn_mock_server(
        body: String,
        content_type: &'static str,
    ) -> Result<(String, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buf).expect("read request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request_text = String::from_utf8_lossy(&request);
            let request_text_lower = request_text.to_ascii_lowercase();
            assert!(
                request_text.starts_with("POST /v1/responses ")
                    || request_text.starts_with("POST /backend-api/codex/responses ")
                    || request_text.starts_with("POST /v1/chat/completions ")
                    || request_text.starts_with("POST /v1/messages ")
            );
            assert!(
                request_text_lower.contains("authorization: bearer test-key")
                    || request_text_lower.contains("authorization: bearer chatgpt-token")
                    || request_text_lower.contains("authorization: bearer claude-oauth-token")
                    || request_text_lower.contains("x-api-key: anthropic-key")
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });
        Ok((format!("http://{addr}/v1"), handle))
    }
}
