//! Instruction and prompt template builders extracted from `lib.rs` (Phase 0.1 carve).
//!
//! Code motion only — behavior is byte-identical to the original definitions.

use anyhow::Result;
use serde_json::Value;

use crate::constants::*;
use crate::CollaborationModeKind;

pub(crate) fn browser_mode_instruction(mode: &str) -> String {
    let normalized = mode.to_ascii_lowercase().replace(['_', ' '], "-");
    match normalized.as_str() {
        "local" | "local-chrome" => concat!(
            "Selected browser mode: Local Chrome. Use `browser connect local` before page work. ",
            "This checks for a local Chromium-family browser exposing CDP and attaches only after remote debugging is enabled. ",
            "If connection is blocked, run `browser local setup` and wait for the user to approve remote debugging."
        )
        .to_string(),
        "headless" | "headless-chromium" | "managed-headless" => concat!(
            "Selected browser mode: Headless Chromium. Use `browser connect managed --headless` before page work. ",
            "This starts a Rust-owned managed browser with an isolated automation profile."
        )
        .to_string(),
        "managed" | "managed-headed" => concat!(
            "Selected browser mode: managed headed browser. Use `browser connect managed --headed` before page work. ",
            "This starts a Rust-owned visible browser with an isolated automation profile."
        )
        .to_string(),
        "cloud" | "browser-use-cloud" => concat!(
            "Selected browser mode: Browser Use cloud. Use `browser remote start` before page work. ",
            "Remote start means start and connect; use `browser remote live-url` to retrieve the watch URL."
        )
        .to_string(),
        other => format!(
            "Selected browser mode: {other}. Use `browser status --json` first, then choose an explicit browser connect command."
        ),
    }
}

pub(crate) fn collaboration_mode_instructions(mode: CollaborationModeKind) -> String {
    let template = match mode {
        CollaborationModeKind::Default => {
            include_str!("../../../prompts/collaboration-mode-default.md")
        }
        CollaborationModeKind::Plan => include_str!("../../../prompts/collaboration-mode-plan.md"),
    };
    let text = template.replace("{{KNOWN_MODE_NAMES}}", "Default and Plan");
    format!("{COLLABORATION_MODE_OPEN_TAG}{text}{COLLABORATION_MODE_CLOSE_TAG}")
}

pub(crate) fn compacted_context_system_message(
    context: &Value,
    browser_agent_contract: &str,
) -> Result<String> {
    let context_json = serde_json::to_string_pretty(context)?;
    Ok(render_prompt_template(
        include_str!("../../../prompts/compacted-context-system.md"),
        &[
            ("{{browser_agent_contract}}", browser_agent_contract),
            ("{{context_json}}", &context_json),
        ],
    ))
}

pub(crate) fn render_model_switch_context(model_instructions: &str) -> String {
    format!(
        "<model_switch>\nThe user was previously using a different model. Please continue the conversation according to the following instructions:\n\n{model_instructions}\n</model_switch>"
    )
}

pub(crate) fn render_personality_context(personality_message: &str) -> String {
    format!(
        "<personality_spec> The user has requested a new communication style. Future messages should adhere to the following personality: \n{personality_message} </personality_spec>"
    )
}

pub(crate) fn render_prompt_template(template: &str, replacements: &[(&str, &str)]) -> String {
    let mut rendered = template.trim().to_string();
    for (placeholder, value) in replacements {
        rendered = rendered.replace(placeholder, value);
    }
    rendered
}
