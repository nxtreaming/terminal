//! `decision/` — PURE SYNC. No tokio, no I/O, no `&self`. The unit-test surface.
//!
//! Every behavior branch in the async layers delegates to a function here so the
//! logic stays deterministically testable against codex parity tables.

pub mod loop_decision;
pub mod retry;
pub mod tool_decision;

pub use loop_decision::{
    auto_compact_token_limit, can_drain_after_compact, classify_loop_step, initial_can_drain,
    needs_follow_up, should_compact_mid_turn, token_limit_reached, AutoCompactTokenLimitScope,
    LoopStep, SamplingOutcome, TokenStatus,
};
pub use retry::{backoff_ms, retry_decision, RetryAction};
pub use tool_decision::{classify_parallelism, ToolParallelism};

#[cfg(test)]
mod tests {
    //! Smoke tests for the public `decision::` re-export surface — every WP-A1
    //! symbol must be reachable directly from the module root.
    use super::*;

    #[test]
    fn reexports_are_reachable() {
        // loop_decision surface.
        assert!(needs_follow_up(true, false));
        assert!(token_limit_reached(10, 10, false));
        assert!(should_compact_mid_turn(true, true));
        assert!(can_drain_after_compact(false));
        assert!(initial_can_drain(false));
        assert_eq!(auto_compact_token_limit(Some(100), None), Some(90));
        assert_eq!(
            AutoCompactTokenLimitScope::default(),
            AutoCompactTokenLimitScope::Total
        );
        let out = SamplingOutcome::default();
        let st = TokenStatus {
            auto_compact_scope_tokens: 0,
            auto_compact_scope_limit: 1,
            full_context_window_limit_reached: false,
            token_limit_reached: false,
        };
        assert_eq!(classify_loop_step(&out, false, &st), LoopStep::Complete);

        // retry surface.
        assert_eq!(retry_decision(0, 0, false, true, None), RetryAction::Fail);
        assert_eq!(backoff_ms(0), 200);

        // sibling tool_decision surface stays re-exported.
        assert_eq!(classify_parallelism(true, true), ToolParallelism::Parallel);
    }
}
