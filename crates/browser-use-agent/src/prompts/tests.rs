//! Tests for the agent-crate `prompts` module: a de-brand guard plus shape /
//! selector / interaction-skills checks.

use super::*;

/// Every model-facing prompt const, paired with a human label for assertions.
fn model_facing_prompts() -> Vec<(&'static str, &'static str)> {
    vec![
        ("BASE_SYSTEM_PROMPT", BASE_SYSTEM_PROMPT),
        ("BROWSER_TOOL_DESCRIPTION", BROWSER_TOOL_DESCRIPTION),
        (
            "BROWSER_SCRIPT_TOOL_DESCRIPTION",
            BROWSER_SCRIPT_TOOL_DESCRIPTION,
        ),
        ("BROWSER_CONNECTION_GUIDANCE", BROWSER_CONNECTION_GUIDANCE),
        ("COLLABORATION_MODE_DEFAULT", COLLABORATION_MODE_DEFAULT),
        ("COLLABORATION_MODE_PLAN", COLLABORATION_MODE_PLAN),
        ("COMPACTED_CONTEXT_SYSTEM", COMPACTED_CONTEXT_SYSTEM),
        ("HELPER_SESSION_IDENTITY", HELPER_SESSION_IDENTITY),
        (
            "HELPER_SESSION_INHERITED_CONTEXT",
            HELPER_SESSION_INHERITED_CONTEXT,
        ),
        ("REVIEW_PROMPT", REVIEW_PROMPT),
    ]
}

/// De-brand guard: no model-facing prompt const may leak `codex` / `chatgpt`
/// brand strings (case-insensitive). The ported content is already browser-use
/// branded; this guards against regressions.
#[test]
fn model_facing_prompts_have_no_codex_or_chatgpt_brand() {
    for (label, content) in model_facing_prompts() {
        let lower = content.to_ascii_lowercase();
        assert!(
            !lower.contains("codex"),
            "model-facing prompt `{label}` leaked the `codex` brand string"
        );
        assert!(
            !lower.contains("chatgpt"),
            "model-facing prompt `{label}` leaked the `chatgpt` brand string"
        );
    }
}

/// Every model-facing prompt const is non-empty (the `include_str!` paths
/// resolve to real, populated assets).
#[test]
fn model_facing_prompts_are_non_empty() {
    for (label, content) in model_facing_prompts() {
        assert!(
            !content.trim().is_empty(),
            "model-facing prompt `{label}` is empty"
        );
    }
}

/// The base system prompt carries the recognizable browser-use preamble marker
/// and `system_prompt()` returns it trimmed.
#[test]
fn system_prompt_has_browser_use_preamble() {
    let prompt = system_prompt();
    assert!(
        prompt.contains("browser-use agent"),
        "base system prompt is missing the browser-use preamble marker"
    );
    // `system_prompt()` is the trimmed asset, matching the legacy provider
    // preamble assembly.
    assert_eq!(prompt, BASE_SYSTEM_PROMPT.trim());
    assert!(!prompt.starts_with(char::is_whitespace));
    assert!(!prompt.ends_with(char::is_whitespace));
}

#[test]
fn browser_agent_system_prompt_loads_main_interaction_skills() {
    let prompt = browser_agent_system_prompt();
    assert!(prompt.starts_with(system_prompt()));
    assert!(prompt.contains("Loaded Browser-Harness Interaction Skills"));
    assert!(prompt.contains("interaction-skills/forms.md"));
    assert!(prompt.contains("interaction-skills/screenshots.md"));
    assert!(prompt.contains("interaction-skills/profile-sync.md"));
    assert_eq!(browser_harness_interaction_skills().len(), 18);
}

#[test]
fn browser_mode_instruction_matches_main_local_connection_guidance() {
    let prompt = browser_mode_instruction("local");
    assert!(prompt.contains("Selected browser mode: Local Chrome"));
    assert!(prompt.contains("Use `browser connect local` before page work"));
    assert!(prompt.contains("browser local setup"));
}

/// The collaboration selector returns the right asset per mode, wrapped in the
/// collaboration-mode tags, with `{{KNOWN_MODE_NAMES}}` substituted, and the
/// two modes differ.
#[test]
fn collaboration_mode_selector_picks_distinct_assets() {
    let default = collaboration_mode_prompt(CollaborationModeKind::Default);
    let plan = collaboration_mode_prompt(CollaborationModeKind::Plan);

    assert_ne!(default, plan, "default and plan modes must differ");

    for rendered in [&default, &plan] {
        assert!(rendered.starts_with(COLLABORATION_MODE_OPEN_TAG));
        assert!(rendered.ends_with(COLLABORATION_MODE_CLOSE_TAG));
        // The placeholder must have been substituted.
        assert!(!rendered.contains("{{KNOWN_MODE_NAMES}}"));
    }

    // Default mode carries its own asset content; Plan carries the plan asset.
    assert!(default.contains("Collaboration Mode: Default"));
    assert!(default.contains(KNOWN_MODE_NAMES));
    assert!(plan.contains("Plan Mode"));
}

/// The browser tool descriptions preserve their interaction-skills content,
/// including the control-plane / data-plane split and the screenshot / image
/// (view-image) workflow notes that drive page interaction.
#[test]
fn browser_tool_descriptions_preserve_interaction_skills() {
    // Control-plane tool description.
    let browser = browser_tool_description();
    assert!(
        browser.contains("Browser runtime control tool"),
        "browser tool description lost its control-plane heading"
    );

    // Data-plane / page-interaction tool description, including the
    // screenshot/image interaction skills that back view-image workflows.
    let script = browser_script_tool_description();
    assert!(
        script.contains("browser interaction tool"),
        "browser_script description lost its interaction-tool framing"
    );
    let script_lower = script.to_ascii_lowercase();
    assert!(
        script_lower.contains("screenshot"),
        "browser_script description lost its screenshot/image interaction skill"
    );

    // The base system prompt enumerates the page-interaction helpers, including
    // the screenshot/image helpers used for visual inspection.
    assert!(
        BASE_SYSTEM_PROMPT.contains("capture_screenshot")
            && BASE_SYSTEM_PROMPT.contains("emit_image"),
        "base system prompt lost its screenshot/image interaction helpers"
    );
}

/// The connection interaction-skills guidance preserves the tab-visibility
/// workflow content.
#[test]
fn connection_guidance_preserves_tab_visibility() {
    assert!(
        BROWSER_CONNECTION_GUIDANCE.contains("ensure_real_tab"),
        "connection guidance lost its tab-visibility workflow"
    );
}

/// `render_prompt_template` trims the template and applies replacements in
/// order, mirroring the legacy helper.
#[test]
fn render_prompt_template_trims_and_substitutes() {
    let rendered = render_prompt_template("  hello {{name}}  ", &[("{{name}}", "browser-use")]);
    assert_eq!(rendered, "hello browser-use");
}

/// `compacted_context_system_message` renders both placeholders from the
/// compacted-context asset.
#[test]
fn compacted_context_message_renders_placeholders() {
    let context = serde_json::json!({ "step": 1, "note": "resume" });
    let rendered = compacted_context_system_message(&context, "the active contract").unwrap();

    assert!(!rendered.contains("{{browser_agent_contract}}"));
    assert!(!rendered.contains("{{context_json}}"));
    assert!(rendered.contains("the active contract"));
    assert!(rendered.contains("\"step\": 1"));
    assert!(rendered.contains("\"note\": \"resume\""));
}
