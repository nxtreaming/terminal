//! `decision/` — PURE SYNC. No tokio, no I/O, no `&self`. The unit-test surface.
//!
//! Every behavior branch in the async layers delegates to a function here so the
//! logic stays deterministically testable against codex parity tables.

pub mod loop_decision;
pub mod retry;
pub mod tool_decision;

pub use loop_decision::{
    can_drain_after_compact, classify_loop_step, initial_can_drain, needs_follow_up,
    should_compact_mid_turn, token_limit_reached, LoopStep, SamplingOutcome, TokenStatus,
};
pub use retry::{backoff_ms, retry_decision, RetryAction};
pub use tool_decision::{classify_parallelism, ToolParallelism};
