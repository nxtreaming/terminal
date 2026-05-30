//! Policy amendments.
//!
//! Codex parity: an amendment is a command prefix that should be added to the
//! policy as an **allow** prefix rule. The wire type is
//! `codex_protocol::approvals::ExecPolicyAmendment { command: Vec<String> }`
//! (codex `protocol/src/approvals.rs:37-58`), documented as "the prefix that
//! would be added as an execpolicy `prefix_rule(..., decision="allow")`"
//! (codex `approvals.rs:32-36`). Applying it is
//! `ExecPolicyManager::append_amendment_and_update`, which calls
//! `add_prefix_rule(&amendment.command, Decision::Allow)` and swaps in the
//! updated policy (codex `core/src/exec_policy.rs:381-429`, specifically :426).
//!
//! This module mirrors the **in-memory** half of that flow (the part that affects
//! a subsequent `check`). Codex additionally persists the rule to a `.rules` file
//! via `blocking_append_allow_prefix_rule` (codex `execpolicy/src/amend.rs:65-81`)
//! — that writes a **Starlark** `prefix_rule(...)` line, which we do not support
//! (see [`crate::execpolicy::policy`] module docs). On-disk persistence is
//! therefore deferred; the amendment here updates the live policy only.

use crate::execpolicy::canonicalize::canonicalize_command;
use crate::execpolicy::policy::Decision;
use crate::execpolicy::policy::ExecPolicyDecision;
use crate::execpolicy::policy::Policy;
use crate::execpolicy::policy::PrefixRule;

/// A proposed/accepted change to the policy: allow commands starting with this
/// prefix.
///
/// Codex parity: `ExecPolicyAmendment` (codex `protocol/src/approvals.rs:37-52`),
/// a transparent newtype over `Vec<String>` with `new` / `command` / `From`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecPolicyAmendment {
    /// The prefix tokens to allow.
    pub command: Vec<String>,
}

impl ExecPolicyAmendment {
    /// Codex parity: `ExecPolicyAmendment::new` (codex `approvals.rs:45-47`).
    pub fn new(command: Vec<String>) -> Self {
        Self { command }
    }

    /// Codex parity: `ExecPolicyAmendment::command` (codex `approvals.rs:49-51`).
    pub fn command(&self) -> &[String] {
        &self.command
    }
}

impl From<Vec<String>> for ExecPolicyAmendment {
    /// Codex parity: `impl From<Vec<String>> for ExecPolicyAmendment`
    /// (codex `approvals.rs:54-58`).
    fn from(command: Vec<String>) -> Self {
        Self { command }
    }
}

impl Policy {
    /// Apply an amendment in memory: add `amendment.command` as an allow-prefix
    /// rule, unless an equivalent allow-prefix is already present.
    ///
    /// Codex parity: `append_amendment_and_update` first checks whether the
    /// command is `already_allowed` and returns early if so (codex
    /// `exec_policy.rs:412-423`), otherwise clones the policy and calls
    /// `add_prefix_rule(&amendment.command, Decision::Allow)` (codex :425-427).
    /// An empty prefix is rejected, mirroring `add_prefix_rule`'s
    /// `Error::InvalidPattern("prefix cannot be empty")` (codex `policy.rs:92-94`)
    /// and `AmendError::EmptyPrefix` (codex `amend.rs:69-71`); here we no-op.
    pub fn amend(&mut self, amendment: &ExecPolicyAmendment) {
        if amendment.command.is_empty() {
            return;
        }
        // Dedup: skip if this exact allow-prefix already exists (codex's
        // `already_allowed` short-circuit, exec_policy.rs:417-423).
        if self
            .allowed_prefixes()
            .iter()
            .any(|prefix| prefix == &amendment.command)
        {
            return;
        }
        self.push_rule(PrefixRule::allow_prefix(&amendment.command));
    }

    /// Convenience: return a *new* policy with the amendment applied.
    pub fn amended(&self, amendment: &ExecPolicyAmendment) -> Policy {
        let mut policy = self.clone();
        policy.amend(amendment);
        policy
    }
}

/// Canonicalize, apply `amendment` (if any), then check `cmd`.
///
/// This composes the three layers exactly in codex's order: an amendment is
/// installed as an allow-prefix rule (codex `exec_policy.rs:426`), then the
/// command is lowered (codex `commands_for_exec_policy`, `exec_policy.rs:772`)
/// and evaluated against the (now amended) policy with the heuristics fallback
/// (codex `policy.rs:188-198`). Because an amendment installs an **allow** rule
/// and allow is the least-restrictive decision, a previously-`Prompt`/`Forbidden`
/// *heuristic* outcome flips to `Allowed` once a matching allow-prefix exists —
/// matching codex, where the added allow rule makes the command policy-allowed
/// (codex `exec_policy.rs:357` `Decision::Allow => ExecApprovalRequirement::Skip`).
///
/// Note: codex never amends *over* a forbidden **rule** (an amendment only adds
/// an allow rule; a forbidden rule with a longer/equal matched prefix still wins
/// via `.max()`, codex `policy.rs:366`). The same holds here.
pub fn check_amended<F>(
    policy: &Policy,
    amendment: Option<&ExecPolicyAmendment>,
    cmd: &[String],
    heuristics: &F,
) -> ExecPolicyDecision
where
    F: Fn(&[String]) -> Decision,
{
    let canonical = canonicalize_command(cmd);
    match amendment {
        Some(amendment) => {
            let amended = policy.amended(amendment);
            amended.check_canonical(&canonical, heuristics)
        }
        None => policy.check_canonical(&canonical, heuristics),
    }
}

#[cfg(test)]
mod amend_unit_tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn amendment_from_vec_roundtrip() {
        let amendment = ExecPolicyAmendment::from(s(&["cargo", "build"]));
        assert_eq!(amendment.command(), s(&["cargo", "build"]).as_slice());
        assert_eq!(amendment, ExecPolicyAmendment::new(s(&["cargo", "build"])));
    }

    #[test]
    fn amend_is_idempotent() {
        let amendment = ExecPolicyAmendment::new(s(&["cargo", "build"]));
        let mut policy = Policy::empty();
        policy.amend(&amendment);
        policy.amend(&amendment);
        assert_eq!(policy.allowed_prefixes(), vec![s(&["cargo", "build"])]);
    }

    #[test]
    fn empty_amendment_is_noop() {
        let mut policy = Policy::empty();
        policy.amend(&ExecPolicyAmendment::new(Vec::new()));
        assert!(policy.allowed_prefixes().is_empty());
    }

    #[test]
    fn amendment_flips_prompt_to_allowed() {
        // Heuristics would Prompt on an unknown command; the amendment installs
        // an allow-prefix so it becomes Allowed (codex exec_policy.rs:357,426).
        let policy = Policy::empty();
        let cmd = s(&["cargo", "build", "--release"]);
        assert_eq!(
            check_amended(&policy, None, &cmd, &|_| Decision::Prompt),
            ExecPolicyDecision::Prompt
        );
        let amendment = ExecPolicyAmendment::new(s(&["cargo", "build"]));
        assert_eq!(
            check_amended(&policy, Some(&amendment), &cmd, &|_| Decision::Prompt),
            ExecPolicyDecision::Allowed
        );
    }
}
