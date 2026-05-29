//! Provider construction and credential management extracted from `lib.rs` (Phase 0.1 carve).
//!
//! Code motion only — behavior is byte-identical to the original definitions.

use std::path::PathBuf;

use anyhow::{Context, Result};
use browser_use_providers::{
    load_codex_auth, load_codex_auth_file, refresh_claude_code_oauth, AnthropicMessagesProvider,
    ClaudeCodeOAuthCredential, CodexAuth, CodexManagedAuth, CodexResponsesProvider,
    OpenAICompatibleChatProvider, OpenAIResponsesProvider,
};
use browser_use_store::{now_ms, Store};

pub(crate) fn openai_provider(store: &Store, model: String) -> Result<OpenAIResponsesProvider> {
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

pub(crate) fn codex_provider(store: &Store, model: String) -> Result<CodexResponsesProvider> {
    let base_url = setting_or_env_or_default(
        store,
        "auth.codex.base_url",
        &["LLM_BROWSER_CODEX_BASE_URL"],
        "https://chatgpt.com/backend-api",
    )?;
    if let Some(auth) = stored_codex_managed_auth(store)? {
        return CodexResponsesProvider::with_managed_base_url(auth, model, base_url);
    }
    let auth = stored_codex_auth(store)?
        .or_else(codex_auth_from_explicit_env)
        .map(Ok)
        .unwrap_or_else(load_codex_auth)?;
    Ok(CodexResponsesProvider::with_base_url(auth, model, base_url))
}

fn codex_auth_from_explicit_env() -> Option<CodexAuth> {
    if let Ok(path) = std::env::var("LLM_BROWSER_CODEX_AUTH_FILE") {
        let path = path.trim();
        if !path.is_empty() {
            return load_codex_auth_file(path).ok();
        }
    }
    let access_token = std::env::var("LLM_BROWSER_CODEX_ACCESS_TOKEN").ok()?;
    let account_id = std::env::var("LLM_BROWSER_CODEX_ACCOUNT_ID").ok()?;
    if access_token.trim().is_empty() || account_id.trim().is_empty() {
        return None;
    }
    Some(CodexAuth {
        access_token,
        account_id,
    })
}

pub(crate) fn anthropic_provider(store: &Store, model: String) -> Result<AnthropicMessagesProvider> {
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
        return match claude_code_provider_auth(store)? {
            ClaudeCodeProviderAuth::Refreshable(credential) => {
                let state_dir = store.state_dir().to_path_buf();
                Ok(
                    AnthropicMessagesProvider::with_claude_code_oauth_persistence(
                        credential,
                        model,
                        base_url,
                        move |credential| {
                            let store = Store::open(&state_dir)?;
                            store_claude_code_oauth(&store, credential)
                        },
                    ),
                )
            }
            ClaudeCodeProviderAuth::StaticToken(auth_token) => Ok(
                AnthropicMessagesProvider::with_auth_token(auth_token, model, base_url),
            ),
        };
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

pub(crate) enum ClaudeCodeProviderAuth {
    Refreshable(ClaudeCodeOAuthCredential),
    StaticToken(String),
}

pub(crate) fn claude_code_provider_auth(store: &Store) -> Result<ClaudeCodeProviderAuth> {
    if let Some(refresh_token) = store.get_setting("auth.claude_code.refresh_token")? {
        let refresh_token = refresh_token.trim().to_string();
        if !refresh_token.is_empty() {
            let access_token = store
                .get_setting("auth.claude_code.access_token")?
                .unwrap_or_default();
            let expires_ms = store
                .get_setting("auth.claude_code.expires_ms")?
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0);
            if access_token.trim().is_empty() || expires_ms <= now_ms() + 60_000 {
                let credential = refresh_claude_code_oauth(&refresh_token)
                    .context("refresh Claude Code OAuth token")?;
                store_claude_code_oauth(store, &credential)?;
                return Ok(ClaudeCodeProviderAuth::Refreshable(credential));
            }
            return Ok(ClaudeCodeProviderAuth::Refreshable(
                ClaudeCodeOAuthCredential {
                    access_token: access_token.trim().to_string(),
                    refresh_token,
                    expires_ms,
                },
            ));
        }
    }
    Ok(ClaudeCodeProviderAuth::StaticToken(
        claude_code_access_token(store)?,
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

pub(crate) fn openrouter_provider(store: &Store, model: String) -> Result<OpenAICompatibleChatProvider> {
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

pub(crate) fn deepseek_provider(store: &Store, model: String) -> Result<OpenAICompatibleChatProvider> {
    let api_key = stored_or_env(
        store,
        "auth.deepseek.api_key",
        &["LLM_BROWSER_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"],
    )?
    .context("run `auth login deepseek --api-key ...` or set DEEPSEEK_API_KEY")?;
    let base_url = setting_or_env_or_default(
        store,
        "auth.deepseek.base_url",
        &["LLM_BROWSER_DEEPSEEK_BASE_URL"],
        "https://api.deepseek.com",
    )?;
    Ok(OpenAICompatibleChatProvider::deepseek(
        api_key, model, base_url,
    ))
}

pub(crate) fn stored_codex_auth(store: &Store) -> Result<Option<CodexAuth>> {
    let Some(access_token) = store.get_setting("auth.codex.access_token")? else {
        return Ok(None);
    };
    let Some(account_id) = store.get_setting("auth.codex.account_id")? else {
        return Ok(None);
    };
    if access_token.trim().is_empty() || account_id.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(CodexAuth::new(access_token, account_id)))
}

pub(crate) fn stored_codex_managed_auth(store: &Store) -> Result<Option<CodexManagedAuth>> {
    let Some(auth) = stored_codex_auth(store)? else {
        return Ok(None);
    };
    let source_path = store
        .get_setting("auth.codex.source_path")?
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from);
    let refresh_token = store
        .get_setting("auth.codex.refresh_token")?
        .filter(|value| !value.trim().is_empty());
    if source_path.is_none() && refresh_token.is_none() {
        return Ok(None);
    }
    let id_token = store
        .get_setting("auth.codex.id_token")?
        .filter(|value| !value.trim().is_empty());
    let last_refresh = store.get_setting("auth.codex.last_refresh")?;
    Ok(Some(CodexManagedAuth::from_stored_parts(
        auth.access_token,
        auth.account_id,
        id_token,
        refresh_token,
        source_path,
        last_refresh,
    )))
}

pub(crate) fn stored_or_env(store: &Store, setting_key: &str, env_names: &[&str]) -> Result<Option<String>> {
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

pub(crate) fn setting_or_env_or_default(
    store: &Store,
    setting_key: &str,
    env_names: &[&str],
    default: &str,
) -> Result<String> {
    Ok(stored_or_env(store, setting_key, env_names)?.unwrap_or_else(|| default.to_string()))
}
