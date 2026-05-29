//! Tool-output message formatting and artifact spilling extracted from `lib.rs`
//! (Phase 0.1 carve).
//!
//! Code motion only — behavior is byte-identical to the original definitions.

use std::path::Path;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use browser_use_protocol::ToolCall;
use browser_use_python_worker::RunPythonResponse;
use browser_use_store::{now_ms, Store};
use serde_json::Value;

use crate::constants::*;
use crate::{approx_token_count, record_model_response_input_item, tools};

pub(crate) fn tool_json_message(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
    name: &str,
    content: Value,
) -> Result<Value> {
    let content = serde_json::to_string(&content)?;
    tool_text_message(
        store,
        session,
        call,
        name,
        &content,
        DEFAULT_TOOL_OUTPUT_TEXT_TOKENS,
    )
}

pub(crate) fn tool_text_message(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
    name: &str,
    content: &str,
    tool_output_token_budget: usize,
) -> Result<Value> {
    let content = spill_large_tool_text(
        store,
        &session.id,
        Some(&call.id),
        name,
        content,
        tool_output_token_budget,
    )?;
    let content_value = Value::String(content);
    record_model_response_input_item(store, &session.id, call, name, &content_value)?;
    Ok(serde_json::json!({
        "role": "tool",
        "tool_call_id": call.id,
        "name": name,
        "content": content_value,
    }))
}

