//! Token usage math + `TokenUsageInfo`.
//!
//! This is the "real tokens, not char-estimation" core. The byte/token
//! heuristics are ported verbatim from codex `context_manager/history.rs`
//! (cross-checked against legacy `browser-use-core`):
//!   * `approx_tokens_from_byte_count_i64` = `bytes.div_ceil(4)`, clamped to 0
//!     for non-positive byte counts (history.rs:~483 / legacy lib.rs:4983).
//!   * `estimate_reasoning_length` = `len * 3 / 4 - 650`, saturating
//!     (history.rs:504 / legacy lib.rs:4171).
//!   * `estimate_encrypted_function_output_length` = `len * 9` div_ceil 16
//!     (history.rs:512 / legacy lib.rs:4179).
//!
//! `TokenUsage::from_llm_usage` falls back to `Usage::computed_total()` (input +
//! output + reasoning_output, EXCLUDING cached) when the server reports no
//! total.

use browser_use_llm::schema::Usage;

pub const APPROX_BYTES_PER_TOKEN: i64 = 4;
pub const RESIZED_IMAGE_BYTES_ESTIMATE: i64 = 7_373;

/// `bytes.div_ceil(4)`, clamped at zero for non-positive inputs.
///
/// Ground: codex `history.rs::approx_tokens_from_byte_count_i64`
/// (`if bytes <= 0 { 0 } else { bytes.div_ceil(APPROX_BYTES_PER_TOKEN) }`);
/// legacy `browser-use-core` lib.rs:4983. `i64::div_ceil` is unstable on the
/// pinned toolchain, so we spell the ceiling division out by hand.
pub fn approx_tokens_from_byte_count_i64(bytes: i64) -> i64 {
    if bytes <= 0 {
        0
    } else {
        (bytes + APPROX_BYTES_PER_TOKEN - 1) / APPROX_BYTES_PER_TOKEN
    }
}

/// Inverse: `tokens * APPROX_BYTES_PER_TOKEN`.
///
/// Cross-check: legacy `browser-use-core::char_budget_for_tokens` = `t * 4`.
pub fn approx_bytes_for_tokens(tokens: usize) -> usize {
    tokens.saturating_mul(APPROX_BYTES_PER_TOKEN as usize)
}

/// `len * 3 / 4 - 650`, saturating at zero. Codex reasoning heuristic.
///
/// Ground: codex `history.rs::estimate_reasoning_length`; legacy lib.rs:4171.
/// The subtraction saturates so tiny reasoning blobs estimate to zero rather
/// than going negative.
pub fn estimate_reasoning_length(len: usize) -> usize {
    len.saturating_mul(3)
        .checked_div(4)
        .unwrap_or(0)
        .saturating_sub(650)
}

/// `len * 9` div_ceil 16. Codex encrypted-function-output heuristic.
///
/// Ground: codex `history.rs::estimate_encrypted_function_output_length`;
/// legacy lib.rs:4179. Models the base64/encryption expansion of an encrypted
/// function-call output.
pub fn estimate_encrypted_function_output_length(len: usize) -> usize {
    len.saturating_mul(9).div_ceil(16)
}

/// Mirrors `browser_use_llm::schema::Usage` accounting (frozen field set: all
/// `i64`).
///
/// `total` is the server-reported total when present; otherwise it is computed
/// as `input + output + reasoning_output` **excluding** `cached_input` (cached
/// tokens are a subset of input already counted). This matches
/// `Usage::computed_total()`.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: i64,
    pub cached_input: i64,
    pub output: i64,
    pub reasoning_output: i64,
    pub total: i64,
}

impl TokenUsage {
    /// Build from an LLM `Usage`. When the server omits a total
    /// (`total_tokens == 0`) we fall back to `computed_total()`
    /// (`input + output + reasoning_output`, excluding cached).
    pub fn from_llm_usage(u: &Usage) -> Self {
        let total = if u.total_tokens != 0 {
            u.total_tokens
        } else {
            u.computed_total()
        };
        Self {
            input: u.input_tokens as i64,
            cached_input: u.cached_input_tokens as i64,
            output: u.output_tokens as i64,
            reasoning_output: u.reasoning_output_tokens as i64,
            total: total as i64,
        }
    }

    /// Field-wise (saturating) sum of two usages.
    pub fn add(&self, o: &Self) -> Self {
        Self {
            input: self.input.saturating_add(o.input),
            cached_input: self.cached_input.saturating_add(o.cached_input),
            output: self.output.saturating_add(o.output),
            reasoning_output: self.reasoning_output.saturating_add(o.reasoning_output),
            total: self.total.saturating_add(o.total),
        }
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct TokenUsageInfo {
    pub total: TokenUsage,
    pub last: TokenUsage,
    pub model_context_window: Option<i64>,
}

impl TokenUsageInfo {
    /// Either append `last` to a running `prev`, or initialize fresh.
    ///
    /// Returns `None` only when there is nothing to record (no previous info
    /// and no new `last` usage). When `prev` exists and `last` is supplied,
    /// `last` is accumulated into `total` and stored as `last`. The context
    /// window is updated when a fresh value is supplied, otherwise the previous
    /// value is preserved.
    pub fn new_or_append(
        prev: Option<&TokenUsageInfo>,
        last: Option<&TokenUsage>,
        window: Option<i64>,
    ) -> Option<TokenUsageInfo> {
        match (prev, last) {
            (None, None) => None,
            (None, Some(last)) => Some(TokenUsageInfo {
                total: *last,
                last: *last,
                model_context_window: window,
            }),
            (Some(prev), None) => {
                let mut info = prev.clone();
                if window.is_some() {
                    info.model_context_window = window;
                }
                Some(info)
            }
            (Some(prev), Some(last)) => {
                let mut info = prev.clone();
                info.total = info.total.add(last);
                info.last = *last;
                if window.is_some() {
                    info.model_context_window = window;
                }
                Some(info)
            }
        }
    }

    /// Record (or overwrite) the model context window.
    pub fn fill_to_context_window(&mut self, window: i64) {
        self.model_context_window = Some(window);
    }

    /// Construct an info whose context window is set to `window` (no usage yet).
    pub fn full_context_window(window: i64) -> Self {
        Self {
            total: TokenUsage::default(),
            last: TokenUsage::default(),
            model_context_window: Some(window),
        }
    }
}
