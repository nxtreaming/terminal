//! Per-item byte/token estimation + prompt assembly + normalization.
//!
//! `Item = serde_json::Value` (legacy provider-message currency). Ported from
//! codex `context_manager/{history,normalize}.rs`, cross-checked against legacy
//! `browser-use-core/src/lib.rs` (which already operates on `Value`):
//!   * `estimate_item_model_visible_bytes` <-> codex
//!     `estimate_response_item_model_visible_bytes` / legacy lib.rs:4215:
//!     reasoning/compaction items use `estimate_reasoning_length(text_len)`;
//!     image data-urls swap their raw `url.len()` for
//!     `estimate_image_context_bytes`; encrypted function outputs swap their
//!     encrypted-blob length for `estimate_encrypted_function_output_length`.
//!   * `process_item` applies a `policy * 1.2` budget to oversized tool outputs
//!     (codex history.rs:378).
//!   * `for_prompt` = `ensure_call_outputs_present` + `remove_orphan_outputs` +
//!     `strip_images_when_unsupported` (codex history.rs:359).
//!   * normalization helpers <-> codex `normalize.rs`.
//!   * `total_token_usage` branches on `server_reasoning_included` <-> codex
//!     `get_total_token_usage` (history.rs:292-332).

use std::collections::HashSet;

use serde_json::Value;

use super::accounting::{
    self, approx_tokens_from_byte_count_i64, estimate_encrypted_function_output_length,
    estimate_reasoning_length, TokenUsageInfo,
};
use super::image_estimate::estimate_image_context_bytes;
use super::Item;

/// Synthetic output content codex inserts for a call that never produced one.
/// Ground: codex `normalize.rs::ensure_call_outputs_present` (`"aborted"`).
const ABORTED_OUTPUT_CONTENT: &str = "aborted";

/// Elision marker appended to truncated text.
const TRUNCATION_NOTE: &str = "\n[output truncated]";

// ---------------------------------------------------------------------------
// Item shape helpers.
// ---------------------------------------------------------------------------

fn item_type(item: &Item) -> Option<&str> {
    item.get("type").and_then(Value::as_str)
}

fn item_role(item: &Item) -> Option<&str> {
    item.get("role").and_then(Value::as_str)
}

/// The call id linking a function/custom/local-shell call to its output.
fn item_call_id(item: &Item) -> Option<&str> {
    item.get("call_id").and_then(Value::as_str)
}

fn is_function_call(item: &Item) -> bool {
    matches!(
        item_type(item),
        Some("function_call") | Some("custom_tool_call") | Some("local_shell_call")
    )
}

fn is_function_call_output(item: &Item) -> bool {
    matches!(
        item_type(item),
        Some("function_call_output") | Some("custom_tool_call_output")
    )
}

fn is_reasoning_item(item: &Item) -> bool {
    matches!(item_type(item), Some("reasoning") | Some("compaction"))
}

// ---------------------------------------------------------------------------
// Byte / token estimation.
// ---------------------------------------------------------------------------

/// Estimate the model-visible byte size of one history item.
///
/// Ground: codex `estimate_response_item_model_visible_bytes` / legacy
/// lib.rs:4215.
pub fn estimate_item_model_visible_bytes(i: &Item) -> i64 {
    // Reasoning / compaction items: cost a function of their textual length.
    if is_reasoning_item(i) {
        let text_len = reasoning_text_len(i);
        return i64::try_from(estimate_reasoning_length(text_len)).unwrap_or(i64::MAX);
    }

    let raw = serde_json::to_string(i)
        .map(|s| i64::try_from(s.len()).unwrap_or(i64::MAX))
        .unwrap_or_default();

    let (image_payload, image_replacement) = image_data_url_estimate_adjustment(i);
    let (enc_payload, enc_replacement) = encrypted_function_output_estimate_adjustment(i);

    let raw = raw
        .saturating_sub(image_payload)
        .saturating_add(image_replacement);
    raw.saturating_sub(enc_payload)
        .saturating_add(enc_replacement)
}

