//! Dependency-light, Rust-native exec-policy engine.
//!
//! # PARITY DEBT — Starlark authoring format NOT supported (LOUD FLAG)
//!
//! Codex's `execpolicy` crate authors its rules in **Starlark** and evaluates
//! them with the heavy `starlark` crate (see codex
//! `execpolicy/src/parser.rs:3-13`, which imports `starlark::syntax::AstModule`,
//! `starlark::eval::Evaluator`, `starlark_module`, etc.). The `starlark` crate is
//! a codex workspace dependency but is **NOT** present in this workspace
//! (`grep -c starlark terminal-decodex/Cargo.toml` == 0), and pulling it in would
//! add a large parser/interpreter dependency tree.
//!
//! Therefore this port **expresses rules Rust-native** (Rust constructors instead
//! of a `.rules` Starlark file) while preserving codex's **decision semantics**:
//!
//! * `Decision { Allow, Prompt, Forbidden }` mirrors codex
//!   `execpolicy/src/decision.rs:9-16`, including the derived `Ord` where
//!   `Forbidden > Prompt > Allow`, so the **most restrictive** matched decision
//!   wins (codex `policy.rs:366` `matched_rules.iter().map(RuleMatch::decision).max()`).
//! * Program-keyed prefix rules with per-token matchers (`Single` / `Alts`)
//!   mirror codex `rule.rs:16-60` (`PatternToken`, `PrefixPattern::matches_prefix`)
//!   and `policy.rs:297-305` (`match_exact_rules`, keyed by the first token).
//! * When no rule matches, a heuristics fallback supplies the decision, mirroring
//!   codex `policy.rs:285-294` (`HeuristicsRuleMatch`).
//! * The engine→approval mapping ([`ExecPolicyDecision`]) mirrors codex
//!   `core/src/exec_policy.rs:331-378`
//!   (`Decision::Forbidden → ExecApprovalRequirement::Forbidden { reason }`,
//!   `Decision::Prompt → NeedsApproval`, `Decision::Allow → Skip`).
//!
//! **Authoring format differs (Rust vs Starlark `.rules`); decision behavior
//! matches codex for the cases covered by `tests.rs`.** Faithfully porting the
//! Starlark front-end (and adding the `starlark` dependency) is deferred.

use crate::execpolicy::canonicalize::canonicalize_command;

/// Decision for a single matched rule.
///
/// Codex parity: `Decision` (codex `execpolicy/src/decision.rs:9-16`). The
/// derived `Ord` orders `Allow < Prompt < Forbidden`, so taking `.max()` over the
/// matched rules selects the most restrictive decision exactly like codex
/// `Evaluation::from_matches` (codex `policy.rs:365-374`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Decision {
    /// Command may run without further approval.
    Allow,
    /// Request explicit user approval.
    Prompt,
    /// Command is blocked without further consideration.
    Forbidden,
}

/// Outcome of evaluating a command against the policy, in the vocabulary of the
/// runtime approval seam.
///
/// Codex parity: this mirrors the engine→runtime mapping in codex
/// `core/src/exec_policy.rs:331-378`, where the engine's [`Decision`] is mapped
/// onto `ExecApprovalRequirement`:
/// * `Decision::Forbidden` → `ExecApprovalRequirement::Forbidden { reason }`
///   (codex `exec_policy.rs:332-334`, reason from `derive_forbidden_reason`).
/// * `Decision::Prompt` → `ExecApprovalRequirement::NeedsApproval { .. }`
///   (codex `exec_policy.rs:335-356`).
/// * `Decision::Allow` → `ExecApprovalRequirement::Skip { .. }`
///   (codex `exec_policy.rs:357-377`).
///
/// We keep [`ExecPolicyDecision::Allowed`] / [`ExecPolicyDecision::Prompt`]
/// /[`ExecPolicyDecision::Forbidden`] as the pure engine surface; wiring this to
/// the agent's `ExecApprovalRequirement` (and to `shell.rs`) is a later concern.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecPolicyDecision {
    /// The command is allowed to run (codex `Decision::Allow`).
    Allowed,
    /// The command requires user approval (codex `Decision::Prompt`).
    Prompt,
    /// The command is forbidden; `reason` is a human-readable explanation,
    /// mirroring codex `derive_forbidden_reason` (codex `exec_policy.rs:964-991`).
    Forbidden { reason: String },
}

