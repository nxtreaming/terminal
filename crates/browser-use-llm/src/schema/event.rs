//! The provider-neutral streaming event model and aggregated response.
//!
//! Every protocol's stream decoder normalizes its native events into this
//! `LlmEvent` sequence, guaranteeing a well-formed lifecycle:
//! `step_start → (text|reasoning|tool_input start/delta/end)* → step_finish → finish`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ids::FinishReason;
use super::messages::ContentPart;

/// Token usage with an explicitly **non-overlapping** breakdown, so consumers
/// never have to subtract. `total_tokens` is the inclusive total reported (or
/// computed) for the turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

impl Usage {
    /// Sum of the breakdown fields (use when a provider does not report an
    /// inclusive total). `cached_input_tokens` is a subset of `input_tokens`
    /// and is therefore not added again.
    pub fn computed_total(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.reasoning_output_tokens
    }
}

/// A normalized streaming event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmEvent {
    StepStart,
    TextStart {
        id: String,
    },
    TextDelta {
        id: String,
        delta: String,
    },
    TextEnd {
        id: String,
    },
    ReasoningStart {
        id: String,
    },
    ReasoningDelta {
        id: String,
        delta: String,
    },
    ReasoningEnd {
        id: String,
    },
    ToolInputStart {
        id: String,
        name: String,
    },
    ToolInputDelta {
        id: String,
        delta: String,
    },
    ToolInputEnd {
        id: String,
    },
    ToolCall {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        input: Value,
    },
    StepFinish {
        #[serde(default)]
        usage: Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<FinishReason>,
    },
    Finish {
        #[serde(default)]
        usage: Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<FinishReason>,
    },
    ProviderError {
        message: String,
        #[serde(default)]
        retryable: bool,
    },
}

/// The aggregated, non-streaming result of a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmResponse {
    #[serde(default)]
    pub content: Vec<ContentPart>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
}