/// Token-count estimate for one item (bytes -> tokens).
pub fn estimate_item_token_count(i: &Item) -> i64 {
    approx_tokens_from_byte_count_i64(estimate_item_model_visible_bytes(i))
}

/// Sum of the lengths of reasoning summary/text content.
///
/// Ground: legacy `estimate_reasoning_text_length`.
fn reasoning_text_len(item: &Item) -> usize {
    let mut len = 0usize;
    if let Some(s) = item.get("text").and_then(Value::as_str) {
        len += s.len();
    }
    if let Some(arr) = item.get("summary").and_then(Value::as_array) {
        for part in arr {
            if let Some(s) = part.get("text").and_then(Value::as_str) {
                len += s.len();
            }
        }
    }
    len
}

/// For each inline image data URL, return the summed (raw `url.len()` payload,
/// replacement estimate) pair.
///
/// Ground: legacy `image_data_url_estimate_adjustment`.
fn image_data_url_estimate_adjustment(item: &Item) -> (i64, i64) {
    let mut payload_total: i64 = 0;
    let mut replacement_total: i64 = 0;
    for url in collect_image_data_urls(item) {
        let payload_len = url.len() as i64;
        let replacement = estimate_image_context_bytes(&url);
        payload_total = payload_total.saturating_add(payload_len);
        replacement_total = replacement_total.saturating_add(replacement);
    }
    (payload_total, replacement_total)
}

/// Collect inline `data:` image URLs from an item's `content` array.
///
/// Ground: legacy `collect_image_data_urls`.
fn collect_image_data_urls(item: &Item) -> Vec<String> {
    let mut urls = Vec::new();
    let Some(content) = item.get("content").and_then(Value::as_array) else {
        return urls;
    };
    for part in content {
        // Responses API: {"type":"input_image","image_url":"data:..."}
        if let Some(url) = part.get("image_url").and_then(Value::as_str) {
            if url.starts_with("data:") {
                urls.push(url.to_string());
            }
        }
        // Chat API nests under image_url.url.
        if let Some(url) = part
            .get("image_url")
            .and_then(|v| v.get("url"))
            .and_then(Value::as_str)
        {
            if url.starts_with("data:") {
                urls.push(url.to_string());
            }
        }
    }
    urls
}

/// For an encrypted function-call output, return (raw encrypted-blob length,
/// estimated decrypted length).
///
/// Ground: legacy `encrypted_function_output_estimate_adjustment`.
fn encrypted_function_output_estimate_adjustment(item: &Item) -> (i64, i64) {
    let Some(output) = item.get("output") else {
        return (0, 0);
    };
    let Some(encrypted) = output.get("encrypted_content").and_then(Value::as_str) else {
        return (0, 0);
    };
    let payload = encrypted.len() as i64;
    let replacement = estimate_encrypted_function_output_length(encrypted.len()) as i64;
    (payload, replacement)
}

// ---------------------------------------------------------------------------
// Classification predicates (legacy lib.rs:4183-4213).
// ---------------------------------------------------------------------------

/// API messages: every non-system item.
///
/// Ground: legacy `is_api_message`.
pub fn is_api_message(i: &Item) -> bool {
    match item_type(i) {
        Some("message") => item_role(i) != Some("system"),
        Some(_) => true,
        None => item_role(i).is_some(),
    }
}

/// Items the model authored (assistant message / reasoning / tool calls).
///
/// Ground: legacy `is_model_generated_item`.
pub fn is_model_generated_item(i: &Item) -> bool {
    if item_role(i) == Some("user") {
        return false;
    }
    if item_role(i) == Some("assistant") {
        return true;
    }
    matches!(
        item_type(i),
        Some("message")
            | Some("reasoning")
            | Some("function_call")
            | Some("custom_tool_call")
            | Some("local_shell_call")
    )
}

