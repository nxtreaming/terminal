//! Token-budget accounting for a goal.
//!
//! ## The formula
//!
//! A response's usage is accounted as the WP-required arithmetic
//!
//! ```text
//! accounted = input_tokens (INCLUDING cached) + max(output_tokens, 0)
//! ```
//!
//! The shape (`input + max(output, 0)`, saturating) and the `.max(0)` output
//! clamp mirror codex/legacy goal token accounting:
//!   * codex `core/src/goals.rs:1684-1688` `goal_token_delta_for_usage`:
//!     `usage.non_cached_input().saturating_add(usage.output_tokens.max(0))`.
//!   * legacy `browser-use-core/src/goals.rs:330-334` `goal_token_delta_for_usage`:
//!     `input_tokens - cached_input_tokens + max(output_tokens, 0)`, clamped
//!     `.max(0)`.
//!
//! **Deliberate divergence (parity debt):** both codex and legacy SUBTRACT
//! `cached_input_tokens` (they bill only the *non-cached* input). This WP
//! requires INCLUDING cached — i.e. we drop the `- cached` term — so a
//! cache-heavy response is charged its full input against the goal budget. This
//! is the explicitly mandated formula for this subsystem, not an accident; the
//! `cached_input_tokens` `Usage` field is intentionally unread.
//!
//! The per-field saturating `.max(0)` delta also matches how codex aggregates
//! turn token usage (every field clamped with `.max(0)`); see the codex
//! `tasks/mod.rs` turn-token-usage block (each field computed as
//! `(total - start).max(0)`).
//!
//! ## Reuse, not reinvention
//!
//! This module does NOT re-implement the byte->token heuristic. The
//! `(bytes + 3) / 4` (`bytes.div_ceil(4)`) conversion is owned by
//! [`crate::context::accounting::approx_tokens_from_byte_count_i64`] and is
//! re-exported here as [`approx_tokens_from_byte_count_i64`] so callers that
//! need to budget a raw blob go through the one shared helper.

use browser_use_llm::schema::Usage;

// Re-export the shared byte->token heuristic so budgeting never grows a private
// copy of `(b + 3) / 4`. Ground: `context/accounting.rs:28-34`.
pub use crate::context::accounting::approx_tokens_from_byte_count_i64;

/// Default fraction of the budget at which a `warning` steering crossing fires.
///
/// Parity debt: the legacy goal path has no soft "warn" threshold — it only
/// flips to budget-limited once `tokens_used >= token_budget`
/// (`browser-use-core/src/goals.rs:250-258` / `:358`). We add a soft warn
/// fraction so the UI/steering layer can surface "running low" before hard
/// exhaustion; the value is a local choice, documented here, and the exhaustion
/// boundary itself stays byte-for-byte with legacy (`>=` budget).
pub const DEFAULT_WARN_FRACTION: f64 = 0.8;

/// Tokens consumed by a single response usage:
/// `input (INCLUDING cached) + max(output, 0)`, saturating.
///
/// Shape parity: codex `goal_token_delta_for_usage`
/// (`core/src/goals.rs:1684-1688`) and legacy
/// (`browser-use-core/src/goals.rs:330-334`) both use
/// `saturating_add(output.max(0))`. The WP-required divergence is that we do NOT
/// subtract `cached_input_tokens` (codex/legacy bill non-cached input only).
pub fn tokens_from_usage(input_tokens: i64, output_tokens: i64) -> i64 {
    input_tokens.saturating_add(output_tokens.max(0))
}

/// Tokens consumed by a response, read straight off an LLM [`Usage`].
///
/// Per the required formula, `cached_input_tokens` is NOT subtracted: the full
/// `input_tokens` (which already includes the cached portion) is charged. This
/// is the deliberate divergence from codex/legacy documented in the module
/// header.
pub fn tokens_from_llm_usage(usage: &Usage) -> i64 {
    // `Usage` fields are `u64`; widen to `i64` for the saturating, `max(0)`
    // arithmetic shared with the rest of `context/accounting.rs`.
    tokens_from_usage(usage.input_tokens as i64, usage.output_tokens as i64)
}

/// Which budget threshold (if any) the accumulated usage currently sits at.
///
/// Ordering is monotonic with `tokens_used`: `Ok -> Warn -> Exhausted`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetLevel {
    /// Below the warn fraction (or no budget set).
    Ok,
    /// At/above the warn fraction but below the hard budget.
    Warn,
    /// At/above the hard budget. Parity: legacy `tokens_used >= token_budget`
    /// (`browser-use-core/src/goals.rs:250-258` / `:358`).
    Exhausted,
}

