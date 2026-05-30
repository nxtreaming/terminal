//! Hook runtime: matcher-group command hooks fired at lifecycle moments.
//!
//! This is the de-codex async port of codex's hook subsystem
//! (`codex-rs/core/src/{hooks,hooks_config,hook_runtime}.rs`) and the legacy
//! `browser-use-core` hook system (`HookEventName` / `HookMatcherGroup` /
//! `matches_hook` / subagent hook dispatch). It provides:
//!
//! - [`event`]: the [`HookEvent`] kinds (codex/legacy parity + the
//!   user-requested `PermissionRequest`, `Prompt`, and `Agent` additions), the
//!   [`HookInput`] payload a hook receives on stdin, and the [`HookDecision`]
//!   a hook returns (allow / deny / inject-context).
//! - [`config`]: [`HooksConfig`] (event-name -> matcher groups),
//!   [`HookMatcherGroup`] (matcher pattern + commands), [`HookCommand`], and
//!   the matcher semantics.
//! - [`runtime`]: the [`HookRuntime`] — selects matching hooks for an event,
//!   runs their commands through the [`CommandRunner`] seam (production =
//!   real `/bin/sh -c` spawn via [`ShellCommandRunner`]; tests = a fake),
//!   folds outcomes (deny short-circuits, context is collected), enforces a
//!   per-hook timeout, and emits the sanctioned `PermissionRequest`
//!   `PendingEvent` through an injected [`crate::events::EventSink`].
//!
//! INTEGRATION (parity debt, not wired here): the [`HookRuntime`] is a
//! self-contained subsystem. Wiring it into the turn loop (PreToolUse/PostToolUse
//! around tool dispatch, UserPromptSubmit/Prompt at the prompt boundary,
//! Stop/Agent at turn boundaries via `task/lifecycle.rs`, SubagentStart/Stop via
//! `subagents/`, and `PermissionRequest` into the approval flow in
//! `tools/approval.rs`) is a later integration WP.

pub mod config;
pub mod event;
pub mod runtime;

pub use config::matcher_is_exact;
pub use config::matcher_matches;
pub use config::HookCommand;
pub use config::HookMatcherGroup;
pub use config::HooksConfig;
pub use event::HookDecision;
pub use event::HookEvent;
pub use event::HookInput;
pub use runtime::default_runner;
pub use runtime::CommandOutput;
pub use runtime::CommandRunner;
pub use runtime::HookOutcome;
pub use runtime::HookRunResult;
pub use runtime::HookRuntime;
pub use runtime::ShellCommandRunner;
pub use runtime::DEFAULT_HOOK_TIMEOUT_SECS;
pub use runtime::PERMISSION_REQUEST_EVENT;

#[cfg(test)]
mod tests;
