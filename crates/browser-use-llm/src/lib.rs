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
/// Credential helpers for on-disk login state.
///
/// **DEV/TEST ONLY** and feature-gated (`codex-dev`): the only thing here is the
/// Codex (`~/.codex/auth.json`) reader, kept solely as a dev smoke-test vehicle.
/// The codex/ChatGPT backend is being CUT from production, so this is not a
/// production code path and is not compiled by default. Production credentials
/// come from standard provider env keys (see `browser-use-agent`'s
/// `turn::model_path`).
#[cfg(feature = "codex-dev")]
pub mod auth;
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
