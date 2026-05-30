//! Pure tool-dispatch parallelism decision (codex `parallel.rs`, `turn.rs:872`).
//!
//! The other pure tool decisions (`default_exec_approval_requirement`,
//! `sandbox_override_for_first_attempt`, `plan_attempts`, `map_decision`) live in
//! the `tools` module because they reference its enums, but stay equally pure.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolParallelism {
    /// `RwLock` read — may run concurrently with other parallel-safe calls.
    Parallel,
    /// `RwLock` write — runs exclusively.
    Serial,
}

pub fn classify_parallelism(parallel_safe: bool, model_supports: bool) -> ToolParallelism {
    if parallel_safe && model_supports {
        ToolParallelism::Parallel
    } else {
        ToolParallelism::Serial
    }
}