/// Running token-budget accounting for a goal.
///
/// `accounted` accumulates [`tokens_from_usage`] per response (saturating). The
/// optional `max` is the hard token budget (`None` => unlimited), matching
/// legacy `ThreadGoalSnapshot::token_budget`
/// (`browser-use-core/src/goals.rs:22`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BudgetState {
    /// Total tokens accounted so far (legacy `tokens_used`).
    accounted: i64,
    /// Hard token budget; `None` means unlimited (legacy `token_budget`).
    max: Option<i64>,
}

impl BudgetState {
    /// A fresh budget with an optional hard ceiling.
    pub fn new(max: Option<i64>) -> Self {
        Self { accounted: 0, max }
    }

    /// Set / replace the hard ceiling (e.g. a `goal.updated` budget change).
    pub fn set_max(&mut self, max: Option<i64>) {
        self.max = max;
    }

    /// The hard ceiling, if any.
    pub fn max(&self) -> Option<i64> {
        self.max
    }

    /// Add one response's usage. Returns the tokens added (so callers can emit a
    /// `goal.accounted`-shaped event with the exact delta).
    ///
    /// The per-response increment is [`tokens_from_usage`]
    /// (`input incl. cached + max(output, 0)`), accumulated saturating like the
    /// legacy `goal.accounted` accumulation
    /// (`browser-use-core/src/goals.rs:110-131` `goal_accounted_usage_from_events`,
    /// constant `GOAL_ACCOUNTING_EVENT = "goal.accounted"`, constants.rs:128).
    pub fn account(&mut self, usage: &Usage) -> i64 {
        let added = tokens_from_llm_usage(usage);
        self.accounted = self.accounted.saturating_add(added);
        added
    }

    /// Add a raw token delta (e.g. one already computed via
    /// [`tokens_from_usage`]). Saturating.
    pub fn account_tokens(&mut self, tokens: i64) {
        self.accounted = self.accounted.saturating_add(tokens);
    }

    /// Total tokens accounted so far (legacy `tokens_used`).
    pub fn total_accounted(&self) -> i64 {
        self.accounted
    }

    /// Tokens remaining against `self.max`, clamped at zero. `None` when no
    /// budget is set.
    ///
    /// Parity: legacy remaining-tokens arithmetic
    /// (`browser-use-core/src/goals.rs:177-179`):
    /// `token_budget.map(|budget| budget.saturating_sub(tokens_used).max(0))`.
    pub fn remaining(&self) -> Option<i64> {
        self.max
            .map(|budget| budget.saturating_sub(self.accounted).max(0))
    }

    /// Whether the hard budget has been reached.
    ///
    /// Parity: legacy budget-limited predicate
    /// (`browser-use-core/src/goals.rs:250-258` `goal_effective_status` and
    /// `:358` `maybe_mark_goal_budget_limited`): `tokens_used >= token_budget`.
    /// With no budget set, never exhausted.
    pub fn is_exhausted(&self) -> bool {
        self.max
            .map(|budget| self.accounted >= budget)
            .unwrap_or(false)
    }

    /// Whether the soft warn fraction has been crossed (and not yet exhausted),
    /// using [`DEFAULT_WARN_FRACTION`].
    pub fn is_warning(&self) -> bool {
        self.is_warning_at(DEFAULT_WARN_FRACTION)
    }

    /// Soft-warn check at a caller-chosen fraction in `[0.0, 1.0]`.
    pub fn is_warning_at(&self, warn_fraction: f64) -> bool {
        let Some(budget) = self.max else {
            return false;
        };
        if self.accounted >= budget {
            // Past the hard limit it is `Exhausted`, not `Warn`.
            return false;
        }
        let threshold = warn_threshold(budget, warn_fraction);
        self.accounted >= threshold
    }

    /// Current budget level using [`DEFAULT_WARN_FRACTION`].
    pub fn level(&self) -> BudgetLevel {
        self.level_at(DEFAULT_WARN_FRACTION)
    }

    /// Current budget level at a caller-chosen warn fraction.
    pub fn level_at(&self, warn_fraction: f64) -> BudgetLevel {
        if self.is_exhausted() {
            BudgetLevel::Exhausted
        } else if self.is_warning_at(warn_fraction) {
            BudgetLevel::Warn
        } else {
            BudgetLevel::Ok
        }
    }
}

/// The integer token count at which the warn fraction is crossed for a budget.
///
/// Saturating throughout; a non-finite or non-positive fraction collapses the
/// warn band onto the hard budget (warn == exhaust).
pub fn warn_threshold(budget: i64, warn_fraction: f64) -> i64 {
    if budget <= 0 {
        return 0;
    }
    if !warn_fraction.is_finite() || warn_fraction <= 0.0 {
        return budget;
    }
    let frac = warn_fraction.min(1.0);
    // budget is > 0 here; `as f64` is exact for realistic token counts.
    let raw = (budget as f64) * frac;
    let threshold = raw.floor() as i64;
    threshold.clamp(0, budget)
}
