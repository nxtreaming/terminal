//! Command canonicalization performed before policy checking.
//!
//! Codex parity: before checking a command against the policy, codex lowers shell
//! wrappers into the underlying command words via `commands_for_exec_policy`
//! (codex `core/src/exec_policy.rs:772-810`), which calls
//! `parse_shell_lc_plain_commands` (codex `shell-command/src/bash.rs:114-121`) to
//! unwrap a `bash -lc "<plain script>"` invocation into the individual "plain"
//! commands (bare words / quoted strings joined by the safe operators `&&`,
//! `||`, `;`, `|`). `is_known_safe_command` performs the same unwrap before
//! consulting its safelist (codex `is_safe_command.rs:37-44`). The legacy engine
//! likewise inspects the `<shell> -c "<script>"` / `-lc` payload
//! (legacy `browser-use-core/src/tools/command.rs:167-176`).
//!
//! # PARITY DEBT (LOUD FLAG)
//!
//! Codex's unwrap is backed by a **tree-sitter-bash** parser
//! (legacy `command.rs:18-19` imports `tree_sitter_bash`; codex `bash.rs` parses
//! the script AST). We do **not** pull in `tree-sitter`; this is a
//! **dependency-light, conservative** tokenizer that unwraps only the common
//! single-plain-command case `bash -lc "<words>"` (and `sh`/`zsh`/`dash`, `-c`).
//! It deliberately **declines to unwrap** anything containing shell
//! metacharacters (operators, redirections, substitutions, quotes, globs,
//! assignments, comments), returning the original argv unchanged so the policy /
//! heuristics evaluate the wrapper verbatim — the safe direction (no command is
//! silently "unwrapped" into something more permissive). Full pipeline/operator
//! AST unwrapping (codex returns *multiple* commands) is deferred; here a script
//! with operators is left wrapped rather than mis-parsed.

/// Canonicalize a command for policy evaluation.
///
/// Returns the underlying command words when `cmd` is a recognized
/// `<shell> -c|-lc "<single plain command>"` wrapper; otherwise returns `cmd`
/// unchanged.
///
/// Codex parity: the single-plain-command path of `commands_for_exec_policy`
/// (codex `exec_policy.rs:772-781`) backed by `parse_shell_lc_plain_commands`
/// (codex `bash.rs:114-121`). Codex also strips a leading `zsh`→`bash` rename in
/// `is_known_safe_command` (codex `is_safe_command.rs:14-21`); the canonical form
/// here keeps the shell name only when it cannot safely unwrap.
pub fn canonicalize_command(cmd: &[String]) -> Vec<String> {
    if let Some(inner) = unwrap_shell_lc_single_plain_command(cmd) {
        return inner;
    }
    cmd.to_vec()
}

