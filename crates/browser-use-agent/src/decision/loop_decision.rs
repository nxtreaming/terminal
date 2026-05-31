//! Pure turn-loop decision core (codex `turn.rs:168-355`, `turn.rs:677`).

use browser_use_llm::schema::FinishReason;

#[derive(Debug, Clone, Default)]
pub struct SamplingOutcome {
    /// `turn.rs:250`.
    pub model_needs_follow_up: bool,
    pub last_agent_message: Option<String>,
    pub finish_reason: Option<FinishReason>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenStatus {
    pub auto_compact_scope_tokens: i64,
    pub auto_compact_scope_limit: i64,
    pub full_context_window_limit_reached: bool,
    /// `scope >= limit || full_window` (`turn.rs:677-678`).
    pub token_limit_reached: bool,
}

impl TokenStatus {
    /// `true` when any compaction trigger condition holds (the loop's gate —
    /// codex `turn.rs:282` reads the soft + hard flags).
    pub fn needs_compaction(&self) -> bool {
        self.token_limit_reached || self.full_context_window_limit_reached
    }

    /// Build a REAL token status from the estimated tokens currently in context
    /// and the model's context-window budget (codex/legacy `Session` auto-compact
    /// math) — "compact early and often".
    ///
    /// Parity (legacy `browser-use-core::Session`, itself a codex port, in
    /// `terminal/crates/browser-use-core/src/lib.rs`; codex
    /// `model_auto_compact_token_limit`):
    /// - auto-compact limit = `(context_window as f64 * 0.8) as i64` — the limit
    ///   falls back to **80% of the model context window** when no explicit
    ///   `model_auto_compact_token_limit` is configured. The trigger therefore
    ///   fires at 80% of the window, NOT at 100% (compact early and often).
    /// - `token_limit_reached` (soft auto-compact trigger) =
    ///   `estimated_tokens >= auto_compact_limit` (codex `total >= limit`).
    /// - `full_context_window_limit_reached` (hard ceiling) =
    ///   `estimated_tokens >= context_window`.
    ///
    /// `context_window == 0` means the budget is unknown; codex returns `None`
    /// from the auto-compact limit and never compacts, so this yields a zeroed
    /// status and the loop's gate never trips.
    pub fn from_estimate(estimated_tokens: i64, context_window: i64) -> Self {
        if context_window <= 0 {
            return Self::default();
        }
        // codex/legacy: (context_window as f64 * 0.8) as i64.
        let auto_compact_limit = (context_window as f64 * 0.8) as i64;
        Self {
            auto_compact_scope_tokens: estimated_tokens,
            auto_compact_scope_limit: auto_compact_limit,
            full_context_window_limit_reached: estimated_tokens >= context_window,
            token_limit_reached: estimated_tokens >= auto_compact_limit,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(model_nfu: bool) -> SamplingOutcome {
        SamplingOutcome {
            model_needs_follow_up: model_nfu,
            last_agent_message: None,
            finish_reason: None,
        }
    }

    fn token_status(tlr: bool) -> TokenStatus {
        TokenStatus {
            auto_compact_scope_tokens: 0,
            auto_compact_scope_limit: 0,
            full_context_window_limit_reached: false,
            token_limit_reached: tlr,
        }
    }

    // ---- needs_follow_up (turn.rs:255): model_nfu || has_pending_input ----
    #[test]
    fn needs_follow_up_truth_table() {
        // (model_nfu, has_pending_input) -> expected
        let cases = [
            (false, false, false),
            (false, true, true),
            (true, false, true),
            (true, true, true),
        ];
        for (model_nfu, pending, expected) in cases {
            assert_eq!(
                needs_follow_up(model_nfu, pending),
                expected,
                "needs_follow_up({model_nfu}, {pending})"
            );
        }
    }

    // ---- token_limit_reached (turn.rs:677): scope >= limit || full ----
    #[test]
    fn token_limit_reached_boundaries() {
        // scope == limit -> reached (>= boundary).
        assert!(token_limit_reached(100, 100, false), "scope == limit");
        // scope < limit -> not reached.
        assert!(!token_limit_reached(99, 100, false), "scope < limit");
        // scope > limit -> reached.
        assert!(token_limit_reached(101, 100, false), "scope > limit");
        // full_window forces reached even when scope < limit.
        assert!(token_limit_reached(0, 100, true), "full window overrides");
        // neither -> not reached.
        assert!(!token_limit_reached(0, 100, false), "well under limit");
    }

    #[test]
    fn token_limit_reached_negative_and_extremes() {
        // i64 inputs: negatives compare as expected; i64::MAX limit never reached by scope alone.
        assert!(!token_limit_reached(0, i64::MAX, false));
        assert!(token_limit_reached(i64::MAX, i64::MAX, false));
        assert!(token_limit_reached(-1, -1, false), "equal negatives");
        assert!(
            !token_limit_reached(-2, -1, false),
            "scope below negative limit"
        );
    }

    // ---- should_compact_mid_turn (turn.rs:282): tlr && nfu ----
    #[test]
    fn should_compact_mid_turn_truth_table() {
        assert!(!should_compact_mid_turn(false, false));
        assert!(!should_compact_mid_turn(false, true));
        assert!(!should_compact_mid_turn(true, false));
        assert!(should_compact_mid_turn(true, true));
    }

    // ---- TokenStatus::from_estimate (codex 80%-of-window auto-compact) ----
    #[test]
    fn from_estimate_fires_at_80_percent_of_window() {
        // window 1000 -> auto-compact limit = 800 (context_window * 0.8).
        let below = TokenStatus::from_estimate(799, 1000);
        assert!(!below.token_limit_reached, "799 < 800 must not trigger");
        assert!(!below.full_context_window_limit_reached);
        assert!(!below.needs_compaction());

        let at = TokenStatus::from_estimate(800, 1000);
        assert!(at.token_limit_reached, "800 >= 800 must trigger (codex >=)");
        assert!(at.needs_compaction());
        assert!(!at.full_context_window_limit_reached, "800 < 1000 window");
        assert_eq!(at.auto_compact_scope_limit, 800);
        assert_eq!(at.auto_compact_scope_tokens, 800);
    }

    #[test]
    fn from_estimate_sets_full_window_at_ceiling() {
        let full = TokenStatus::from_estimate(1000, 1000);
        assert!(full.token_limit_reached);
        assert!(
            full.full_context_window_limit_reached,
            "1000 >= 1000 window is the hard ceiling"
        );
        assert!(full.needs_compaction());
    }

    #[test]
    fn from_estimate_zero_window_disables_accounting() {
        // Unknown budget => zeroed status => loop never compacts (codex None=>false).
        let st = TokenStatus::from_estimate(1_000_000, 0);
        assert_eq!(st, TokenStatus::default());
        assert!(!st.needs_compaction());
    }

    // ---- can_drain_after_compact (turn.rs:306): !model_nfu ----
    #[test]
    fn can_drain_after_compact_inverts_model_nfu() {
        // After compaction, drain pending only if the MODEL itself did not ask
        // to continue (model_needs_follow_up). turn.rs:306.
        assert!(can_drain_after_compact(false), "model done -> may drain");
        assert!(
            !can_drain_after_compact(true),
            "model continues -> hold drain"
        );
    }

    // ---- initial_can_drain (turn.rs:168): !turn_has_fresh_input ----
    #[test]
    fn initial_can_drain_inverts_fresh_input() {
        // input.is_empty() == !turn_has_fresh_input.
        assert!(
            initial_can_drain(false),
            "no fresh input -> drain immediately"
        );
        assert!(!initial_can_drain(true), "fresh input -> sample it first");
    }

    // ---- classify_loop_step: full truth table over (model_nfu, pending, tlr) ----
    #[test]
    fn classify_loop_step_full_truth_table() {
        // Columns: model_nfu, has_pending_input, token_limit_reached -> expected LoopStep.
        // nfu = model_nfu || pending; compact = tlr && nfu;
        // can_drain_next (only when compacting) = !model_nfu.
        struct Case {
            model_nfu: bool,
            pending: bool,
            tlr: bool,
            expected: LoopStep,
        }
        let cases = [
            // tlr == false: never compact; Continue iff nfu, else Complete.
            Case {
                model_nfu: false,
                pending: false,
                tlr: false,
                expected: LoopStep::Complete,
            },
            Case {
                model_nfu: false,
                pending: true,
                tlr: false,
                expected: LoopStep::Continue,
            },
            Case {
                model_nfu: true,
                pending: false,
                tlr: false,
                expected: LoopStep::Continue,
            },
            Case {
                model_nfu: true,
                pending: true,
                tlr: false,
                expected: LoopStep::Continue,
            },
            // tlr == true: compact iff nfu (else Complete). can_drain_next = !model_nfu.
            Case {
                model_nfu: false,
                pending: false,
                tlr: true,
                expected: LoopStep::Complete,
            },
            Case {
                model_nfu: false,
                pending: true,
                tlr: true,
                // nfu via pending only; model itself is done -> may drain after compact.
                expected: LoopStep::CompactThenContinue {
                    can_drain_next: true,
                },
            },
            Case {
                model_nfu: true,
                pending: false,
                tlr: true,
                // model wants to continue -> hold drain after compact.
                expected: LoopStep::CompactThenContinue {
                    can_drain_next: false,
                },
            },
            Case {
                model_nfu: true,
                pending: true,
                tlr: true,
                expected: LoopStep::CompactThenContinue {
                    can_drain_next: false,
                },
            },
        ];
        for c in cases {
            let got = classify_loop_step(&outcome(c.model_nfu), c.pending, &token_status(c.tlr));
            assert_eq!(
                got, c.expected,
                "classify_loop_step(model_nfu={}, pending={}, tlr={})",
                c.model_nfu, c.pending, c.tlr
            );
        }
    }

    #[test]
    fn classify_loop_step_compact_takes_precedence_over_plain_continue() {
        // When both compaction and follow-up conditions hold, CompactThenContinue
        // wins (the `if should_compact` branch precedes the `else if nfu` branch).
        let step = classify_loop_step(&outcome(true), true, &token_status(true));
        assert_eq!(
            step,
            LoopStep::CompactThenContinue {
                can_drain_next: false
            }
        );
    }
}
