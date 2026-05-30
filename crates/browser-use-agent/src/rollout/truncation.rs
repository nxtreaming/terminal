//! Size-bounded rollout truncation.
//!
//! Codex parity: ported from `ConversationHistory::truncate_if_needed` in
//! `codex-rs/core/src/conversation_history.rs:44-76`. Codex keeps the rollout
//! history bounded so it never sends an overly large payload to the model: once
//! the serialized size exceeds a byte budget it drops the OLDEST items one at a
//! time until it fits, always keeping at least the most-recent item.
//!
//! IMPORTANT parity note (flagged as required by the WP brief): the symbol names
//! in the brief (`DEFAULT_THREAD_ROLLOUT_MAX_BYTES`, `truncate_rollout_if_needed`,
//! `TruncationOutcome` in `thread_rollout_truncation.rs`) do NOT exist in the
//! pinned codex checkout. The real byte-bounded truncation lives in
//! `conversation_history.rs` and `thread_rollout_truncation.rs` contains ONLY
//! user-turn-boundary slicing (no byte logic) — that turn logic is mirrored in
//! [`crate::rollout::fork`]. We keep the brief's public names as the stable agent
//! API but cite the REAL codex source (`conversation_history.rs:11,44-76`) and
//! replicate its algorithm exactly: drop-oldest-until-fit, keep >= last item.
//!
//! This module operates over the async agent's [`EventRecord`] log rather than
//! codex's `ResponseItem`; the threshold, byte-sizing
//! (`serde_json::to_string(item).len()`, `conversation_history.rs:72-76`), and
//! drop-oldest algorithm are replicated 1:1.
//!
//! Replay-correctness: dropping oldest events can leave a tool-output event
//! whose tool-call was dropped (or vice-versa), but the existing reducer
//! ([`crate::session::reconstruct::provider_messages_from_events`] ->
//! `normalize_provider_messages`) already reconciles those: a tool call with no
//! output gets a synthetic "aborted" output, and a tool output with no call is
//! converted to a context message. So the kept slice always reduces to a valid
//! history with no orphan tool outputs — exactly as codex relies on its prompt
//! assembly to do.

use browser_use_protocol::EventRecord;

/// Default maximum serialized size of a rollout before truncation kicks in.
///
/// Codex parity: `MAX_ROLLOUT_BYTES_BEFORE_TRUNCATION = 5 * 1024 * 1024` (5 MiB),
/// `codex-rs/core/src/conversation_history.rs:11`. (The brief named this
/// `DEFAULT_THREAD_ROLLOUT_MAX_BYTES`; we keep that public name with the real
/// value + citation.)
pub const DEFAULT_THREAD_ROLLOUT_MAX_BYTES: usize = 5 * 1024 * 1024;

/// The outcome of a truncation pass: the kept (now-bounded) events and the
/// archived (oldest, dropped) events. Codex's in-place `drain(0..index)` does
/// not surface the dropped prefix; we return it so the archival seam can persist
/// it durably.
///
/// (`PartialEq` only — `EventRecord`'s `payload: serde_json::Value` is not `Eq`,
/// so this outcome cannot derive `Eq` either.)
#[derive(Debug, Clone, PartialEq)]
pub struct TruncationOutcome {
    /// The events that remain in the bounded rollout, in order.
    pub kept: Vec<EventRecord>,
    /// The events dropped from the head (oldest first).
    pub archived: Vec<EventRecord>,
}

impl TruncationOutcome {
    /// Whether this pass archived anything.
    pub fn did_truncate(&self) -> bool {
        !self.archived.is_empty()
    }
}

/// Serialized byte length of a single event.
///
/// Codex parity: `estimated_item_size_bytes` in
/// `conversation_history.rs:72-76` = `serde_json::to_string(item).len()` (0 on
/// serialize error).
fn estimated_event_size_bytes(event: &EventRecord) -> usize {
    serde_json::to_string(event).map(|s| s.len()).unwrap_or(0)
}

/// Truncate a rollout if its serialized size exceeds `max_bytes`.
///
/// Codex parity: `ConversationHistory::truncate_if_needed`,
/// `conversation_history.rs:44-63`:
/// 1. If total serialized bytes <= `max_bytes`, keep everything (`:46-49`).
/// 2. Otherwise advance `index` over the oldest events, subtracting each one's
///    serialized size, while still over budget AND `index + 1 < len` — i.e.
///    always keep at least the most recent event (`:53-58`).
/// 3. Drop the first `index` events (`:60-62`).
pub fn truncate_rollout_if_needed(events: &[EventRecord], max_bytes: usize) -> TruncationOutcome {
    // Codex parity step 1.
    let mut total_bytes: usize = events.iter().map(estimated_event_size_bytes).sum();
    if total_bytes <= max_bytes {
        return TruncationOutcome {
            kept: events.to_vec(),
            archived: Vec::new(),
        };
    }

    // Codex parity step 2: drop oldest until within budget, always keep >= last.
    let mut index = 0usize;
    while total_bytes > max_bytes && index + 1 < events.len() {
        let item_bytes = estimated_event_size_bytes(&events[index]);
        total_bytes = total_bytes.saturating_sub(item_bytes);
        index += 1;
    }

    // Codex parity step 3.
    let archived = events[..index].to_vec();
    let kept = events[index..].to_vec();
    TruncationOutcome { kept, archived }
}
