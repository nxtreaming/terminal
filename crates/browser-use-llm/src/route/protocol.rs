//! `Protocol` — the wire-format contract every provider implements.
//!
//! A protocol does two things, both **synchronous and testable in isolation**:
//! 1. `build_body` — lower the canonical [`LlmRequest`] to the provider's native
//!    JSON request body.
//! 2. `decoder` — hand back a stateful [`ProtocolStream`] that turns the SSE
//!    frames of one response into normalized [`LlmEvent`]s (typically using the
//!    shared `Lifecycle` / `ToolStream` helpers).
//!
//! The async client (`route::client`) owns transport, retries and auth; it never
//! needs to know which provider it is talking to — it just drives a `Protocol`.

use serde_json::Value;

use super::framing::SseFrame;
use crate::schema::{LlmError, LlmEvent, LlmRequest};

/// Stateful per-response stream decoder.
pub trait ProtocolStream: Send {
    /// Decode one SSE frame into zero or more normalized events.
    fn on_frame(&mut self, frame: &SseFrame) -> Result<Vec<LlmEvent>, LlmError>;

    /// The stream ended; flush any open blocks / pending tool calls and emit the
    /// terminal `Finish` (idempotent).
    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError>;
}

/// A wire format (OpenAI Responses, OpenAI Chat, Anthropic Messages, …).
pub trait Protocol: Send + Sync {
    /// Lower the canonical request to this protocol's native JSON body.
    fn build_body(&self, req: &LlmRequest) -> Result<Value, LlmError>;

    /// A fresh decoder for one streamed response.
    fn decoder(&self) -> Box<dyn ProtocolStream>;

    /// All current protocols stream via SSE; override if a protocol returns a
    /// single non-streamed JSON body instead.
    fn is_streaming(&self) -> bool {
        true
    }
}