impl ExecPolicyDecision {
    /// Build an [`ExecPolicyDecision`] from a raw engine [`Decision`] plus the
    /// originating command (used to render a forbidden reason).
    ///
    /// Codex parity: the `match evaluation.decision { .. }` arm in codex
    /// `exec_policy.rs:331-378`. For the forbidden reason we mirror the
    /// "no policy justification" branch of `derive_forbidden_reason`
    /// (codex `exec_policy.rs:985-990`): when a matched forbidden rule has a
    /// matched prefix, the reason names the forbidden prefix; otherwise it falls
    /// back to "blocked by policy".
    fn from_decision(
        decision: Decision,
        command: &[String],
        matched_prefix: Option<&[String]>,
        justification: Option<&str>,
    ) -> Self {
        match decision {
            Decision::Allow => ExecPolicyDecision::Allowed,
            Decision::Prompt => ExecPolicyDecision::Prompt,
            Decision::Forbidden => ExecPolicyDecision::Forbidden {
                reason: derive_forbidden_reason(command, matched_prefix, justification),
            },
        }
    }
}

/// Render a forbidden-reason string.
///
/// Codex parity: `derive_forbidden_reason` (codex `core/src/exec_policy.rs:964-991`).
/// Codex renders the command with shlex; we use a simple space join (shlex is not
/// a dependency-light requirement here and the cases we test contain no spaces).
/// The three branches match codex exactly:
/// * justification present → `` `{command}` rejected: {justification} `` (codex :982-984)
/// * matched prefix, no justification → `` `{command}` rejected: policy forbids
///   commands starting with `{prefix}` `` (codex :985-988)
/// * nothing matched → `` `{command}` rejected: blocked by policy `` (codex :989)
fn derive_forbidden_reason(
    command: &[String],
    matched_prefix: Option<&[String]>,
    justification: Option<&str>,
) -> String {
    let rendered = command.join(" ");
    match (matched_prefix, justification) {
        (_, Some(justification)) => format!("`{rendered}` rejected: {justification}"),
        (Some(prefix), None) => {
            let prefix = prefix.join(" ");
            format!("`{rendered}` rejected: policy forbids commands starting with `{prefix}`")
        }
        (None, None) => format!("`{rendered}` rejected: blocked by policy"),
    }
}

/// One token of a prefix pattern.
///
/// Codex parity: `PatternToken` (codex `execpolicy/src/rule.rs:16-35`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatternToken {
    /// Matches one exact token.
    Single(String),
    /// Matches any one of the listed alternatives.
    Alts(Vec<String>),
}

impl PatternToken {
    /// Codex parity: `PatternToken::matches` (codex `rule.rs:22-27`).
    fn matches(&self, token: &str) -> bool {
        match self {
            PatternToken::Single(expected) => expected == token,
            PatternToken::Alts(alts) => alts.iter().any(|alt| alt == token),
        }
    }
}

/// A program-keyed prefix rule: if a command starts with `first` followed by
/// tokens matching `rest`, it yields `decision`.
///
/// Codex parity: `PrefixRule` + `PrefixPattern` (codex `rule.rs:39-115`). The
/// first token is fixed because the policy is keyed by the first token
/// (codex `rule.rs:38`, `policy.rs:297-305`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefixRule {
    /// Fixed first token (the program), the policy key.
    pub first: String,
    /// Remaining pattern tokens that must match positionally.
    pub rest: Vec<PatternToken>,
    /// Decision when this rule matches.
    pub decision: Decision,
    /// Optional rationale, surfaced in forbidden/prompt reasons.
    ///
    /// Codex parity: `PrefixRule::justification` (codex `rule.rs:114`).
    pub justification: Option<String>,
}

