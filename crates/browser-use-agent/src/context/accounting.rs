//! REAL token accounting (codex parity). Pure, no I/O.

use browser_use_llm::schema::Usage;

pub const APPROX_BYTES_PER_TOKEN: i64 = 4;
pub const RESIZED_IMAGE_BYTES_ESTIMATE: i64 = 7_373;

/// `<= 0 => 0`, else divide-ceil by 4.
pub fn approx_tokens_from_byte_count_i64(_b: i64) -> i64 {
    unimplemented!()
}

/// `t * 4`.
pub fn approx_bytes_for_tokens(_t: usize) -> usize {
    unimplemented!()
}

/// `* 3 / 4 - 650`, saturating.
pub fn estimate_reasoning_length(_len: usize) -> usize {
    unimplemented!()
}

/// `* 9`, divide-ceil by 16.
pub fn estimate_encrypted_function_output_length(_len: usize) -> usize {
    unimplemented!()
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: i64,
    pub cached_input: i64,
    pub output: i64,
    pub reasoning_output: i64,
    pub total: i64,
}

impl TokenUsage {
    /// `total = input + output + reasoning` if 0 (excludes cached).
    pub fn from_llm_usage(_u: &Usage) -> Self {
        unimplemented!()
    }

    pub fn add(&self, _o: &Self) -> Self {
        unimplemented!()
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct TokenUsageInfo {
    pub total: TokenUsage,
    pub last: TokenUsage,
    pub model_context_window: Option<i64>,
}

impl TokenUsageInfo {
    pub fn new_or_append(
        _prev: Option<&TokenUsageInfo>,
        _last: Option<&TokenUsage>,
        _window: Option<i64>,
    ) -> Option<TokenUsageInfo> {
        unimplemented!()
    }

    pub fn fill_to_context_window(&mut self, _window: i64) {
        unimplemented!()
    }

    pub fn full_context_window(_window: i64) -> Self {
        unimplemented!()
    }
}
