//! Review-mode prompt builders for different git scenarios.
//!
//! Ported faithfully from `browser-use-core`'s `review.rs`
//! (`crates/browser-use-core/src/review.rs`). Only the git-scenario prompt
//! builder functions are ported here; the base review instructions are NOT
//! duplicated — they delegate to the canonical [`crate::prompts::review_prompt`]
//! that already exists in this crate.
//!
//! The session-construction helpers (`start_review_session`,
//! `review_mode_options`, `session_is_review_mode`) from core are intentionally
//! NOT ported here: they depend on `Store` / `AgentRunOptions` event plumbing
//! that belongs to other Phase modules, not this misc-infra leaf.

use std::path::Path;

use browser_use_store::Store;

use crate::context::user_input::typed_user_input_payload_from_text_for_cwd;

/// Durable event type marking a session as review-mode. Exact literal from legacy
/// `browser-use-core` (`SESSION_REVIEW_MODE_EVENT`, constants.rs:101).
const SESSION_REVIEW_MODE_EVENT: &str = "session.review_mode";

/// Durable event type carrying a session's base system instructions. Exact literal
/// from legacy `browser-use-core` (`SESSION_BASE_INSTRUCTIONS_EVENT`,
/// constants.rs).
const SESSION_BASE_INSTRUCTIONS_EVENT: &str = "session.base_instructions";

/// Create a fresh review-mode session in `store`, seeded with `prompt`, and return its
/// id.
///
/// Parity: legacy `browser-use-core::review::start_review_session`
/// (`crates/browser-use-core/src/review.rs:59`). The faithful event sequence is:
///   1. create a root session for `cwd`;
///   2. append a `session.review_mode` event with the review tool restrictions;
///   3. append a `session.base_instructions` event carrying
///      [`review_base_instructions`] (source `"review"`);
///   4. append the `session.input` event with the typed user-input payload built from
///      `prompt` for `cwd` (reusing
///      [`crate::context::user_input::typed_user_input_payload_from_text_for_cwd`]).
///
/// ## Deferred vs legacy
///
/// Legacy additionally derives `AgentRunOptions` via `review_mode_options(...)` and
/// calls `append_workspace_context_event_with_options(store, &session, &options)` to
/// assemble + append the per-section workspace-context events (AGENTS.md, environment,
/// permissions, …). Both `AgentRunOptions` and that section-assembly orchestration live
/// outside the agent crate (the agent crate's `append_workspace_context_event_with_options`
/// is the leaf de-dup variant, not the assembler). So this faithful-minimal port writes
/// the review-mode + base-instructions + typed-input events — the durable markers that
/// make a session review-mode and seed its first turn — and leaves the workspace-context
/// assembly to be wired when `AgentRunOptions` lands. The `review_tool_restrictions`
/// payload is byte-identical to legacy.
pub fn start_review_session(
    store: &Store,
    prompt: &str,
    cwd: impl AsRef<Path>,
) -> anyhow::Result<String> {
    let cwd = cwd.as_ref();
    let session = store.create_session(None, cwd)?;
    store.append_event(
        &session.id,
        SESSION_REVIEW_MODE_EVENT,
        serde_json::json!({
            "kind": "review",
            "review_tool_restrictions": {
                "goals": false,
                "multi_agent": false,
                "web_search": false,
                "image_generation": false
            },
        }),
    )?;
    store.append_event(
        &session.id,
        SESSION_BASE_INSTRUCTIONS_EVENT,
        serde_json::json!({
            "base_instructions": review_base_instructions(),
            "source": "review",
        }),
    )?;
    store.append_event(
        &session.id,
        "session.input",
        typed_user_input_payload_from_text_for_cwd(prompt, cwd)?,
    )?;
    Ok(session.id)
}