impl PrefixRule {
    /// Construct an allow rule for an exact program (no extra args matched).
    pub fn allow_program(program: impl Into<String>) -> Self {
        Self {
            first: program.into(),
            rest: Vec::new(),
            decision: Decision::Allow,
            justification: None,
        }
    }

    /// Construct a forbid rule from a full prefix (program + literal arg tokens).
    ///
    /// Codex parity: a `prefix_rule(pattern=[...], decision="forbidden")`
    /// authored in Starlark; here built Rust-native (see module docs).
    pub fn forbid_prefix(prefix: &[String]) -> Self {
        Self::prefix_with_decision(prefix, Decision::Forbidden)
    }

    /// Construct a prompt rule from a full prefix.
    pub fn prompt_prefix(prefix: &[String]) -> Self {
        Self::prefix_with_decision(prefix, Decision::Prompt)
    }

    /// Construct an allow rule from a full prefix.
    ///
    /// Codex parity: `Policy::add_prefix_rule(prefix, Decision::Allow)` builds
    /// exactly this shape — first token fixed, the rest as `Single` tokens
    /// (codex `policy.rs:91-111`). This is what an amendment installs.
    pub fn allow_prefix(prefix: &[String]) -> Self {
        Self::prefix_with_decision(prefix, Decision::Allow)
    }

    fn prefix_with_decision(prefix: &[String], decision: Decision) -> Self {
        let (first, rest) = prefix.split_first().expect("prefix must be non-empty");
        Self {
            first: first.clone(),
            rest: rest.iter().cloned().map(PatternToken::Single).collect(),
            decision,
            justification: None,
        }
    }

    /// Attach a justification, returning `self` for chaining.
    pub fn with_justification(mut self, justification: impl Into<String>) -> Self {
        self.justification = Some(justification.into());
        self
    }

    /// The full literal prefix this rule represents (for reasons / dedup).
    fn prefix_tokens(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.rest.len() + 1);
        out.push(self.first.clone());
        for token in &self.rest {
            match token {
                PatternToken::Single(value) => out.push(value.clone()),
                PatternToken::Alts(alts) => out.push(format!("[{}]", alts.join("|"))),
            }
        }
        out
    }

    /// If `cmd` matches this rule, return the matched (literal) prefix.
    ///
    /// Codex parity: `PrefixPattern::matches_prefix` (codex `rule.rs:46-59`):
    /// the command must be at least as long as the pattern, the first token must
    /// equal `first`, and every subsequent pattern token must match positionally.
    fn matches(&self, cmd: &[String]) -> Option<Vec<String>> {
        let pattern_len = self.rest.len() + 1;
        if cmd.len() < pattern_len || cmd[0] != self.first {
            return None;
        }
        for (pattern_token, cmd_token) in self.rest.iter().zip(&cmd[1..pattern_len]) {
            if !pattern_token.matches(cmd_token) {
                return None;
            }
        }
        Some(cmd[..pattern_len].to_vec())
    }
}

/// The result of matching a single rule against a command.
struct RuleMatch {
    decision: Decision,
    matched_prefix: Vec<String>,
    justification: Option<String>,
}

/// A Rust-native exec policy: an ordered set of program-keyed prefix rules.
///
/// Codex parity: `Policy` (codex `execpolicy/src/policy.rs:27-32`). Codex keys
/// rules by program in a `MultiMap`; we keep a flat `Vec` and filter by first
/// token in [`Policy::matches`], which is behaviorally equivalent for matching
/// (codex `policy.rs:297-305`). Aggregation is the same `.max()` over matched
/// decisions (codex `policy.rs:365-368`).
#[derive(Clone, Debug, Default)]
pub struct Policy {
    rules: Vec<PrefixRule>,
}

