//! `browser-use-agent` — the async agent engine (rearchitecture Milestone 3).
//!
//! Strategy **B (parallel rewrite)**: this crate is built alongside the legacy
//! synchronous `browser-use-core`, on top of the async, provider-neutral
//! `browser-use-llm`. Subsystems are ported here one at a time, each with
//! codex-parity tests, and the TUI/CLI are switched over to this engine only
//! once parity is reached. Until then `browser-use-core` remains the live engine.
//!
//! This module tree is the **frozen interface scaffold** (WP-A0): every type,
//! trait, and function signature is the contract later work packages fill in.
//! Bodies are `unimplemented!()` / trivial; only the shapes are load-bearing.
//!
//! Layering:
//! - [`decision`] — PURE sync decision core (the unit-test surface).
//! - [`events`]   — sync `EventSink` fan-out + pure stream-event mapper.
//! - [`context`]  — `ContextManager` + real token accounting (pure core + thin async).
//! - [`compact`]  — model-based context compaction (codex `compact.rs` parity).
//! - [`tools`]    — `ToolOrchestrator` + runtime/approval/sandbox seam.
//! - [`turn`]     — the async turn loop + in-turn tool dispatch.
//! - [`task`]     — async task driver / lifecycle.
//! - [`session`]  — lifecycle, resume-by-replay, fork/rollback.
//! - [`testkit`]  — deterministic fakes shared by every WP's tests.

pub mod compact;
pub mod config;
pub mod context;
pub mod decision;
pub mod error;
pub mod events;
pub mod goals;
pub mod hooks;
pub mod mcp;
pub mod prompts;
pub mod rollout;
pub mod session;
pub mod skills;
pub mod subagents;
pub mod task;
pub mod testkit;
pub mod tools;
pub mod turn;

pub use config::AgentConfig;
pub use error::AgentError;

#[cfg(test)]
mod tests {
    /// Scaffold smoke test: the crate builds and the async test harness runs.
    #[tokio::test]
    async fn crate_builds_and_async_harness_runs() {
        assert_eq!(2 + 2, 4);
    }
}
