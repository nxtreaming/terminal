//! History assembly / normalization for the prompt (codex parity). Pure, no I/O.

use super::accounting;
use super::Item;

pub fn estimate_item_model_visible_bytes(_i: &Item) -> i64 {
    unimplemented!()
}

pub fn estimate_item_token_count(_i: &Item) -> i64 {
    unimplemented!()
}

pub fn is_api_message(_i: &Item) -> bool {
    unimplemented!()
}

pub fn is_model_generated_item(_i: &Item) -> bool {
    unimplemented!()
}

pub fn is_user_turn_boundary(_i: &Item) -> bool {
    unimplemented!()
}

pub fn is_codex_generated_item(_i: &Item) -> bool {
    unimplemented!()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruncationPolicy {
    Bytes(usize),
    Tokens(usize),
}

impl TruncationPolicy {
    pub fn byte_budget(self) -> usize {
        unimplemented!()
    }

    pub fn scale(self, _f: f64) -> Self {
        unimplemented!()
    }
}

pub fn truncate_text(_t: &str, _p: TruncationPolicy) -> String {
    unimplemented!()
}

/// Outputs get `p * 1.2`.
pub fn process_item(_i: &Item, _p: TruncationPolicy) -> Item {
    unimplemented!()
}

pub fn ensure_call_outputs_present(_items: &mut Vec<Item>) {
    unimplemented!()
}

pub fn remove_orphan_outputs(_items: &mut Vec<Item>) {
    unimplemented!()
}

pub fn strip_images_when_unsupported(_supports_image: bool, _items: &mut [Item]) {
    unimplemented!()
}

pub fn remove_corresponding_for(_items: &mut Vec<Item>, _removed: &Item) {
    unimplemented!()
}

/// `ensure -> orphan -> strip`.
pub fn for_prompt(_items: Vec<Item>, _supports_image: bool) -> Vec<Item> {
    unimplemented!()
}

#[derive(Clone, Copy, Default, Debug)]
pub struct TotalTokenUsageBreakdown {
    pub last_api_response_total_tokens: i64,
    pub all_history_items_model_visible_bytes: i64,
    pub estimated_tokens_since_last_api_response: i64,
    pub estimated_bytes_since_last_api_response: i64,
}

pub fn total_token_usage(
    _items: &[Item],
    _info: Option<&accounting::TokenUsageInfo>,
    _server_reasoning_included: bool,
) -> i64 {
    unimplemented!()
}

pub fn total_token_usage_breakdown(
    _items: &[Item],
    _info: Option<&accounting::TokenUsageInfo>,
) -> TotalTokenUsageBreakdown {
    unimplemented!()
}
