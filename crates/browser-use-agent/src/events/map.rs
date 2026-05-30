//! PURE `LlmEvent` -> `PendingEvent` mapper + usage/payload helpers.

use super::{PendingEvent, TurnCtx};
use browser_use_llm::schema::{LlmEvent, Usage};
use browser_use_protocol::ModelUsage;
use serde_json::Value;

pub fn map_llm_event(_ctx: &TurnCtx, _ev: &LlmEvent) -> Vec<PendingEvent> {
    unimplemented!()
}

/// `computed_total()` fallback when total == 0.
pub fn usage_to_model_usage(_u: &Usage) -> ModelUsage {
    unimplemented!()
}

pub fn token_count_payload(
    _usage: &ModelUsage,
    _prev_total: &Value,
    _window: Option<i64>,
    _turn_idx: usize,
) -> Value {
    unimplemented!()
}

pub struct ResultFilePtr {
    pub url: Option<String>,
    pub path: Option<String>,
    pub bytes: Option<u64>,
}

pub fn session_done_payload(_result: Option<&str>, _result_file: Option<&ResultFilePtr>) -> Value {
    unimplemented!()
}
