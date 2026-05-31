//! TUI-side product-analytics adapter.
//!
//! origin/main's TUI called `browser_use_core::product_analytics::…`, a module
//! on the (now-deleted) `browser-use-core` engine. The new `browser-use-agent`
//! engine re-implements only the low-level capture primitives in
//! [`browser_use_agent::infra::analytics`] (`capture_async` / `capture_blocking`)
//! and does NOT port the higher-level `capture_user_message` /
//! `capture_user_message_blocked` helpers or the `MESSAGE_KIND_*` /
//! `BLOCKED_REASON_*` event-property constants the TUI calls.
//!
//! Rather than edit the engine, this thin TUI-local module restores exactly that
//! lost surface on top of the engine's primitives. The helper signatures, event
//! names, and JSON property shapes are copied verbatim from the deleted
//! `browser-use-core::product_analytics` (origin/main) so analytics stay
//! byte-for-byte identical. See the engine-gap note in the commit message.

use browser_use_store::Store;

pub use browser_use_agent::infra::analytics::capture_async;

pub const MESSAGE_KIND_INITIAL: &str = "initial";
pub const MESSAGE_KIND_FOLLOWUP: &str = "followup";
pub const MESSAGE_KIND_REQUEST_INPUT_RESPONSE: &str = "request_input_response";
pub const BLOCKED_REASON_NO_AUTH: &str = "no_auth";

const APPROX_CHARS_PER_TOKEN: usize = 4;

#[allow(clippy::too_many_arguments)]
pub fn capture_user_message(
    store: &Store,
    surface: &str,
    session_id: &str,
    is_subagent: bool,
    kind: &str,
    seq: i64,
    text: &str,
) {
    let trimmed = text.trim();
    let char_count = trimmed.chars().count();
    let word_count = if trimmed.is_empty() {
        0
    } else {
        trimmed.split_whitespace().count()
    };
    let approx_tokens = char_count.div_ceil(APPROX_CHARS_PER_TOKEN);
    capture_async(
        store,
        "bu:tui user_message",
        serde_json::json!({
            "surface": surface,
            "session_id": session_id,
            "is_subagent": is_subagent,
            "kind": kind,
            "seq": seq,
            "char_count": char_count,
            "word_count": word_count,
            "approx_tokens": approx_tokens,
        }),
    );
}

#[allow(clippy::too_many_arguments)]
pub fn capture_user_message_blocked(
    store: &Store,
    surface: &str,
    session_id: &str,
    is_subagent: bool,
    seq: i64,
    text: &str,
    blocked_reason: &str,
) {
    let trimmed = text.trim();
    let char_count = trimmed.chars().count();
    let word_count = if trimmed.is_empty() {
        0
    } else {
        trimmed.split_whitespace().count()
    };
    let approx_tokens = char_count.div_ceil(APPROX_CHARS_PER_TOKEN);
    capture_async(
        store,
        "bu:tui user_message_blocked",
        serde_json::json!({
            "surface": surface,
            "session_id": session_id,
            "is_subagent": is_subagent,
            "seq": seq,
            "char_count": char_count,
            "word_count": word_count,
            "approx_tokens": approx_tokens,
            "blocked_reason": blocked_reason,
        }),
    );
}