/// True if the item marks a user-turn boundary (a user-authored message).
///
/// Ground: legacy `is_user_turn_boundary`.
pub fn is_user_turn_boundary(i: &Item) -> bool {
    item_role(i) == Some("user") && item_type(i) != Some("function_call_output")
}

/// Items emitted by codex tooling (model items plus their tool outputs).
///
/// Ground: legacy `is_codex_generated_item`.
pub fn is_codex_generated_item(i: &Item) -> bool {
    matches!(
        item_type(i),
        Some("reasoning")
            | Some("function_call")
            | Some("custom_tool_call")
            | Some("local_shell_call")
            | Some("function_call_output")
            | Some("custom_tool_call_output")
    )
}

// ---------------------------------------------------------------------------
// Truncation (codex history.rs + legacy lib.rs:16113-16153).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruncationPolicy {
    Bytes(usize),
    Tokens(usize),
}

impl TruncationPolicy {
    /// The effective byte budget. `Tokens(t)` resolves via the 4-bytes/token
    /// ratio (`t * 4`).
    ///
    /// Ground: legacy `TruncationPolicy::byte_budget`.
    pub fn byte_budget(self) -> usize {
        match self {
            Self::Bytes(b) => b,
            Self::Tokens(t) => t.saturating_mul(accounting::APPROX_BYTES_PER_TOKEN as usize),
        }
    }

    /// Scale the budget by `f` (truncating to integer), preserving the variant.
    ///
    /// Ground: legacy `impl Mul<f64> for TruncationPolicy`.
    pub fn scale(self, f: f64) -> Self {
        match self {
            Self::Bytes(b) => Self::Bytes(((b as f64) * f) as usize),
            Self::Tokens(t) => Self::Tokens(((t as f64) * f) as usize),
        }
    }
}

/// Truncate text to fit the policy's byte budget, appending an elision note.
///
/// Ground: legacy `truncate_text`: no-op when within budget; empty string when
/// budget is 0; otherwise truncate on a char boundary and append the note.
pub fn truncate_text(t: &str, p: TruncationPolicy) -> String {
    let budget = p.byte_budget();
    if t.len() <= budget {
        return t.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let mut end = budget.min(t.len());
    while end > 0 && !t.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &t[..end], TRUNCATION_NOTE)
}

/// Apply truncation policy (scaled x1.2) to an oversized tool-output item.
///
/// Ground: codex `process_item` (history.rs:378): tool/function-call outputs are
/// truncated with `policy * 1.2`; all other item kinds are returned unchanged.
pub fn process_item(i: &Item, p: TruncationPolicy) -> Item {
    if !is_function_call_output(i) {
        return i.clone();
    }
    let policy = p.scale(1.2);
    let mut out = i.clone();
    truncate_output_text(&mut out, policy);
    out
}

/// Truncate the textual payload of a function-call output in place.
///
/// Supports both the responses shape (`output` string or `output.content`
/// string) and the chat shape (`content` string).
fn truncate_output_text(item: &mut Item, policy: TruncationPolicy) {
    let Some(obj) = item.as_object_mut() else {
        return;
    };

    if let Some(Value::String(s)) = obj.get("output") {
        let truncated = truncate_text(s, policy);
        obj.insert("output".to_string(), Value::String(truncated));
        return;
    }
    if let Some(Value::String(s)) = obj.get("content") {
        let truncated = truncate_text(s, policy);
        obj.insert("content".to_string(), Value::String(truncated));
        return;
    }
    if let Some(Value::Object(inner)) = obj.get_mut("output") {
        if let Some(Value::String(s)) = inner.get("content") {
            let truncated = truncate_text(s, policy);
            inner.insert("content".to_string(), Value::String(truncated));
        }
    }
}

// ---------------------------------------------------------------------------
// Normalization (codex normalize.rs parity).
// ---------------------------------------------------------------------------

