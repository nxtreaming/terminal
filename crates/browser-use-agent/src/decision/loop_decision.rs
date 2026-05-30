//! Pure turn-loop decision core (codex `turn.rs:168-355`, `turn.rs:677`).

use browser_use_llm::schema::FinishReason;

#[derive(Debug, Clone, Default)]
pub struct SamplingOutcome {
    /// `turn.rs:250`.
    pub model_needs_follow_up: bool,
    pub last_agent_message: Option<String>,
    pub finish_reason: Option<FinishReason>,
}

#[derive(Debug, Clone)]
pub struct TokenStatus {
    pub auto_compact_scope_tokens: i64,
    pub auto_compact_scope_limit: i64,
    pub full_context_window_limit_reached: bool,
    /// `scope >= limit || full_window` (`turn.rs:677-678`).
    pub token_limit_reached: bool,
}

/// `turn.rs:255`.
pub fn needs_follow_up(model_nfu: bool, has_pending_input: bool) -> bool {
    model_nfu || has_pending_input
}

/// `turn.rs:677`.
pub fn token_limit_reached(scope: i64, limit: i64, full: bool) -> bool {
    scope >= limit || full
}

/// `turn.rs:282`.
pub fn should_compact_mid_turn(tlr: bool, nfu: bool) -> bool {
    tlr && nfu
}

/// `turn.rs:306`.
pub fn can_drain_after_compact(model_nfu: bool) -> bool {
    !model_nfu
}

/// `turn.rs:168`.
pub fn initial_can_drain(turn_has_fresh_input: bool) -> bool {
    !turn_has_fresh_input
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopStep {
    CompactThenContinue { can_drain_next: bool },
    Continue,
    Complete,
}

pub fn classify_loop_step(
    out: &SamplingOutcome,
    has_pending_input: bool,
    st: &TokenStatus,
) -> LoopStep {
    let nfu = needs_follow_up(out.model_needs_follow_up, has_pending_input);
    if should_compact_mid_turn(st.token_limit_reached, nfu) {
        LoopStep::CompactThenContinue {
            can_drain_next: can_drain_after_compact(out.model_needs_follow_up),
        }
    } else if nfu {
        LoopStep::Continue
    } else {
        LoopStep::Complete
    }
}
