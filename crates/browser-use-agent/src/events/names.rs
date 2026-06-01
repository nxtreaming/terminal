//! Canonical event-type strings (codex parity).

pub const SESSION_INPUT: &str = "session.input";
pub const SESSION_FOLLOWUP: &str = "session.followup";
pub const SESSION_DONE: &str = "session.done";
pub const SESSION_FAILED: &str = "session.failed";
pub const TASK_STARTED: &str = "task_started"; // CODEX_TURN_STARTED_EVENT
pub const TASK_COMPLETE: &str = "task_complete"; // CODEX_TURN_COMPLETE_EVENT
pub const TURN_ABORTED: &str = "turn_aborted"; // CODEX_TURN_ABORTED_EVENT
pub const TOKEN_COUNT: &str = "token_count"; // CODEX_TOKEN_COUNT_EVENT
pub const STREAM_ERROR: &str = "stream_error"; // CODEX_STREAM_ERROR_EVENT
pub const MODEL_TURN_REQUEST: &str = "model.turn.request";
pub const MODEL_TURN_RETRY: &str = "model.turn.retry";
pub const MODEL_TURN_ERROR: &str = "model.turn.error";
pub const MODEL_STREAM_DELTA: &str = "model.stream_delta";
pub const MODEL_THINKING_DELTA: &str = "model.thinking_delta";
pub const TOOL_STARTED: &str = "tool.started";
pub const TOOL_OUTPUT: &str = "tool.output";
pub const TOOL_OUTPUT_DELTA: &str = "tool.output_delta";
pub const TOOL_FAILED: &str = "tool.failed";
pub const TOOL_ABORTED: &str = "tool.aborted";
pub const TOOL_IMAGE: &str = "tool.image";
pub const TOOL_OUTPUT_SPILLED: &str = "tool.output_spilled";
pub const ARTIFACT_CREATED: &str = "artifact.created";
/// core `constants.rs:10`.
pub const DEFAULT_TOOL_OUTPUT_TEXT_TOKENS: usize = 2_500;
