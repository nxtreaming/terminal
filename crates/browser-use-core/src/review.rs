//! Code-review session prompts and review-mode helpers extracted from `lib.rs` (Phase 0.1 carve).
//!
//! Code motion only — behavior is byte-identical to the original definitions.

use std::path::Path;

use anyhow::{bail, Result};
use browser_use_protocol::EventRecord;
use browser_use_store::Store;

use crate::constants::*;
use crate::{
    append_session_base_instructions_event, append_workspace_context_event_with_options,
    typed_user_input_payload_from_text_for_cwd, AgentRunOptions,
};

pub fn review_prompt_uncommitted_changes() -> String {
    "Review the current code changes (staged, unstaged, and untracked files) and provide prioritized findings.".to_string()
}

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

pub fn review_prompt_commit(cwd: &Path, sha: &str) -> String {
    let sha = sha.trim();
    let title = git_commit_title(cwd, sha);
    if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
        format!("Review the code changes introduced by commit {sha} (\"{title}\"). Provide prioritized, actionable findings.")
    } else {
        format!("Review the code changes introduced by commit {sha}. Provide prioritized, actionable findings.")
    }
}

pub fn review_prompt_custom(instructions: &str) -> Result<String> {
    let instructions = instructions.trim();
    if instructions.is_empty() {
        bail!("Review prompt cannot be empty");
    }
    Ok(instructions.to_string())
}

pub fn review_base_instructions() -> &'static str {
    include_str!("../../../prompts/review-prompt.md")
}

pub fn start_review_session(store: &Store, prompt: &str, cwd: impl AsRef<Path>) -> Result<String> {
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
    append_session_base_instructions_event(
        store,
        &session.id,
        review_base_instructions(),
        "review",
    )?;
    let options = review_mode_options(AgentRunOptions::default());
    append_workspace_context_event_with_options(store, &session, &options)?;
    store.append_event(
        &session.id,
        "session.input",
        typed_user_input_payload_from_text_for_cwd(prompt, cwd)?,
    )?;
    Ok(session.id)
}

pub(crate) fn review_mode_options(mut options: AgentRunOptions) -> AgentRunOptions {
    if options.base_instructions.is_none() {
        options.base_instructions = Some(review_base_instructions().to_string());
    }
    for (key, value) in [
        ("features.goals", toml::Value::Boolean(false)),
        ("features.multi_agent", toml::Value::Boolean(false)),
        (
            "features.multi_agent_v2.enabled",
            toml::Value::Boolean(false),
        ),
        ("features.web_search_cached", toml::Value::Boolean(false)),
        ("features.web_search_request", toml::Value::Boolean(false)),
        ("features.image_generation", toml::Value::Boolean(false)),
    ] {
        if !options
            .config_overrides
            .iter()
            .any(|(existing, _)| existing == key)
        {
            options.config_overrides.push((key.to_string(), value));
        }
    }
    options
}

pub(crate) fn session_is_review_mode(events: &[EventRecord]) -> bool {
    events
        .iter()
        .any(|event| event.event_type == SESSION_REVIEW_MODE_EVENT)
}

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
