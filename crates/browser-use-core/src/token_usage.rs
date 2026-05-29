//! Codex token-usage accounting helpers extracted from `lib.rs` (Phase 0.1 carve).
//!
//! Code motion only — behavior is byte-identical to the original definitions.

use browser_use_protocol::EventRecord;
use serde_json::Value;

use crate::constants::*;
use crate::json_payload_i64;

pub(crate) fn latest_codex_total_token_usage_from_events(events: &[EventRecord]) -> Value {
    events
        .iter()
        .rev()
        .filter_map(|event| {
            (event.event_type == CODEX_TOKEN_COUNT_EVENT)
                .then(|| event.payload.get("info"))
                .flatten()
                .and_then(|info| info.get("total_token_usage"))
                .cloned()
        })
        .next()
        .unwrap_or_else(empty_codex_token_usage)
}

pub(crate) fn model_usage_to_codex_token_usage(usage: &browser_use_protocol::ModelUsage) -> Value {
    let input_tokens = usage.input_tokens.unwrap_or_default();
    let cached_input_tokens = usage.input_cached_tokens.unwrap_or_default();
    let output_tokens = usage.output_tokens.unwrap_or_default();
    let reasoning_output_tokens = usage.reasoning_output_tokens.unwrap_or_default();
    let total_tokens = usage
        .total_tokens
        .unwrap_or(input_tokens + output_tokens + reasoning_output_tokens);
    serde_json::json!({
        "input_tokens": input_tokens,
        "cached_input_tokens": cached_input_tokens,
        "output_tokens": output_tokens,
        "reasoning_output_tokens": reasoning_output_tokens,
        "total_tokens": total_tokens,
    })
}

pub(crate) fn empty_codex_token_usage() -> Value {
    serde_json::json!({
        "input_tokens": 0,
        "cached_input_tokens": 0,
        "output_tokens": 0,
        "reasoning_output_tokens": 0,
        "total_tokens": 0,
    })
}

pub(crate) fn add_codex_token_usage(left: &Value, right: &Value) -> Value {
    serde_json::json!({
        "input_tokens": json_payload_i64(left, "input_tokens") + json_payload_i64(right, "input_tokens"),
        "cached_input_tokens": json_payload_i64(left, "cached_input_tokens") + json_payload_i64(right, "cached_input_tokens"),
        "output_tokens": json_payload_i64(left, "output_tokens") + json_payload_i64(right, "output_tokens"),
        "reasoning_output_tokens": json_payload_i64(left, "reasoning_output_tokens") + json_payload_i64(right, "reasoning_output_tokens"),
        "total_tokens": json_payload_i64(left, "total_tokens") + json_payload_i64(right, "total_tokens"),
    })
}

pub(crate) fn subtract_codex_token_usage(left: &Value, right: &Value) -> Value {
    serde_json::json!({
        "input_tokens": (json_payload_i64(left, "input_tokens") - json_payload_i64(right, "input_tokens")).max(0),
        "cached_input_tokens": (json_payload_i64(left, "cached_input_tokens") - json_payload_i64(right, "cached_input_tokens")).max(0),
        "output_tokens": (json_payload_i64(left, "output_tokens") - json_payload_i64(right, "output_tokens")).max(0),
        "reasoning_output_tokens": (json_payload_i64(left, "reasoning_output_tokens") - json_payload_i64(right, "reasoning_output_tokens")).max(0),
        "total_tokens": (json_payload_i64(left, "total_tokens") - json_payload_i64(right, "total_tokens")).max(0),
    })
}