/// Ensure every call has a corresponding output; insert a synthetic `"aborted"`
/// output immediately after any call that lacks one.
///
/// Ground: codex `normalize.rs::ensure_call_outputs_present` (synthetic outputs
/// are inserted right after their call, in reverse index order to avoid
/// reindexing).
pub fn ensure_call_outputs_present(items: &mut Vec<Item>) {
    let mut have_output: HashSet<String> = HashSet::new();
    for item in items.iter() {
        if is_function_call_output(item) {
            if let Some(id) = item_call_id(item) {
                have_output.insert(id.to_string());
            }
        }
    }

    // Collect (call index, synthetic output) for calls missing an output.
    let mut to_insert: Vec<(usize, Item)> = Vec::new();
    let mut inserted_ids: HashSet<String> = HashSet::new();
    for (idx, item) in items.iter().enumerate() {
        if !is_function_call(item) {
            continue;
        }
        let Some(id) = item_call_id(item) else {
            continue;
        };
        if have_output.contains(id) || inserted_ids.contains(id) {
            continue;
        }
        inserted_ids.insert(id.to_string());
        to_insert.push((idx, placeholder_output(id)));
    }

    // Insert in reverse index order so earlier indices stay valid.
    for (idx, output_item) in to_insert.into_iter().rev() {
        items.insert(idx + 1, output_item);
    }
}

fn placeholder_output(call_id: &str) -> Item {
    serde_json::json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": {
            "content": ABORTED_OUTPUT_CONTENT,
            "success": false,
        },
    })
}

/// Remove outputs that have no corresponding call.
///
/// Ground: codex `normalize.rs::remove_orphan_outputs`.
pub fn remove_orphan_outputs(items: &mut Vec<Item>) {
    let mut seen_call_ids: HashSet<String> = HashSet::new();
    for item in items.iter() {
        if is_function_call(item) {
            if let Some(id) = item_call_id(item) {
                seen_call_ids.insert(id.to_string());
            }
        }
    }
    items.retain(|item| {
        if is_function_call_output(item) {
            item_call_id(item)
                .map(|id| seen_call_ids.contains(id))
                .unwrap_or(false)
        } else {
            true
        }
    });
}

/// Strip image content parts from messages when the model doesn't support
/// images.
///
/// Ground: codex `normalize.rs::strip_images_when_unsupported`. Image parts are
/// removed from `content` arrays.
pub fn strip_images_when_unsupported(supports_image: bool, items: &mut [Item]) {
    if supports_image {
        return;
    }
    for item in items.iter_mut() {
        let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        content.retain(|part| !is_image_part(part));
    }
}

fn is_image_part(part: &Value) -> bool {
    matches!(
        part.get("type").and_then(Value::as_str),
        Some("input_image") | Some("image_url") | Some("output_image")
    )
}

/// Remove the call/output pair corresponding to a removed item.
///
/// Ground: codex `normalize.rs::remove_corresponding_for`. Extracts the
/// `call_id` of `removed` and retains items whose `call_id` differs.
pub fn remove_corresponding_for(items: &mut Vec<Item>, removed: &Item) {
    let call_id = if is_function_call(removed) || is_function_call_output(removed) {
        item_call_id(removed).map(str::to_string)
    } else {
        None
    };
    let Some(call_id) = call_id else {
        return;
    };
    items.retain(|item| {
        if is_function_call(item) || is_function_call_output(item) {
            item_call_id(item) != Some(call_id.as_str())
        } else {
            true
        }
    });
}

/// Assemble the final prompt item list (normalize pipeline).
///
/// Ground: codex `for_prompt` (history.rs:359) = `ensure_call_outputs_present`
/// then `remove_orphan_outputs` then `strip_images_when_unsupported`.
pub fn for_prompt(items: Vec<Item>, supports_image: bool) -> Vec<Item> {
    let mut items = items;
    ensure_call_outputs_present(&mut items);
    remove_orphan_outputs(&mut items);
    strip_images_when_unsupported(supports_image, &mut items);
    items
}

