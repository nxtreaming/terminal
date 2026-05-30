//! Spawn-depth limits (codex `agent/registry.rs` parity).
//!
//! Pure helpers that bound how deeply sub-agents may spawn further sub-agents.
//! Codex computes the child depth as `parent_depth + 1` and rejects a spawn when
//! that child depth exceeds the configured maximum.
//!
//! Parity:
//! - `core/src/config/mod.rs:195` `DEFAULT_AGENT_MAX_DEPTH = 1` (+ field
//!   `agent_max_depth` at `:797`).
//! - `core/src/agent/registry.rs:71-73` `next_thread_spawn_depth(src) =
//!   session_depth(src).saturating_add(1)`.
//! - `core/src/agent/registry.rs:75-77` `exceeds_thread_spawn_depth_limit(depth,
//!   max) = depth > max`.
//! - depth enforcement at the spawn handler `core/src/tools/handlers/
//!   multi_agents_common.rs:284`.

/// Default maximum spawn depth (codex `config/mod.rs:195`
/// `DEFAULT_AGENT_MAX_DEPTH = 1`).
///
/// A root agent has depth `0`; with a max of `1`, root may spawn children (depth
/// `1`) but those children may not spawn further (their children would be depth
/// `2 > 1`).
pub const DEFAULT_AGENT_MAX_DEPTH: i32 = 1;

/// Compute the depth a child spawned by a parent at `parent_depth` would have
/// (codex `next_thread_spawn_depth` = `session_depth(src).saturating_add(1)`).
///
/// Saturating so a pathological `i32::MAX` parent depth cannot wrap.
pub fn next_spawn_depth(parent_depth: i32) -> i32 {
    parent_depth.saturating_add(1)
}

/// `true` iff a child at `depth` violates the `max_depth` limit
/// (codex `exceeds_thread_spawn_depth_limit` = `depth > max`).
pub fn exceeds_depth_limit(depth: i32, max_depth: i32) -> bool {
    depth > max_depth
}

#[cfg(test)]
mod depth_unit_tests {
    use super::*;

    #[test]
    fn next_spawn_depth_increments() {
        assert_eq!(next_spawn_depth(0), 1);
        assert_eq!(next_spawn_depth(1), 2);
        assert_eq!(next_spawn_depth(5), 6);
    }

    #[test]
    fn next_spawn_depth_saturates() {
        assert_eq!(next_spawn_depth(i32::MAX), i32::MAX);
    }

    #[test]
    fn exceeds_depth_limit_is_strict_greater_than() {
        // At max: allowed.
        assert!(!exceeds_depth_limit(1, DEFAULT_AGENT_MAX_DEPTH));
        // Above max: rejected.
        assert!(exceeds_depth_limit(2, DEFAULT_AGENT_MAX_DEPTH));
        // Below max: allowed.
        assert!(!exceeds_depth_limit(0, DEFAULT_AGENT_MAX_DEPTH));
    }
}
