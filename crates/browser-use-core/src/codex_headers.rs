//! Codex request header/metadata builders extracted from `lib.rs` (Phase 0.1 carve).
//!
//! Code motion only — behavior is byte-identical to the original definitions.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use browser_use_protocol::SessionMeta;
use browser_use_store::Store;
use serde_json::Value;

use crate::constants::*;
use crate::{load_agents_md_config_for_options, AgentRunOptions, CodexTurnLifecycle};

pub(crate) fn codex_responses_extra_headers(
    store: &Store,
    session: &SessionMeta,
    options: &AgentRunOptions,
    turn_metadata_header: Option<&str>,
) -> Result<HashMap<String, String>> {
    let mut headers = HashMap::new();
    headers.insert(CODEX_HTTP_SESSION_ID_HEADER.to_string(), session.id.clone());
    headers.insert(CODEX_HTTP_THREAD_ID_HEADER.to_string(), session.id.clone());
    headers.insert(
        CODEX_HTTP_CLIENT_REQUEST_ID_HEADER.to_string(),
        session.id.clone(),
    );
    headers.insert(
        CODEX_WINDOW_ID_HEADER.to_string(),
        stable_codex_window_id(store)?,
    );
    if let Some(parent_id) = session.parent_id.as_deref().filter(|id| !id.is_empty()) {
        headers.insert(
            OPENAI_SUBAGENT_HEADER.to_string(),
            OPENAI_SUBAGENT_COLLAB_SPAWN.to_string(),
        );
        headers.insert(
            CODEX_PARENT_THREAD_ID_HEADER.to_string(),
            parent_id.to_string(),
        );
    }
    if let Some(turn_metadata_header) =
        turn_metadata_header.filter(|value| !value.trim().is_empty())
    {
        headers.insert(
            CODEX_TURN_METADATA_HEADER.to_string(),
            turn_metadata_header.to_string(),
        );
    }
    if let Some(beta_features_header) = codex_beta_features_header_for_session(session, options)? {
        headers.insert(CODEX_BETA_FEATURES_HEADER.to_string(), beta_features_header);
    }
    Ok(headers)
}

fn codex_beta_features_header_for_session(
    session: &SessionMeta,
    options: &AgentRunOptions,
) -> Result<Option<String>> {
    let mut warnings = Vec::new();
    let config =
        load_agents_md_config_for_options(Path::new(&session.cwd), &mut warnings, options)?;
    Ok(config.beta_features_header())
}

pub(crate) fn codex_responses_turn_metadata_header(
    session: &SessionMeta,
    lifecycle: &CodexTurnLifecycle,
) -> Result<String> {
    let thread_source = if session
        .parent_id
        .as_deref()
        .is_some_and(|parent_id| !parent_id.is_empty())
    {
        "subagent"
    } else {
        "user"
    };
    let metadata = serde_json::json!({
        "session_id": session.id.clone(),
        "thread_id": session.id.clone(),
        "thread_source": thread_source,
        "turn_id": lifecycle.turn_id.clone(),
        "sandbox": "none",
        "turn_started_at_unix_ms": lifecycle.started_at_ms,
    });
    json_to_ascii_string(&metadata)
}

fn json_to_ascii_string(value: &Value) -> Result<String> {
    let raw = serde_json::to_string(value)?;
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii() {
            out.push(ch);
            continue;
        }
        let code = ch as u32;
        if code <= 0xffff {
            write!(&mut out, "\\u{code:04x}")?;
        } else {
            let code = code - 0x1_0000;
            let high = 0xd800 + ((code >> 10) & 0x3ff);
            let low = 0xdc00 + (code & 0x3ff);
            write!(&mut out, "\\u{high:04x}\\u{low:04x}")?;
        }
    }
    Ok(out)
}

pub(crate) fn codex_responses_client_metadata(store: &Store) -> Result<HashMap<String, String>> {
    Ok(HashMap::from([(
        CODEX_INSTALLATION_ID_HEADER.to_string(),
        stable_codex_installation_id(store)?,
    )]))
}

pub(crate) fn provider_uses_codex_request_metadata(provider_name: &str) -> bool {
    provider_name == "codex"
}

pub(crate) fn provider_uses_openai_prompt_cache_key(provider_name: &str) -> bool {
    matches!(provider_name, "codex" | "openai")
}

fn stable_codex_installation_id(store: &Store) -> Result<String> {
    if let Some(installation_id) = store.get_setting(CODEX_INSTALLATION_ID_SETTING)? {
        if !installation_id.trim().is_empty() {
            return Ok(installation_id);
        }
    }
    let installation_id = uuid::Uuid::new_v4().to_string();
    store.set_setting(CODEX_INSTALLATION_ID_SETTING, &installation_id)?;
    Ok(installation_id)
}

fn stable_codex_window_id(store: &Store) -> Result<String> {
    if let Some(window_id) = store.get_setting(CODEX_WINDOW_ID_SETTING)? {
        if !window_id.trim().is_empty() {
            return Ok(window_id);
        }
    }
    let window_id = uuid::Uuid::new_v4().to_string();
    store.set_setting(CODEX_WINDOW_ID_SETTING, &window_id)?;
    Ok(window_id)
}
