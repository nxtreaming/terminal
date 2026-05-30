//! Network-free parity tests for the Rust-native exec-policy engine.
//!
//! Each test cites the codex behavior it mirrors. The heuristics fallback used
//! here is a small stand-in for codex's `render_decision_for_unmatched_command`
//! (codex `core/src/exec_policy.rs:632-750`): a known-safe read command →
//! `Allow` (codex `is_known_safe_command`, `is_safe_command.rs:11`), an obviously
//! destructive command → `Forbidden` (mirroring the agent `shell.rs` rm-rf
//! denylist and codex `command_might_be_dangerous`), everything else → `Prompt`
//! (codex's "prefer to prompt" stance, `exec_policy.rs:670-701`).

use crate::execpolicy::canonicalize_command;
use crate::execpolicy::check_amended;
use crate::execpolicy::default_policy;
use crate::execpolicy::Decision;
use crate::execpolicy::ExecPolicyAmendment;
use crate::execpolicy::ExecPolicyDecision;
use crate::execpolicy::Policy;
use crate::execpolicy::PrefixRule;

fn s(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// Test stand-in for codex's unmatched-command heuristics.
///
/// Codex parity:
/// * known-safe read commands → `Decision::Allow` (codex
///   `render_decision_for_unmatched_command` :645-668 via `is_known_safe_command`,
///   `is_safe_command.rs:63-168`: `ls`, `cat`, `pwd`, `grep`, `git status`, ...).
/// * destructive `rm -rf <root>` → `Decision::Forbidden` (mirrors the agent
///   `shell.rs` `is_root_wipe` denylist and legacy `command.rs:196-218`, and
///   codex `command_might_be_dangerous` feeding `Decision::Forbidden`/`Prompt`,
///   `exec_policy.rs:676-702`). We classify it `Forbidden` directly for the test.
/// * anything else → `Decision::Prompt` (codex prefers to prompt, :670-701).
fn test_heuristics(cmd: &[String]) -> Decision {
    if is_known_safe(cmd) {
        return Decision::Allow;
    }
    if is_root_wipe(cmd) {
        return Decision::Forbidden;
    }
    Decision::Prompt
}

/// Tiny known-safe safelist (subset of codex `is_safe_to_call_with_exec`,
/// `is_safe_command.rs:63-168`).
fn is_known_safe(cmd: &[String]) -> bool {
    let Some(program) = cmd.first().map(|s| base_name(s)) else {
        return false;
    };
    match program {
        "ls" | "cat" | "pwd" | "echo" | "whoami" | "head" | "tail" | "wc" | "true" => true,
        // `git status` / `git log` / `git diff` are read-only (codex
        // is_safe_git_command, is_safe_command.rs:171-196).
        "git" => matches!(
            cmd.get(1).map(String::as_str),
            Some("status") | Some("log") | Some("diff")
        ),
        _ => false,
    }
}

fn base_name(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// Mirror of the agent `shell.rs` `is_root_wipe` (and legacy `command.rs:196-218`):
/// `rm` with a recursive+force flag targeting a filesystem/home root.
fn is_root_wipe(cmd: &[String]) -> bool {
    let Some(first) = cmd.first() else {
        return false;
    };
    if base_name(first) != "rm" {
        return false;
    }
    let joined = format!(" {}", cmd[1..].join(" "));
    let recursive_force = joined.contains(" -rf")
        || joined.contains(" -fr")
        || (joined.contains(" -r") && joined.contains(" -f"))
        || joined.contains(" --recursive");
    if !recursive_force {
        return false;
    }
    const ROOTS: &[&str] = &["/", "/*", "~", "~/", "$HOME", "${HOME}"];
    cmd.iter().skip(1).any(|tok| ROOTS.contains(&tok.as_str()))
}

// ── known-safe read command → Allowed ──────────────────────────────────────

#[test]
fn known_safe_read_command_is_allowed() {
    // Codex parity: `is_known_safe_command(["ls"])` is true (is_safe_command.rs:11,
    // known_safe_examples test :343), so the unmatched-command fallback yields
    // Allow and the engine maps it to Allowed (exec_policy.rs:357).
    let policy = default_policy();
    assert_eq!(
        policy.check(&s(&["ls", "-1"]), &test_heuristics),
        ExecPolicyDecision::Allowed
    );
    assert_eq!(
        policy.check(&s(&["git", "status"]), &test_heuristics),
        ExecPolicyDecision::Allowed
    );
    // Codex parity: `bash -lc "ls"` is also known-safe via the bash unwrap
    // (is_safe_command.rs:37-44, bash_lc_safe_examples :648). Canonicalization
    // lowers it to `["ls"]` first.
    assert_eq!(
        policy.check(&s(&["bash", "-lc", "ls"]), &test_heuristics),
        ExecPolicyDecision::Allowed
    );
}

// ── destructive command → Forbidden { reason } ─────────────────────────────

#[test]
fn destructive_command_is_forbidden_by_rule() {
    // A policy rule forbids `rm -rf` outright. Codex maps Decision::Forbidden to
    // ExecApprovalRequirement::Forbidden { reason } with derive_forbidden_reason
    // (exec_policy.rs:332-334, :964-991).
    let policy = Policy::from_rules(vec![PrefixRule::forbid_prefix(&s(&["rm", "-rf"]))]);
    let decision = policy.check(&s(&["rm", "-rf", "/"]), &test_heuristics);
    match decision {
        ExecPolicyDecision::Forbidden { reason } => {
            assert_eq!(
                reason,
                "`rm -rf /` rejected: policy forbids commands starting with `rm -rf`"
            );
        }
        other => panic!("expected Forbidden, got {other:?}"),
    }
}

#[test]
fn destructive_command_forbidden_by_heuristics() {
    // With no rule, the heuristics fallback forbids `rm -rf /` (mirrors shell.rs
    // is_root_wipe / codex command_might_be_dangerous, exec_policy.rs:676-702).
    let policy = default_policy();
    let decision = policy.check(&s(&["rm", "-rf", "/"]), &test_heuristics);
    match decision {
        ExecPolicyDecision::Forbidden { reason } => {
            // No matched policy rule → codex's "blocked by policy" branch
            // (derive_forbidden_reason, exec_policy.rs:989).
            assert_eq!(reason, "`rm -rf /` rejected: blocked by policy");
        }
        other => panic!("expected Forbidden, got {other:?}"),
    }
}

// ── ambiguous command → Prompt ─────────────────────────────────────────────

#[test]
fn ambiguous_command_prompts() {
    // Codex parity: an unmatched, non-safe, non-dangerous command prefers to
    // prompt (exec_policy.rs:670-701; cargo check is not known-safe,
    // is_safe_command.rs:526). No rule + heuristics Prompt → Prompt.
    let policy = default_policy();
    assert_eq!(
        policy.check(&s(&["cargo", "check"]), &test_heuristics),
        ExecPolicyDecision::Prompt
    );
}

// ── canonicalize: exact-output assertions ──────────────────────────────────

#[test]
fn canonicalize_exact_outputs() {
    // Codex parity: parse_shell_lc_plain_commands / commands_for_exec_policy
    // (bash.rs:114-121, exec_policy.rs:772-781). Exact lowered tokens:
    assert_eq!(
        canonicalize_command(&s(&["bash", "-lc", "ls -1"])),
        s(&["ls", "-1"])
    );
    assert_eq!(
        canonicalize_command(&s(&["sh", "-c", "git status"])),
        s(&["git", "status"])
    );
    // Passthrough for a plain command.
    assert_eq!(
        canonicalize_command(&s(&["grep", "-n", "foo"])),
        s(&["grep", "-n", "foo"])
    );
    // NOT unwrapped: operators / redirection / four-arg / quoting
    // (codex bash_lc_unsafe_examples, is_safe_command.rs:702-739).
    let seq = s(&["bash", "-lc", "ls && rm -rf /"]);
    assert_eq!(canonicalize_command(&seq), seq);
    let four = s(&["bash", "-lc", "git", "status"]);
    assert_eq!(canonicalize_command(&four), four);
}

// ── amendment flips Prompt → Allowed ───────────────────────────────────────

#[test]
fn amendment_flips_prompt_to_allowed() {
    // Codex parity: accepting an ExecPolicyAmendment installs an allow-prefix
    // rule (approvals.rs:32-36, exec_policy.rs:426), so the same command becomes
    // policy-allowed → Skip (exec_policy.rs:357).
    let policy = default_policy();
    let cmd = s(&["cargo", "check"]);

    // Before: ambiguous → Prompt.
    assert_eq!(
        check_amended(&policy, None, &cmd, &test_heuristics),
        ExecPolicyDecision::Prompt
    );

    // After amending to allow the `cargo check` prefix: Allowed.
    let amendment = ExecPolicyAmendment::new(s(&["cargo", "check"]));
    assert_eq!(
        check_amended(&policy, Some(&amendment), &cmd, &test_heuristics),
        ExecPolicyDecision::Allowed
    );
}

#[test]
fn amendment_does_not_override_forbidden_rule() {
    // Codex parity: an amendment only adds an *allow* rule; a forbidden rule with
    // an equal/longer matched prefix still wins via `.max()` (policy.rs:366).
    let policy = Policy::from_rules(vec![PrefixRule::forbid_prefix(&s(&["rm", "-rf"]))]);
    let cmd = s(&["rm", "-rf", "/tmp/x"]);
    let amendment = ExecPolicyAmendment::new(s(&["rm", "-rf"]));
    let decision = check_amended(&policy, Some(&amendment), &cmd, &test_heuristics);
    assert!(
        matches!(decision, ExecPolicyDecision::Forbidden { .. }),
        "forbidden rule must still win over an allow amendment, got {decision:?}"
    );
}

// ── default policy baseline ────────────────────────────────────────────────

#[test]
fn default_policy_is_empty_and_defers_to_heuristics() {
    // Codex ships a Starlark default.rules; we cannot evaluate it, so the default
    // policy is empty and every decision flows through the heuristics fallback
    // (codex no-rule-matched path, policy.rs:285-294). Documented parity debt.
    let policy = default_policy();
    assert!(policy.allowed_prefixes().is_empty());
    // Safe → Allowed, destructive → Forbidden, other → Prompt, all via fallback.
    assert_eq!(
        policy.check(&s(&["pwd"]), &test_heuristics),
        ExecPolicyDecision::Allowed
    );
    assert!(matches!(
        policy.check(&s(&["rm", "-rf", "~"]), &test_heuristics),
        ExecPolicyDecision::Forbidden { .. }
    ));
    assert_eq!(
        policy.check(&s(&["make", "install"]), &test_heuristics),
        ExecPolicyDecision::Prompt
    );
}