/// The trailing path component of a program name (so `/bin/bash` -> `bash`).
///
/// Codex/legacy parity: legacy `base_name` (legacy `command.rs:190-192`); codex
/// keys on the executable basename (codex `executable_name.rs:7-14`).
fn base_name(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// Recognize `<shell> -c|-lc "<script>"` and, if `<script>` is a single "plain"
/// command (no shell metacharacters), return its tokens.
///
/// Mirrors codex `parse_shell_lc_single_command_prefix` for the single-command
/// case (codex `bash.rs:123-130`) but with a conservative, dependency-light
/// tokenizer (see module docs).
fn unwrap_shell_lc_single_plain_command(cmd: &[String]) -> Option<Vec<String>> {
    // Exactly: [shell, flag, script]. The four-arg form `bash -lc git status`
    // is NOT a valid single wrapper (codex/legacy treat it as unsafe / not
    // unwrappable — see codex `bash_lc_unsafe_examples`, is_safe_command.rs:702).
    if cmd.len() != 3 {
        return None;
    }
    // Shell wrappers codex/legacy recognize (legacy `command.rs:170-173`).
    if !matches!(base_name(&cmd[0]), "sh" | "bash" | "zsh" | "dash") {
        return None;
    }
    // Login/non-login command flags (legacy `command.rs:173`; codex `bash.rs`).
    if !matches!(cmd[1].as_str(), "-c" | "-lc") {
        return None;
    }

    let script = cmd[2].as_str();
    let tokens = tokenize_plain_script(script)?;
    if tokens.is_empty() {
        return None;
    }
    Some(tokens)
}

/// Tokenize a script string into bare words, declining (returning `None`) if any
/// shell metacharacter that could change meaning is present.
///
/// This is deliberately stricter than a full shell tokenizer: anything that is
/// not a plain run of words separated by spaces is rejected, so we never unwrap a
/// command into a more permissive form than codex's AST parser would. The set of
/// rejected characters covers operators (`& | ; ( ) < > `), substitutions
/// (`$ \``), quotes (`' "`), escapes (`\\`), globs (`* ? [ ]`), assignments
/// (`=`), braces (`{ }`), and comments (`#`) — mirroring the negative cases codex
/// lists in `bash_lc_unsafe_examples` (codex `is_safe_command.rs:702-739`:
/// redirection, subshell, `$(...)`, `<(...)`, `$HOME`, comments, `FOO=bar`).
fn tokenize_plain_script(script: &str) -> Option<Vec<String>> {
    const FORBIDDEN: &[char] = &[
        '&', '|', ';', '(', ')', '<', '>', '$', '`', '\'', '"', '\\', '*', '?', '[', ']', '=', '{',
        '}', '#', '\n', '\t',
    ];
    if script.chars().any(|c| FORBIDDEN.contains(&c)) {
        return None;
    }
    let tokens: Vec<String> = script
        .split_whitespace()
        .map(|word| word.to_string())
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens)
    }
}

#[cfg(test)]
mod canonicalize_unit_tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn plain_command_passthrough() {
        assert_eq!(canonicalize_command(&s(&["ls", "-1"])), s(&["ls", "-1"]));
    }

    #[test]
    fn unwraps_bash_lc_single_plain_command() {
        // Codex parity: `bash -lc "ls -1"` lowers to `["ls", "-1"]`
        // (codex bash.rs parse_shell_lc_plain_commands; is_safe_command.rs:649).
        assert_eq!(
            canonicalize_command(&s(&["bash", "-lc", "ls -1"])),
            s(&["ls", "-1"])
        );
        assert_eq!(
            canonicalize_command(&s(&["sh", "-c", "git status"])),
            s(&["git", "status"])
        );
        assert_eq!(
            canonicalize_command(&s(&["/bin/bash", "-lc", "pwd"])),
            s(&["pwd"])
        );
    }

    #[test]
    fn does_not_unwrap_scripts_with_operators() {
        // Codex's AST parser returns multiple commands here; our conservative
        // tokenizer declines and leaves the wrapper verbatim (the safe direction).
        let cmd = s(&["bash", "-lc", "ls && rm -rf /"]);
        assert_eq!(canonicalize_command(&cmd), cmd);
        let redir = s(&["bash", "-lc", "ls > out.txt"]);
        assert_eq!(canonicalize_command(&redir), redir);
        let subst = s(&["bash", "-lc", "echo $(pwd)"]);
        assert_eq!(canonicalize_command(&subst), subst);
    }

    #[test]
    fn four_arg_wrapper_is_not_unwrapped() {
        // Codex: `["bash","-lc","git","status"]` is NOT a single safe wrapper
        // (codex is_safe_command.rs:704-707).
        let cmd = s(&["bash", "-lc", "git", "status"]);
        assert_eq!(canonicalize_command(&cmd), cmd);
    }

    #[test]
    fn quoted_inner_program_is_not_unwrapped() {
        // Codex: extra quoting makes a program literally named "git status"
        // (codex is_safe_command.rs:708-711); our tokenizer rejects the quote.
        let cmd = s(&["bash", "-lc", "'git status'"]);
        assert_eq!(canonicalize_command(&cmd), cmd);
    }
}
