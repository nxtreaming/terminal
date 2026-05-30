//! Command exec-policy engine (WP-Safety-2), ported to **codex decision
//! semantics** with a **dependency-light, Rust-native** rule format.
//!
//! # Starlark-vs-Rust-native (LOUD PARITY-DEBT FLAG)
//!
//! Codex's `execpolicy` crate authors policies in **Starlark** and evaluates them
//! with the heavy `starlark` crate (codex `execpolicy/src/parser.rs:3-13`,
//! `policy.rs`, `rule.rs`, `decision.rs`). The `starlark` crate is a codex
//! workspace dependency but is **NOT** in this workspace
//! (`grep -c starlark terminal-decodex/Cargo.toml` == 0); adding it would pull in
//! a large parser/interpreter. **Decision:** this port does **not** add
//! `starlark`. Rules are expressed Rust-native ([`policy::PrefixRule`] /
//! [`policy::Policy`]) instead of a `.rules` Starlark file.
//!
//! **What is preserved (codex decision semantics):**
//! * `Decision { Allow, Prompt, Forbidden }` with `Forbidden > Prompt > Allow`
//!   and most-restrictive-wins aggregation — codex `decision.rs:9-16`,
//!   `policy.rs:366`.
//! * Program-keyed prefix matching with `Single`/`Alts` token matchers — codex
//!   `rule.rs:16-60`, `policy.rs:297-305`.
//! * Heuristics fallback for unmatched commands — codex `policy.rs:285-294`,
//!   fed (in codex) by `is_known_safe_command` / `command_might_be_dangerous`
//!   (codex `exec_policy.rs:632-702`).
//! * Engine→approval mapping `Forbidden{reason}` / `NeedsApproval` / `Skip`
//!   ([`policy::ExecPolicyDecision`]) — codex `exec_policy.rs:331-378`, with the
//!   forbidden-reason text from `derive_forbidden_reason` (codex :964-991).
//! * `bash -lc` canonicalization before checking — codex
//!   `commands_for_exec_policy` (`exec_policy.rs:772-810`) /
//!   `parse_shell_lc_plain_commands` (`bash.rs:114-121`).
//! * Amendments as allow-prefix rules + apply/compose — codex
//!   `ExecPolicyAmendment` (`approvals.rs:37-58`) +
//!   `append_amendment_and_update` (`exec_policy.rs:381-429`).
//!
//! **What differs / is deferred (caveats):**
//! * **Authoring format**: Rust constructors, not Starlark `.rules` files.
//! * **Default policy is empty**: codex ships `default.rules` (Starlark); since we
//!   cannot evaluate it, every decision is driven by the heuristics fallback,
//!   mirroring codex's no-rule-matched path. Hard destructive denials live in the
//!   heuristics fallback (mirroring the agent's `shell.rs` rm-rf denylist and
//!   legacy `command.rs:153-218`).
//! * **Canonicalization** unwraps only the single-plain-command `bash -lc "..."`
//!   case with a conservative tokenizer (no tree-sitter); multi-command/operator
//!   scripts are left wrapped (the safe direction). See
//!   [`canonicalize`] module docs.
//! * **On-disk amendment persistence** (Starlark `prefix_rule(...)` line written
//!   by codex `execpolicy/src/amend.rs:65-81`) is not implemented; amendments
//!   update the live policy only.
//! * **Network rules**, host-executable resolution, and config-layer loading
//!   (codex `policy.rs:113-186`, `exec_policy.rs:572-629`) are out of scope.
//! * **shell.rs → execpolicy wiring is deferred** (a later WP). The agent's
//!   `tools/handlers/shell.rs` keeps its inline rm-rf denylist
//!   (`dangerous_command_rejection`); this engine is the general replacement but
//!   is not yet wired into the shell tool.

pub mod amend;
pub mod canonicalize;
pub mod policy;

pub use amend::check_amended;
pub use amend::ExecPolicyAmendment;
pub use canonicalize::canonicalize_command;
pub use policy::default_policy;
pub use policy::Decision;
pub use policy::ExecPolicyDecision;
pub use policy::PatternToken;
pub use policy::Policy;
pub use policy::PrefixRule;

#[cfg(test)]
mod tests;