impl Policy {
    /// An empty policy (no rules). Codex parity: `Policy::empty` (codex `policy.rs:51-53`).
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    /// Build a policy from a list of rules.
    pub fn from_rules(rules: Vec<PrefixRule>) -> Self {
        Self { rules }
    }

    /// Append a rule (used by amendments). Codex parity: `Policy::add_prefix_rule`
    /// inserts into the program-keyed map (codex `policy.rs:91-111`).
    pub fn push_rule(&mut self, rule: PrefixRule) {
        self.rules.push(rule);
    }

    /// All allow-prefixes currently present (for amendment dedup / inspection).
    pub fn allowed_prefixes(&self) -> Vec<Vec<String>> {
        self.rules
            .iter()
            .filter(|rule| rule.decision == Decision::Allow)
            .map(PrefixRule::prefix_tokens)
            .collect()
    }

    /// Return the matched rules for `cmd` (already canonicalized by the caller).
    ///
    /// Codex parity: `Policy::match_exact_rules` filters rules whose key equals
    /// `cmd[0]` and collects those that match (codex `policy.rs:297-305`).
    fn matches(&self, cmd: &[String]) -> Vec<RuleMatch> {
        let Some(first) = cmd.first() else {
            return Vec::new();
        };
        self.rules
            .iter()
            .filter(|rule| &rule.first == first)
            .filter_map(|rule| {
                rule.matches(cmd).map(|matched_prefix| RuleMatch {
                    decision: rule.decision,
                    matched_prefix,
                    justification: rule.justification.clone(),
                })
            })
            .collect()
    }

    /// Check a command against the policy, falling back to `heuristics` when no
    /// rule matches.
    ///
    /// `cmd` is canonicalized first (codex canonicalizes a `bash -lc "<plain>"`
    /// wrapper before checking — codex `commands_for_exec_policy`
    /// `exec_policy.rs:772-810` via `parse_shell_lc_plain_commands`).
    ///
    /// Codex parity for aggregation: when one or more rules match, the decision is
    /// the **max** (most restrictive) over their decisions (codex `policy.rs:366`).
    /// When none match, the heuristics fallback decides (codex `policy.rs:285-294`),
    /// just like `is_known_safe_command` / `command_might_be_dangerous` feed the
    /// fallback in codex `render_decision_for_unmatched_command` (`exec_policy.rs:632`).
    pub fn check<F>(&self, cmd: &[String], heuristics: &F) -> ExecPolicyDecision
    where
        F: Fn(&[String]) -> Decision,
    {
        let canonical = canonicalize_command(cmd);
        self.check_canonical(&canonical, heuristics)
    }

    /// Like [`Policy::check`] but assumes `cmd` is already canonical. Exposed for
    /// composition by the amendment layer (which canonicalizes once).
    pub fn check_canonical<F>(&self, cmd: &[String], heuristics: &F) -> ExecPolicyDecision
    where
        F: Fn(&[String]) -> Decision,
    {
        let matched = self.matches(cmd);
        if matched.is_empty() {
            // Codex parity: `HeuristicsRuleMatch` carries the fallback decision
            // when no rule matched (codex `policy.rs:288-291`).
            let decision = heuristics(cmd);
            return ExecPolicyDecision::from_decision(decision, cmd, None, None);
        }

        // Codex parity: most restrictive decision wins (codex `policy.rs:366`).
        let best = matched
            .iter()
            .max_by_key(|rule_match| rule_match.decision)
            .expect("matched is non-empty");
        let decision = best.decision;

        // For a forbidden reason, codex selects the *most specific* (longest
        // matched prefix) forbidden rule (codex `derive_forbidden_reason`
        // `exec_policy.rs:967-979`). Mirror that selection here.
        let (matched_prefix, justification): (Option<&[String]>, Option<&str>) =
            if decision == Decision::Forbidden {
                matched
                    .iter()
                    .filter(|rule_match| rule_match.decision == Decision::Forbidden)
                    .max_by_key(|rule_match| rule_match.matched_prefix.len())
                    .map(|rule_match| {
                        (
                            Some(rule_match.matched_prefix.as_slice()),
                            rule_match.justification.as_deref(),
                        )
                    })
                    .unwrap_or((None, None))
            } else {
                (None, None)
            };

        ExecPolicyDecision::from_decision(decision, cmd, matched_prefix, justification)
    }
}