pub(crate) fn tool_content_message(
    store: &Store,
    session: &browser_use_protocol::SessionMeta,
    call: &ToolCall,
    name: &str,
    content: Value,
    tool_output_token_budget: usize,
) -> Result<Value> {
    let content = spill_large_tool_content(
        store,
        &session.id,
        Some(&call.id),
        name,
        content,
        tool_output_token_budget,
    )?;
    record_model_response_input_item(store, &session.id, call, name, &content)?;
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
    tool_output_token_budget: usize,
) -> Result<Value> {
    match content {
        Value::String(text) => Ok(Value::String(spill_large_tool_text(
            store,
            session_id,
            call_id,
            tool_name,
            &text,
            tool_output_token_budget,
        )?)),
        Value::Array(items) => spill_large_tool_content_items(
            store,
            session_id,
            call_id,
            tool_name,
            items,
            tool_output_token_budget,
        ),
        other => {
            let serialized = serde_json::to_string(&other)?;
            if approx_token_count(&serialized)
                <= tool_output_serialization_token_budget(tool_output_token_budget)
            {
                Ok(other)
            } else {
                Ok(Value::String(spill_large_tool_text(
                    store,
                    session_id,
                    call_id,
                    tool_name,
                    &serialized,
                    tool_output_token_budget,
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
    tool_output_token_budget: usize,
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
    if approx_token_count(&combined_text)
        <= tool_output_serialization_token_budget(tool_output_token_budget)
    {
        return Ok(Value::Array(items));
    }

    let preview = spill_large_tool_text(
        store,
        session_id,
        call_id,
        tool_name,
        &combined_text,
        tool_output_token_budget,
    )?;
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
    tool_output_token_budget: usize,
) -> Result<String> {
    if approx_token_count(text) <= tool_output_serialization_token_budget(tool_output_token_budget)
    {
        return Ok(text.to_string());
    }
    let artifact = write_tool_output_artifact(
        store,
        session_id,
        tool_name,
        call_id,
        text,
        tool_output_token_budget,
    )?;
    Ok(spilled_tool_output_preview(
        text,
        &artifact,
        tool_output_token_budget,
    ))
}

fn spilled_tool_output_preview(
    text: &str,
    _artifact: &Value,
    tool_output_token_budget: usize,
) -> String {
    tools::command::codex_formatted_truncate_text(text, tool_output_token_budget)
}

pub(crate) fn write_tool_output_artifact(
    store: &Store,
    session_id: &str,
    tool_name: &str,
    call_id: Option<&str>,
    text: &str,
    tool_output_token_budget: usize,
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
        "truncated_tokens": tool_output_token_budget,
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

pub(crate) fn tool_output_serialization_token_budget(tool_output_token_budget: usize) -> usize {
    tool_output_token_budget.saturating_mul(6).div_ceil(5)
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
    tool_output_token_budget: usize,
) -> String {
    if response.ok {
        let mut parts = Vec::new();
        if !response.text.trim().is_empty() {
            let text = response.text.trim();
            let text = if approx_token_count(text) > tool_output_token_budget {
                text_artifact
                    .map(|artifact| {
                        spilled_tool_output_preview(text, artifact, tool_output_token_budget)
                    })
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

pub(crate) fn python_tool_message_content_value(
    response: &RunPythonResponse,
    text_artifact: Option<&Value>,
    tool_output_token_budget: usize,
) -> Result<Value> {
    let text = python_tool_message_content(response, text_artifact, tool_output_token_budget);
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

pub(crate) fn browser_script_tool_message_content(
    response: &browser_use_browser::BrowserScriptOutput,
) -> String {
    if response.status.as_deref() == Some("running") {
        return browser_script_running_message(response);
    }
    if response.status.as_deref() == Some("cancelled") {
        return browser_script_cancelled_message(response);
    }
    if response.ok {
        let mut parts = Vec::new();
        if !response.text.trim().is_empty() {
            parts.push(response.text.trim().to_string());
        }
        parts.extend(browser_script_structured_message_parts(response));
        if parts.is_empty() {
            "browser_script completed".to_string()
        } else {
            parts.join("\n")
        }
    } else {
        browser_script_failure_message(response)
    }
}

fn browser_script_running_message(response: &browser_use_browser::BrowserScriptOutput) -> String {
    let mut parts = Vec::new();
    if !response.text.trim().is_empty() {
        parts.push(response.text.trim().to_string());
    } else {
        parts.push("browser_script is still running.".to_string());
    }
    if let Some(run_id) = response.run_id.as_deref() {
        if !parts
            .iter()
            .any(|part| part.contains(&format!("run_id: {run_id}")))
        {
            parts.push(format!("run_id: {run_id}"));
        }
        parts.push(format!(
            "Next step: call browser_script with action=\"observe\", run_id=\"{run_id}\", and observe_timeout_ms={}.",
            response.next_observe_ms.unwrap_or(1_000)
        ));
    }
    parts.extend(browser_script_structured_message_parts(response));
    parts.join("\n")
}

fn browser_script_cancelled_message(response: &browser_use_browser::BrowserScriptOutput) -> String {
    let mut parts = Vec::new();
    if response.text.trim().is_empty() {
        parts.push("browser_script cancelled.".to_string());
    } else {
        parts.push(response.text.trim().to_string());
    }
    parts.extend(browser_script_structured_message_parts(response));
    parts.join("\n")
}

fn browser_script_failure_message(response: &browser_use_browser::BrowserScriptOutput) -> String {
    let mut parts = vec!["browser_script failed.".to_string()];
    if let Some(diagnosis) = response.diagnosis.as_ref() {
        parts.push(diagnosis.summary.clone());
        parts.push(format!("What happened: {}", diagnosis.what_happened));
        parts.push(format!("Next step: {}", diagnosis.next_step));
    }
    if let Some(error) = response.error.as_deref() {
        let detail = browser_script_error_detail(error);
        if !detail.is_empty() {
            parts.push(format!("Details: {detail}"));
        }
    } else if response.diagnosis.is_none() {
        parts.push("Details: unknown browser_script error".to_string());
    }
    parts.extend(browser_script_structured_message_parts(response));
    parts.join("\n")
}

fn browser_script_structured_message_parts(
    response: &browser_use_browser::BrowserScriptOutput,
) -> Vec<String> {
    let mut parts = Vec::new();
    if !response.outputs.is_empty() {
        parts.push(format!(
            "outputs: {}",
            Value::Array(response.outputs.clone())
        ));
    }
    if !response.summary.is_empty() {
        parts.push(format!(
            "summary: {}",
            Value::Array(response.summary.clone())
        ));
    }
    if !response.data.is_null() && response.data != serde_json::json!({}) {
        parts.push(format!("data: {}", response.data));
    }
    parts
}

fn browser_script_error_detail(error: &str) -> String {
    const MAX_DETAIL_CHARS: usize = 500;
    let detail = error
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(error.trim());
    if detail.chars().count() <= MAX_DETAIL_CHARS {
        return detail.to_string();
    }
    let mut out = detail
        .chars()
        .take(MAX_DETAIL_CHARS.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

pub(crate) fn browser_script_tool_message_content_value(
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
    if response.images.is_empty() {
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
