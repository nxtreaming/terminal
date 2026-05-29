//! Reusable, provider-agnostic stream-normalization helpers.
//!
//! These keep each protocol's stream `step` tiny and uniform:
//! - [`Lifecycle`] guarantees a well-formed text/reasoning block sequence.
//! - [`ToolStream`] accumulates streamed tool-call arguments into a `ToolCall`.

pub mod lifecycle;
pub mod tool_stream;

pub use lifecycle::Lifecycle;
pub use tool_stream::ToolStream;