/// User-facing prompt to review the working tree (staged + unstaged + untracked).
///
/// Mirrors `browser-use-core::review::review_prompt_uncommitted_changes`
/// (`crates/browser-use-core/src/review.rs:17`).
pub fn review_prompt_uncommitted_changes() -> String {
    "Review the current code changes (staged, unstaged, and untracked files) and provide prioritized findings.".to_string()
}

/// User-facing prompt to review the current branch against a base `branch`.
///
/// Mirrors `browser-use-core::review::review_prompt_base_branch`
/// (`crates/browser-use-core/src/review.rs:21`). When the merge base with HEAD
/// can be resolved it is inlined; otherwise the model is told how to derive it
/// from the branch's upstream. An empty branch falls back to the
/// uncommitted-changes prompt.
pub fn review_prompt_base_branch(cwd: &Path, branch: &str) -> String {
    let branch = branch.trim();
    if branch.is_empty() {
        return review_prompt_uncommitted_changes();
    }
    if let Some(merge_base) = git_merge_base_with_head(cwd, branch) {
        format!(
            "Review the code changes against the base branch '{branch}'. The merge base commit for this comparison is {merge_base}. Run `git diff {merge_base}` to inspect the changes relative to {branch}. Provide prioritized, actionable findings."
        )
    } else {
        format!(
            "Review the code changes against the base branch '{branch}'. Start by finding the merge diff between the current branch and {branch}'s upstream e.g. (`git merge-base HEAD \"$(git rev-parse --abbrev-ref \"{branch}@{{upstream}}\")\"`), then run `git diff` against that SHA to see what changes we would merge into the {branch} branch. Provide prioritized, actionable findings."
        )
    }
}

/// User-facing prompt to review the changes introduced by a single commit.
///
/// Mirrors `browser-use-core::review::review_prompt_commit`
/// (`crates/browser-use-core/src/review.rs:37`). When the commit title can be
/// resolved via `git show`, it is quoted in the prompt.
pub fn review_prompt_commit(cwd: &Path, sha: &str) -> String {
    let sha = sha.trim();
    let title = git_commit_title(cwd, sha);
    if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
        format!("Review the code changes introduced by commit {sha} (\"{title}\"). Provide prioritized, actionable findings.")
    } else {
        format!("Review the code changes introduced by commit {sha}. Provide prioritized, actionable findings.")
    }
}

/// User-facing prompt for a custom review instruction.
///
/// Mirrors `browser-use-core::review::review_prompt_custom`
/// (`crates/browser-use-core/src/review.rs:47`): trims the instruction and
/// errors on empty input.
pub fn review_prompt_custom(instructions: &str) -> Result<String, ReviewPromptError> {
    let instructions = instructions.trim();
    if instructions.is_empty() {
        return Err(ReviewPromptError::Empty);
    }
    Ok(instructions.to_string())
}

/// The base review-mode system instructions.
///
/// Mirrors `browser-use-core::review::review_base_instructions`
/// (`crates/browser-use-core/src/review.rs:55`, which `include_str!`s
/// `prompts/review-prompt.md`). Here it delegates to the canonical
/// [`crate::prompts::review_prompt`] so the asset is not duplicated.
pub fn review_base_instructions() -> &'static str {
    crate::prompts::review_prompt()
}

/// Error returned when a review prompt cannot be built.
///
/// Mirrors the `bail!("Review prompt cannot be empty")` in core
/// (`crates/browser-use-core/src/review.rs:50`), but as a typed error so this
/// leaf module does not pull `anyhow` into its non-test surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewPromptError {
    /// The custom review instruction was empty after trimming.
    Empty,
}

impl std::fmt::Display for ReviewPromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReviewPromptError::Empty => write!(f, "Review prompt cannot be empty"),
        }
    }
}

impl std::error::Error for ReviewPromptError {}

