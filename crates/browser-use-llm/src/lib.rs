//! `browser-use-llm` — provider-neutral LLM core (rearchitecture Phase 1).
//!
//! This crate implements the multi-provider model layer described in
//! `REARCHITECTURE.md` §3, following the opencode design: a typed canonical
//! request/message/event model (`schema`), composed at runtime by a
//! `protocol × provider` routing layer (added in later work packages).
//!
//! Phase 1.1 (this file set) is the **schema** layer only: the typed shapes
//! every protocol lowers to / normalizes from. It has no provider, no I/O, and
//! no `async` — it is pure data and is intentionally testable in isolation.
pub mod protocols;
pub mod providers;
pub mod route;
pub mod schema;
pub mod tool;
pub mod tool_runtime;

pub use providers::{Anthropic, AnthropicConfig, OpenAi, OpenAiCompatible, OpenAiConfig};
pub use schema::*;
pub use tool::{Tool, ToolFailure, ToolHandler, ToolResult, ToolSet};
pub use tool_runtime::{run_tool_loop, LoopOutput, LoopStatus, ScriptedTurnSource, TurnSource};
