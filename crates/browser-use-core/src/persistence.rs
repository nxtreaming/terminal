//! Persistence helpers: recording Python/browser worker outputs into the `Store`
//! (Phase 0.1 carve).
//!
//! Code motion only — behavior is byte-identical to the original definitions.

use anyhow::Result;
use browser_use_python_worker::{PythonWorkerEvent, RunPythonResponse};
use browser_use_store::Store;
use serde_json::Value;

use crate::constants::*;
use crate::{approx_token_count, tools, write_tool_output_artifact};

pub fn record_python_response_events(
    store: &Store,
    session_id: &str,
    response: &RunPythonResponse,
) -> Result<Option<Value>> {
    record_python_response_events_inner(
        store,
        session_id,
        response,
        true,
        DEFAULT_TOOL_OUTPUT_TEXT_TOKENS,
    )
}

pub fn record_python_response_final_event(
    store: &Store,
    session_id: &str,
    response: &RunPythonResponse,
) -> Result<Option<Value>> {
    record_python_response_final_event_with_budget(
        store,
        session_id,
        response,
        DEFAULT_TOOL_OUTPUT_TEXT_TOKENS,
    )
}

pub(crate) fn record_python_response_final_event_with_budget(
    store: &Store,
    session_id: &str,
    response: &RunPythonResponse,
    tool_output_token_budget: usize,
) -> Result<Option<Value>> {
    record_python_response_events_inner(
        store,
        session_id,
        response,
        false,
        tool_output_token_budget,
    )
}

pub fn record_browser_script_response_events(
    store: &Store,
    session_id: &str,
    tool_call_id: &str,
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
        record_tool_image_with_call_id(
            store,
            session_id,
            "browser_script",
            Some(tool_call_id),
            image,
        )?;
    }
    for artifact in &response.artifacts {
        let Some(path) = artifact.get("path").and_then(Value::as_str) else {
            continue;
        };
        if image_paths.contains(path) {
            continue;
        }
        record_tool_artifact_with_call_id(
            store,
            session_id,
            "browser_script",
            Some(tool_call_id),
            artifact,
        )?;
    }
    let transcript_text = browser_script_transcript_text(response);
    store.append_event(
        session_id,
        "tool.output",
        serde_json::json!({
            "name": "browser_script",
            "tool_call_id": tool_call_id,
            "ok": response.ok,
            "status": response.status,
            "run_id": response.run_id,
            "next_observe_ms": response.next_observe_ms,
            "text": transcript_text,
            "data": response.data,
            "outputs": response.outputs,
            "summary": response.summary,
            "images": response.images,
            "artifacts": response.artifacts,
            "error": response.error,
            "diagnosis": response.diagnosis,
        }),
    )?;
    Ok(())
}

fn browser_script_transcript_text(response: &browser_use_browser::BrowserScriptOutput) -> String {
    if response.text.trim().is_empty() {
        return String::new();
    }
    response
        .text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !is_browser_script_transport_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_browser_script_transport_line(line: &str) -> bool {
    let line = line.trim_start();
    line == "browser_script is still running."
        || line.starts_with("run_id:")
        || line.starts_with("Next:")
        || line.starts_with("Next step:")
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
    tool_output_token_budget: usize,
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

    let (text, text_artifact) =
        spill_large_text_output(store, session_id, &response.text, tool_output_token_budget)?;
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
    tool_output_token_budget: usize,
) -> Result<(String, Option<Value>)> {
    if approx_token_count(text) <= tool_output_token_budget {
        return Ok((text.to_string(), None));
    }
    let artifact = write_tool_output_artifact(
        store,
        session_id,
        "python",
        None,
        text,
        tool_output_token_budget,
    )?;
    Ok((
        tools::command::codex_formatted_truncate_text(text, tool_output_token_budget),
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

pub(crate) fn record_python_browser_event(
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
    record_tool_image_with_call_id(store, session_id, name, None, image)
}

fn record_tool_image_with_call_id(
    store: &Store,
    session_id: &str,
    name: &str,
    tool_call_id: Option<&str>,
    image: &Value,
) -> Result<()> {
    let mut payload = serde_json::json!({
        "name": name,
        "image": image,
    });
    if let Some(tool_call_id) = tool_call_id {
        payload["tool_call_id"] = Value::String(tool_call_id.to_string());
    }
    let event = store.append_event(session_id, "tool.image", payload)?;
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

pub(crate) fn record_tool_artifact(
    store: &Store,
    session_id: &str,
    name: &str,
    artifact: &Value,
) -> Result<()> {
    record_tool_artifact_with_call_id(store, session_id, name, None, artifact)
}

fn record_tool_artifact_with_call_id(
    store: &Store,
    session_id: &str,
    name: &str,
    tool_call_id: Option<&str>,
    artifact: &Value,
) -> Result<()> {
    let Some(path) = artifact.get("path").and_then(Value::as_str) else {
        return Ok(());
    };
    let kind = artifact
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("file");
    let mut payload = serde_json::json!({
        "name": name,
        "artifact": artifact,
    });
    if let Some(tool_call_id) = tool_call_id {
        payload["tool_call_id"] = Value::String(tool_call_id.to_string());
    }
    let event = store.append_event(session_id, "artifact.created", payload)?;
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