// ---------------------------------------------------------------------------
// Total token usage (codex get_total_token_usage parity).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default, Debug)]
pub struct TotalTokenUsageBreakdown {
    pub last_api_response_total_tokens: i64,
    pub all_history_items_model_visible_bytes: i64,
    pub estimated_tokens_since_last_api_response: i64,
    pub estimated_bytes_since_last_api_response: i64,
}

/// Index of the last model-generated item, if any.
fn rposition_model_generated(items: &[Item]) -> Option<usize> {
    items
        .iter()
        .enumerate()
        .rev()
        .find(|(_, item)| is_model_generated_item(item))
        .map(|(idx, _)| idx)
}

/// Index just past the last model-generated item (or 0 if none).
///
/// Ground: codex `items_after_last_model_generated_item`.
fn start_after_last_model_generated(items: &[Item]) -> usize {
    rposition_model_generated(items).map_or(0, |idx| idx.saturating_add(1))
}

/// Sum the model-visible token estimate over a slice of items.
fn sum_item_tokens(items: &[Item]) -> i64 {
    let mut total: i64 = 0;
    for item in items {
        total = total.saturating_add(estimate_item_token_count(item));
    }
    total
}

/// Sum the model-visible byte estimate over a slice of items.
fn sum_item_bytes(items: &[Item]) -> i64 {
    let mut total: i64 = 0;
    for item in items {
        total = total.saturating_add(estimate_item_model_visible_bytes(item));
    }
    total
}

/// Sum of estimated token cost of every reasoning item before the last
/// model-generated item.
///
/// Ground: codex `get_non_last_reasoning_items_tokens` (history.rs:270). Codex
/// filters to reasoning items carrying encrypted content; with `Item = Value`
/// (no typed encrypted field), we count reasoning/compaction items in that
/// prefix.
fn non_last_reasoning_items_tokens(items: &[Item]) -> i64 {
    let Some(last_model_idx) = rposition_model_generated(items) else {
        return 0;
    };
    let mut total: i64 = 0;
    for item in &items[..last_model_idx] {
        if is_reasoning_item(item) {
            total = total.saturating_add(estimate_item_token_count(item));
        }
    }
    total
}

/// Sum a turn's total token usage, branching on `server_reasoning_included`.
///
/// Ground: codex `get_total_token_usage` (history.rs:292-332):
///   * `last` = `info.last.total` (0 when no info),
///   * `after` = tokens of items after the last model-generated item,
///   * if reasoning IS included by the server: `last + after`,
///   * else: `last + non_last_reasoning + after`.
pub fn total_token_usage(
    items: &[Item],
    info: Option<&TokenUsageInfo>,
    server_reasoning_included: bool,
) -> i64 {
    let last_tokens = info.map(|i| i.last.total).unwrap_or(0);
    let start = start_after_last_model_generated(items);
    let after_tokens = sum_item_tokens(&items[start..]);

    if server_reasoning_included {
        last_tokens.saturating_add(after_tokens)
    } else {
        last_tokens
            .saturating_add(non_last_reasoning_items_tokens(items))
            .saturating_add(after_tokens)
    }
}

/// Detailed breakdown variant.
///
/// Ground: codex `get_total_token_usage_breakdown`.
pub fn total_token_usage_breakdown(
    items: &[Item],
    info: Option<&TokenUsageInfo>,
) -> TotalTokenUsageBreakdown {
    let last_total = info.map(|i| i.last.total).unwrap_or(0);
    let start = start_after_last_model_generated(items);
    let after = &items[start..];

    TotalTokenUsageBreakdown {
        last_api_response_total_tokens: last_total,
        all_history_items_model_visible_bytes: sum_item_bytes(items),
        estimated_tokens_since_last_api_response: sum_item_tokens(after),
        estimated_bytes_since_last_api_response: sum_item_bytes(after),
    }
}
