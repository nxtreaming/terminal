//! Hook configuration: matcher groups, hook commands, and matching logic.
//!
//! Parity sources:
//! - codex `core/src/hooks_config.rs`: `HooksConfig` (event-name -> matcher
//!   groups), `HookMatcherGroup` (`matcher` + `hooks`), `HookCommand` (the
//!   `{ type: "command", command, timeout }` shape), and `HookMatcherGroup::matches`
//!   (`/home/exedev/repos/codex/codex-rs/core/src/hooks_config.rs:15-74`).
//! - legacy `browser-use-core`: `HookMatcherGroup`
//!   (`crates/browser-use-core/src/lib.rs:6833-6837`), `HookCommandConfig`
//!   (`6841-6847`), `HookConfig` (`6901-6904`), and `matches_hook`
//!   (`6854-6865`). Legacy + codex agree on event names, matcher-group shape,
//!   and matcher semantics, so those wire shapes are authoritative.

use std::collections::HashMap;

use serde::Deserialize;
use serde::Serialize;

use super::event::HookEvent;

/// Top-level hooks configuration, keyed by event name.
///
/// Mirrors codex `HooksConfig`
/// (`/home/exedev/repos/codex/codex-rs/core/src/hooks_config.rs:15-18`) and
/// legacy `HookConfig` (`crates/browser-use-core/src/lib.rs:6901-6904`): each
/// event name maps to a list of matcher groups. Keyed by `String` (the codex
/// shape) so unknown/future event names round-trip without loss.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HooksConfig {
    /// Event name -> list of matcher groups.
    pub groups: HashMap<String, Vec<HookMatcherGroup>>,
}

impl HooksConfig {
    /// Construct from a raw map.
    pub fn from_groups(groups: HashMap<String, Vec<HookMatcherGroup>>) -> Self {
        Self { groups }
    }

    /// Return the matcher groups registered for `event`, if any.
    ///
    /// Mirrors codex `HooksConfig::groups_for`
    /// (`/home/exedev/repos/codex/codex-rs/core/src/hooks_config.rs:22-27`).
    pub fn groups_for(&self, event: HookEvent) -> &[HookMatcherGroup] {
        self.groups
            .get(event.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Insert (replace) the matcher groups for `event`.
    pub fn set_groups(&mut self, event: HookEvent, groups: Vec<HookMatcherGroup>) {
        self.groups.insert(event.as_str().to_string(), groups);
    }
}

/// A single matcher group: a matcher pattern plus the hooks to run.
///
/// Mirrors codex `HookMatcherGroup`
/// (`/home/exedev/repos/codex/codex-rs/core/src/hooks_config.rs:31-39`) and
/// legacy `HookMatcherGroup` (`crates/browser-use-core/src/lib.rs:6833-6837`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookMatcherGroup {
    /// Matcher pattern. Semantics depend on the event: for tool events it
    /// matches the tool name; empty/`*`/absent matches all.
    #[serde(default)]
    pub matcher: Option<String>,
    /// The hooks (commands) to execute when this group matches.
    pub hooks: Vec<HookCommand>,
}

impl HookMatcherGroup {
    /// True when this group applies to `subject` (e.g. a tool name).
    ///
    /// Full regex parity with legacy `hook_matcher_matches`
    /// (`crates/browser-use-core/src/lib.rs:8354-8367`) +
    /// `hook_matcher_is_exact` (`:8369-8373`); see [`matcher_matches`] for the
    /// exact rules.
    pub fn matches(&self, subject: &str) -> bool {
        matcher_matches(self.matcher.as_deref(), subject)
    }
}

/// Free-function matcher used by [`HookMatcherGroup::matches`].
///
/// Byte-for-byte parity with legacy `hook_matcher_matches`
/// (`crates/browser-use-core/src/lib.rs:8354-8367`):
/// - `None`, empty, or `*` matches everything (the matcher is trimmed first,
///   matching legacy `matcher.map(str::trim)` at `:8355`).
/// - if the matcher contains only exact-name characters
///   ([`matcher_is_exact`], legacy `hook_matcher_is_exact` `:8369-8373`), it is
///   treated as a `|`-separated alternation of literal names, matching when
///   `subject` equals any part (legacy `:8362`). This is the fast path for
///   plain names like `Bash` and exact alternations like `Edit|Write` and is
///   why `Edit` does NOT match `Edited`.
/// - otherwise the matcher is compiled as a regex and tested against `subject`
///   (legacy `:8364-8366`, `regex::Regex::new(matcher).is_match(candidate)`).
///   This makes real patterns like `Bash.*` and `mcp__.*` work.
/// - LEGACY INVALID-PATTERN BEHAVIOR (cited, replicated): when
///   `Regex::new(matcher)` fails to compile, legacy returns `false` via
///   `.unwrap_or(false)` (`:8366`) — an uncompilable pattern matches NOTHING.
pub fn matcher_matches(matcher: Option<&str>, subject: &str) -> bool {
    let Some(matcher) = matcher.map(str::trim) else {
        return true;
    };
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    if matcher_is_exact(matcher) {
        return matcher.split('|').any(|part| part == subject);
    }
    regex::Regex::new(matcher)
        .map(|re| re.is_match(subject))
        .unwrap_or(false)
}

/// True when `matcher` contains only exact-name characters (ASCII alphanumeric,
/// `_`, or `|`), so it can be compared by literal alternation without compiling
/// a regex.
///
/// Byte-for-byte parity with legacy `hook_matcher_is_exact`
/// (`crates/browser-use-core/src/lib.rs:8369-8373`).
pub fn matcher_is_exact(matcher: &str) -> bool {
    matcher
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '|')
}

/// A single hook command definition.
///
/// Mirrors codex `HookCommand`
/// (`/home/exedev/repos/codex/codex-rs/core/src/hooks_config.rs:63-74`): an
/// internally-tagged enum (`type: "command"`) carrying the shell command line
/// and an optional per-hook timeout (seconds). Legacy `HookCommandConfig`
/// (`crates/browser-use-core/src/lib.rs:6841-6847`) uses a struct with a
/// `type` field defaulting to `"command"`; the codex tagged-enum form is the
/// one mirrored here (it is the more precise shape and the one the codex tests
/// construct).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HookCommand {
    /// Run an external command.
    Command {
        /// The shell command line to execute.
        command: String,
        /// Optional per-hook timeout in seconds.
        #[serde(default)]
        timeout: Option<u64>,
    },
}

impl HookCommand {
    /// Convenience constructor for a command hook.
    pub fn command(command: impl Into<String>, timeout: Option<u64>) -> Self {
        HookCommand::Command {
            command: command.into(),
            timeout,
        }
    }

    /// The shell command line.
    pub fn command_line(&self) -> &str {
        match self {
            HookCommand::Command { command, .. } => command,
        }
    }

    /// The configured per-hook timeout in seconds, if any.
    pub fn timeout_secs(&self) -> Option<u64> {
        match self {
            HookCommand::Command { timeout, .. } => *timeout,
        }
    }
}
