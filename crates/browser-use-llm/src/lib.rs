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
pub mod schema;

pub use schema::*;