/// Resolve the merge base of `branch` with HEAD in `cwd`.
///
/// Mirrors `browser-use-core::review::git_merge_base_with_head`
/// (`crates/browser-use-core/src/review.rs:123`).
fn git_merge_base_with_head(cwd: &Path, branch: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("merge-base")
        .arg("HEAD")
        .arg(branch)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Resolve the subject line of commit `sha` in `cwd`.
///
/// Mirrors `browser-use-core::review::git_commit_title`
/// (`crates/browser-use-core/src/review.rs:138`).
fn git_commit_title(cwd: &Path, sha: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("show")
        .arg("-s")
        .arg("--format=%s")
        .arg(sha)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn uncommitted_changes_text_is_exact() {
        assert_eq!(
            review_prompt_uncommitted_changes(),
            "Review the current code changes (staged, unstaged, and untracked files) and provide prioritized findings."
        );
    }

    #[test]
    fn base_branch_empty_falls_back_to_uncommitted() {
        let cwd = Path::new(".");
        assert_eq!(
            review_prompt_base_branch(cwd, "   "),
            review_prompt_uncommitted_changes()
        );
    }

    #[test]
    fn base_branch_in_non_git_dir_uses_upstream_guidance() {
        // A temp dir is not a git repo, so merge-base fails and we get the
        // upstream-derivation guidance branch.
        let dir = std::env::temp_dir().join(format!("review-base-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let prompt = review_prompt_base_branch(&dir, "main");
        assert!(prompt.contains("base branch 'main'"));
        assert!(prompt.contains("git merge-base HEAD"));
        assert!(prompt.contains("@{upstream}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_in_non_git_dir_omits_title() {
        let dir = std::env::temp_dir().join(format!("review-commit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let prompt = review_prompt_commit(&dir, "abc123");
        assert_eq!(
            prompt,
            "Review the code changes introduced by commit abc123. Provide prioritized, actionable findings."
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn custom_trims_and_rejects_empty() {
        assert_eq!(review_prompt_custom("  look here  ").unwrap(), "look here");
        assert_eq!(review_prompt_custom("   "), Err(ReviewPromptError::Empty));
        assert_eq!(
            ReviewPromptError::Empty.to_string(),
            "Review prompt cannot be empty"
        );
    }

    #[test]
    fn base_instructions_delegate_to_canonical_prompt() {
        assert_eq!(review_base_instructions(), crate::prompts::review_prompt());
    }

    #[test]
    fn start_review_session_writes_review_events_and_input() {
        use browser_use_store::Store;

        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("open store");

        let session_id = start_review_session(&store, "Review my changes", dir.path())
            .expect("start review session");

        let events = store
            .events_for_session(&session_id)
            .expect("events for session");
        let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        // `Store::create_session` itself records a leading `session.created` event, so
        // the three markers `start_review_session` writes are the tail of the log, in
        // this order.
        assert_eq!(
            &types[types.len() - 3..],
            &[
                SESSION_REVIEW_MODE_EVENT,
                SESSION_BASE_INSTRUCTIONS_EVENT,
                "session.input",
            ]
        );

        // The three review markers we appended, in order (skipping the store prelude).
        let review_idx = types.len() - 3;
        let events = &events[review_idx..];

        // Review-mode event carries the byte-identical tool restrictions.
        let review = &events[0];
        assert_eq!(review.payload["kind"], "review");
        assert_eq!(review.payload["review_tool_restrictions"]["goals"], false);
        assert_eq!(
            review.payload["review_tool_restrictions"]["multi_agent"],
            false
        );
        assert_eq!(
            review.payload["review_tool_restrictions"]["web_search"],
            false
        );
        assert_eq!(
            review.payload["review_tool_restrictions"]["image_generation"],
            false
        );

        // Base-instructions event delegates to the canonical review prompt.
        let base = &events[1];
        assert_eq!(base.payload["source"], "review");
        assert_eq!(
            base.payload["base_instructions"],
            crate::prompts::review_prompt()
        );

        // The seeded user input carries the typed payload built from the prompt.
        let input = &events[2];
        assert_eq!(input.payload["text"], "Review my changes");
        assert_eq!(input.payload["content"], "Review my changes");
    }
}