/// Build the baseline default policy.
///
/// Codex parity: codex ships a `default.rules` Starlark file
/// (codex `core/src/exec_policy.rs:51` `DEFAULT_POLICY_FILE = "default.rules"`).
/// We cannot evaluate Starlark (see module docs), so the default policy here is
/// **empty**: every decision is therefore driven by the heuristics fallback,
/// exactly mirroring codex behavior when no `.rules` file matches a command
/// (codex `policy.rs:285-294`). Hard-coded destructive denials live in the
/// heuristics fallback (`default_heuristics`), mirroring the agent's existing
/// `shell.rs` rm-rf denylist and codex's `command_might_be_dangerous` feeding the
/// fallback (codex `exec_policy.rs:676-702`).
pub fn default_policy() -> Policy {
    Policy::empty()
}

#[cfg(test)]
mod policy_unit_tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn most_restrictive_decision_wins() {
        // Two matching rules for `git`: allow + forbidden. Codex `.max()` picks
        // Forbidden (codex policy.rs:366).
        let policy = Policy::from_rules(vec![
            PrefixRule::allow_program("git"),
            PrefixRule::forbid_prefix(&s(&["git", "push"])),
        ]);
        let decision = policy.check_canonical(&s(&["git", "push"]), &|_| Decision::Allow);
        assert!(matches!(decision, ExecPolicyDecision::Forbidden { .. }));
    }

    #[test]
    fn forbidden_reason_names_longest_prefix() {
        let policy = Policy::from_rules(vec![PrefixRule::forbid_prefix(&s(&["rm", "-rf"]))]);
        let decision = policy.check_canonical(&s(&["rm", "-rf", "/"]), &|_| Decision::Allow);
        match decision {
            ExecPolicyDecision::Forbidden { reason } => {
                assert_eq!(
                    reason,
                    "`rm -rf /` rejected: policy forbids commands starting with `rm -rf`"
                );
            }
            other => panic!("expected forbidden, got {other:?}"),
        }
    }

    #[test]
    fn justification_overrides_prefix_reason() {
        let policy = Policy::from_rules(vec![
            PrefixRule::forbid_prefix(&s(&["rm"])).with_justification("use trash instead")
        ]);
        let decision = policy.check_canonical(&s(&["rm", "x"]), &|_| Decision::Allow);
        match decision {
            ExecPolicyDecision::Forbidden { reason } => {
                assert_eq!(reason, "`rm x` rejected: use trash instead");
            }
            other => panic!("expected forbidden, got {other:?}"),
        }
    }

    #[test]
    fn unmatched_falls_back_to_heuristics() {
        let policy = Policy::empty();
        assert_eq!(
            policy.check_canonical(&s(&["ls"]), &|_| Decision::Allow),
            ExecPolicyDecision::Allowed
        );
        assert_eq!(
            policy.check_canonical(&s(&["weird"]), &|_| Decision::Prompt),
            ExecPolicyDecision::Prompt
        );
    }

    #[test]
    fn alts_token_matches_any_alternative() {
        let rule = PrefixRule {
            first: "git".to_string(),
            rest: vec![PatternToken::Alts(s(&["status", "log"]))],
            decision: Decision::Allow,
            justification: None,
        };
        let policy = Policy::from_rules(vec![rule]);
        assert_eq!(
            policy.check_canonical(&s(&["git", "log"]), &|_| Decision::Prompt),
            ExecPolicyDecision::Allowed
        );
        // `git diff` is not an alternative → no rule → heuristics.
        assert_eq!(
            policy.check_canonical(&s(&["git", "diff"]), &|_| Decision::Prompt),
            ExecPolicyDecision::Prompt
        );
    }
}
