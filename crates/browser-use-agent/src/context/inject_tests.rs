//! Pure parity tests for `context::inject` (WP-A4, reconciled in WP-B2). No
//! async, no network.
//!
//! Coverage:
//!   * `context_message_name` returns the exact `*_CONTEXT_MESSAGE_NAME`
//!     constant per kind (including the shared `workspace_context` tag for all
//!     three `Workspace*` variants).
//!   * `build_context_message` produces the **legacy-faithful** Value shapes
//!     (developer/user role + `input_text` content array; hook bare-string),
//!     byte-identical to the `browser-use-core` builders, and round-trips
//!     through `is_contextual_message`.
//!   * `move_workspace_context_before_first_user` collapses + repositions the
//!     workspace/permissions blocks exactly like legacy
//!     `move_workspace_context_before_first_user_message`.
//!   * `build_settings_update_items` emits an update only for changed fields,
//!     with the legacy model-switch envelope + a structured `update` object.

use serde_json::json;

use super::constants::{
    COLLABORATION_CONTEXT_MESSAGE_NAME, GENERATED_IMAGE_CONTEXT_MESSAGE_NAME,
    GOAL_CONTEXT_MESSAGE_NAME, HOOK_CONTEXT_MESSAGE_NAME, MENTION_CONTEXT_MESSAGE_NAME,
    MODEL_SWITCH_CONTEXT_MESSAGE_NAME, MULTI_AGENT_USAGE_HINT_CONTEXT_MESSAGE_NAME,
    PERMISSIONS_CONTEXT_MESSAGE_NAME, PERSONALITY_CONTEXT_MESSAGE_NAME,
    WORKSPACE_CONTEXT_MESSAGE_NAME,
};
use super::inject::{
    build_context_message, build_settings_update_items, context_message_name,
    is_contextual_message, move_workspace_context_before_first_user, ContextKind, TurnContextItem,
};
use super::Item;

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn user_message(text: &str) -> Item {
    json!({ "type": "message", "role": "user", "content": text })
}

fn names_of(items: &[Item]) -> Vec<Option<String>> {
    items
        .iter()
        .map(|i| {
            i.get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// context_message_name.
// ---------------------------------------------------------------------------

#[test]
fn context_message_name_matches_constants_per_kind() {
    // All three workspace sources share the single workspace_context tag.
    assert_eq!(
        context_message_name(ContextKind::WorkspaceEnv),
        WORKSPACE_CONTEXT_MESSAGE_NAME
    );
    assert_eq!(
        context_message_name(ContextKind::WorkspaceAgents),
        WORKSPACE_CONTEXT_MESSAGE_NAME
    );
    assert_eq!(
        context_message_name(ContextKind::WorkspaceUserShell),
        WORKSPACE_CONTEXT_MESSAGE_NAME
    );
    assert_eq!(
        context_message_name(ContextKind::Permissions),
        PERMISSIONS_CONTEXT_MESSAGE_NAME
    );
    assert_eq!(
        context_message_name(ContextKind::MultiAgentUsageHint),
        MULTI_AGENT_USAGE_HINT_CONTEXT_MESSAGE_NAME
    );
    assert_eq!(
        context_message_name(ContextKind::ModelSwitch),
        MODEL_SWITCH_CONTEXT_MESSAGE_NAME
    );
    assert_eq!(
        context_message_name(ContextKind::Personality),
        PERSONALITY_CONTEXT_MESSAGE_NAME
    );
    assert_eq!(
        context_message_name(ContextKind::Goal),
        GOAL_CONTEXT_MESSAGE_NAME
    );
    assert_eq!(
        context_message_name(ContextKind::GeneratedImage),
        GENERATED_IMAGE_CONTEXT_MESSAGE_NAME
    );
    assert_eq!(
        context_message_name(ContextKind::Collaboration),
        COLLABORATION_CONTEXT_MESSAGE_NAME
    );
    assert_eq!(
        context_message_name(ContextKind::Mention),
        MENTION_CONTEXT_MESSAGE_NAME
    );
    assert_eq!(
        context_message_name(ContextKind::Hook),
        HOOK_CONTEXT_MESSAGE_NAME
    );
}

#[test]
fn context_message_name_exact_string_values() {
    // Cross-check the literal strings (the WP-A3-copied legacy values).
    assert_eq!(
        context_message_name(ContextKind::WorkspaceEnv),
        "workspace_context"
    );
    assert_eq!(
        context_message_name(ContextKind::Permissions),
        "permissions_context"
    );
    assert_eq!(
        context_message_name(ContextKind::MultiAgentUsageHint),
        "multi_agent_usage_hint"
    );
    assert_eq!(
        context_message_name(ContextKind::ModelSwitch),
        "model_switch_context"
    );
    assert_eq!(
        context_message_name(ContextKind::Personality),
        "personality_context"
    );
    assert_eq!(context_message_name(ContextKind::Goal), "goal_context");
    assert_eq!(
        context_message_name(ContextKind::GeneratedImage),
        "generated_image_context"
    );
    assert_eq!(
        context_message_name(ContextKind::Collaboration),
        "collaboration_context"
    );
    assert_eq!(
        context_message_name(ContextKind::Mention),
        "typed_mention_context"
    );
    assert_eq!(context_message_name(ContextKind::Hook), "hook_context");
}

// ---------------------------------------------------------------------------
// build_context_message — legacy-faithful shapes.
// ---------------------------------------------------------------------------

#[test]
fn build_context_message_goal_legacy_shape() {
    // Legacy `goal_context_message`: role=user, name=goal_context, content
    // is a one-element input_text array.
    let msg = build_context_message(ContextKind::Goal, "do the thing".to_string());
    assert_eq!(
        msg,
        json!({
            "role": "user",
            "name": "goal_context",
            "content": [{ "type": "input_text", "text": "do the thing" }],
        })
    );
}

#[test]
fn build_context_message_workspace_legacy_shape() {
    // Legacy `workspace_context_message(vec![text])`: role=user, one input_text.
    let msg = build_context_message(ContextKind::WorkspaceEnv, "ws".to_string());
    assert_eq!(
        msg,
        json!({
            "role": "user",
            "name": "workspace_context",
            "content": [{ "type": "input_text", "text": "ws" }],
        })
    );
}

#[test]
fn build_context_message_developer_kinds_legacy_shape() {
    // permissions / multi-agent / model-switch / personality / collaboration /
    // generated-image / mention are all developer-role input_text arrays.
    for (kind, name) in [
        (ContextKind::Permissions, "permissions_context"),
        (ContextKind::MultiAgentUsageHint, "multi_agent_usage_hint"),
        (ContextKind::ModelSwitch, "model_switch_context"),
        (ContextKind::Personality, "personality_context"),
        (ContextKind::Collaboration, "collaboration_context"),
        (ContextKind::GeneratedImage, "generated_image_context"),
        (ContextKind::Mention, "typed_mention_context"),
    ] {
        let msg = build_context_message(kind, "x".to_string());
        assert_eq!(
            msg,
            json!({
                "role": "developer",
                "name": name,
                "content": [{ "type": "input_text", "text": "x" }],
            }),
            "shape mismatch for {kind:?}"
        );
    }
}

#[test]
fn build_context_message_hook_legacy_shape() {
    // Legacy `hook_context_message`: developer role, bare *trimmed* string
    // content (the hook_event_name field is omitted when not knowable here).
    let msg = build_context_message(ContextKind::Hook, "  hello hook  ".to_string());
    assert_eq!(
        msg,
        json!({
            "role": "developer",
            "name": "hook_context",
            "content": "hello hook",
        })
    );
}

#[test]
fn build_context_message_round_trips_is_contextual() {
    for kind in [
        ContextKind::WorkspaceEnv,
        ContextKind::WorkspaceAgents,
        ContextKind::WorkspaceUserShell,
        ContextKind::Permissions,
        ContextKind::MultiAgentUsageHint,
        ContextKind::ModelSwitch,
        ContextKind::Personality,
        ContextKind::Goal,
        ContextKind::GeneratedImage,
        ContextKind::Collaboration,
        ContextKind::Mention,
        ContextKind::Hook,
    ] {
        let msg = build_context_message(kind, "x".to_string());
        assert!(
            is_contextual_message(&msg),
            "expected contextual for {kind:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// GOLDEN: inject builder == legacy permissions_context_message shape.
// ---------------------------------------------------------------------------

#[test]
fn golden_permissions_matches_legacy_permissions_context_message() {
    // The exact Value `browser-use-core::permissions_context_message("policy")`
    // produces (lib.rs:9752): role=developer, name=permissions_context, content
    // is a one-element input_text array. This is the A4/A6 reconciliation golden.
    let legacy = json!({
        "role": "developer",
        "name": "permissions_context",
        "content": [{ "type": "input_text", "text": "policy" }],
    });
    assert_eq!(
        build_context_message(ContextKind::Permissions, "policy".to_string()),
        legacy
    );
}

#[test]
fn golden_workspace_matches_legacy_workspace_context_message() {
    // Legacy `workspace_context_message(vec!["env section"])` (lib.rs:9676).
    let legacy = json!({
        "role": "user",
        "name": "workspace_context",
        "content": [{ "type": "input_text", "text": "env section" }],
    });
    assert_eq!(
        build_context_message(ContextKind::WorkspaceEnv, "env section".to_string()),
        legacy
    );
}

#[test]
fn is_contextual_message_false_on_plain_user_message() {
    assert!(!is_contextual_message(&user_message("hello")));
}

#[test]
fn is_contextual_message_false_on_unknown_name() {
    let weird =
        json!({ "type": "message", "role": "user", "name": "not_a_context", "content": "x" });
    assert!(!is_contextual_message(&weird));
}

#[test]
fn is_contextual_message_false_on_non_object() {
    assert!(!is_contextual_message(&json!("a bare string")));
    assert!(!is_contextual_message(&json!(42)));
}

// ---------------------------------------------------------------------------
// move_workspace_context_before_first_user — legacy collapse + reposition.
// ---------------------------------------------------------------------------

#[test]
fn move_collapses_and_repositions_before_first_user() {
    // [user, workspace] -> a single workspace block lands right before the user.
    let mut items = vec![
        user_message("first ask"),
        build_context_message(ContextKind::WorkspaceEnv, "ws".to_string()),
    ];
    move_workspace_context_before_first_user(&mut items, true);
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0],
        json!({
            "role": "user",
            "name": "workspace_context",
            "content": [{ "type": "input_text", "text": "ws" }],
        })
    );
    assert_eq!(items[1], user_message("first ask"));
}

#[test]
fn move_collapses_permissions_then_workspace_in_order() {
    // Legacy emits the rebuilt permissions block, then the workspace block,
    // both before the first user message. Multiple workspace sections collapse
    // into one block (one input_text part each).
    let mut items = vec![
        user_message("ask"),
        build_context_message(ContextKind::WorkspaceEnv, "env".to_string()),
        build_context_message(ContextKind::Permissions, "perms".to_string()),
        build_context_message(ContextKind::WorkspaceAgents, "agents".to_string()),
    ];
    move_workspace_context_before_first_user(&mut items, true);
    // permissions block, workspace block (env, agents), then the user message.
    assert_eq!(
        names_of(&items),
        vec![
            Some("permissions_context".to_string()),
            Some("workspace_context".to_string()),
            None,
        ]
    );
    assert_eq!(
        items[0],
        json!({
            "role": "developer",
            "name": "permissions_context",
            "content": [{ "type": "input_text", "text": "perms" }],
        })
    );
    assert_eq!(
        items[1],
        json!({
            "role": "user",
            "name": "workspace_context",
            "content": [
                { "type": "input_text", "text": "env" },
                { "type": "input_text", "text": "agents" },
            ],
        })
    );
    assert_eq!(items[2], user_message("ask"));
}

#[test]
fn move_floats_environment_section_to_end_of_workspace_block() {
    // The <environment_context> section is floated to the end of the collapsed
    // workspace block (legacy is_environment_context_section behavior).
    let mut items = vec![
        user_message("ask"),
        build_context_message(
            ContextKind::WorkspaceEnv,
            "<environment_context>\ncwd=/repo\n</environment_context>".to_string(),
        ),
        build_context_message(ContextKind::WorkspaceAgents, "agents".to_string()),
    ];
    move_workspace_context_before_first_user(&mut items, true);
    assert_eq!(
        items[0],
        json!({
            "role": "user",
            "name": "workspace_context",
            "content": [
                { "type": "input_text", "text": "agents" },
                { "type": "input_text", "text": "<environment_context>\ncwd=/repo\n</environment_context>" },
            ],
        })
    );
}

#[test]
fn move_with_no_user_message_appends_block_at_end() {
    // No user message: the collapsed block is appended at the end.
    let mut items = vec![
        json!({ "type": "message", "role": "assistant", "content": "hi" }),
        build_context_message(ContextKind::WorkspaceEnv, "ws".to_string()),
    ];
    move_workspace_context_before_first_user(&mut items, true);
    assert_eq!(
        names_of(&items),
        vec![None, Some("workspace_context".to_string())]
    );
    assert_eq!(items.len(), 2);
}

#[test]
fn move_inject_default_permissions_true_keeps_permissions() {
    let mut items = vec![
        user_message("ask"),
        build_context_message(ContextKind::Permissions, "perms".to_string()),
    ];
    move_workspace_context_before_first_user(&mut items, true);
    assert_eq!(
        names_of(&items),
        vec![Some("permissions_context".to_string()), None]
    );
}

#[test]
fn move_inject_default_permissions_false_drops_permissions() {
    let mut items = vec![
        user_message("ask"),
        build_context_message(ContextKind::Permissions, "perms".to_string()),
        build_context_message(ContextKind::WorkspaceEnv, "ws".to_string()),
    ];
    move_workspace_context_before_first_user(&mut items, false);
    // Permissions dropped; workspace block repositioned before the user message.
    assert_eq!(
        names_of(&items),
        vec![Some("workspace_context".to_string()), None]
    );
    assert_eq!(items[1], user_message("ask"));
}

#[test]
fn move_is_noop_when_no_workspace_context_present() {
    let mut items = vec![
        user_message("ask"),
        json!({ "type": "message", "role": "assistant", "content": "reply" }),
    ];
    let before = items.clone();
    move_workspace_context_before_first_user(&mut items, true);
    assert_eq!(items, before);
}

#[test]
fn move_drops_empty_workspace_sections() {
    // Whitespace-only workspace content is dropped (legacy non-empty filter);
    // with nothing left the messages pass through unchanged.
    let mut items = vec![
        user_message("ask"),
        build_context_message(ContextKind::WorkspaceEnv, "   ".to_string()),
    ];
    move_workspace_context_before_first_user(&mut items, true);
    assert_eq!(names_of(&items), vec![None]);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0], user_message("ask"));
}

// ---------------------------------------------------------------------------
// build_settings_update_items (reference_context diff).
// ---------------------------------------------------------------------------

#[test]
fn settings_update_none_prev_is_empty() {
    let next = TurnContextItem(json!({ "model": "gpt-5" }));
    assert!(build_settings_update_items(None, &next).is_empty());
}

#[test]
fn settings_update_no_change_is_empty() {
    let prev = TurnContextItem(json!({ "model": "gpt-5", "cwd": "/repo" }));
    let next = TurnContextItem(json!({ "model": "gpt-5", "cwd": "/repo" }));
    assert!(build_settings_update_items(Some(&prev), &next).is_empty());
}

#[test]
fn settings_update_model_change_emits_one_item() {
    let prev = TurnContextItem(json!({ "model": "gpt-5", "cwd": "/repo" }));
    let next = TurnContextItem(json!({ "model": "o3", "cwd": "/repo" }));
    let updates = build_settings_update_items(Some(&prev), &next);
    assert_eq!(updates.len(), 1);
    // Legacy model_switch_context_message envelope + structured update object.
    assert_eq!(
        updates[0],
        json!({
            "role": "developer",
            "name": "model_switch_context",
            "content": [{ "type": "input_text", "text": "model changed from gpt-5 to o3" }],
            "update": { "field": "model", "old": "gpt-5", "new": "o3" },
        })
    );
}

#[test]
fn settings_update_added_field_records_unset_old() {
    let prev = TurnContextItem(json!({ "model": "gpt-5" }));
    let next = TurnContextItem(json!({ "model": "gpt-5", "approval": "on-request" }));
    let updates = build_settings_update_items(Some(&prev), &next);
    assert_eq!(updates.len(), 1);
    assert_eq!(
        updates[0],
        json!({
            "role": "developer",
            "name": "model_switch_context",
            "content": [{ "type": "input_text", "text": "approval changed from (unset) to on-request" }],
            "update": { "field": "approval", "old": null, "new": "on-request" },
        })
    );
}

#[test]
fn settings_update_removed_field_records_unset_new() {
    let prev = TurnContextItem(json!({ "model": "gpt-5", "approval": "on-request" }));
    let next = TurnContextItem(json!({ "model": "gpt-5" }));
    let updates = build_settings_update_items(Some(&prev), &next);
    assert_eq!(updates.len(), 1);
    assert_eq!(
        updates[0],
        json!({
            "role": "developer",
            "name": "model_switch_context",
            "content": [{ "type": "input_text", "text": "approval changed from on-request to (unset)" }],
            "update": { "field": "approval", "old": "on-request", "new": null },
        })
    );
}

#[test]
fn settings_update_multiple_changes_sorted_deterministic() {
    let prev = TurnContextItem(json!({ "model": "gpt-5", "cwd": "/a", "sandbox": "ro" }));
    let next = TurnContextItem(json!({ "model": "o3", "cwd": "/b", "sandbox": "ro" }));
    let updates = build_settings_update_items(Some(&prev), &next);
    // Two changed fields (cwd, model); sandbox unchanged. Sorted key order.
    assert_eq!(updates.len(), 2);
    let fields: Vec<&str> = updates
        .iter()
        .map(|u| u["update"]["field"].as_str().unwrap())
        .collect();
    assert_eq!(fields, vec!["cwd", "model"]);
    assert!(updates.iter().all(is_contextual_message));
}

#[test]
fn settings_update_non_string_value_change() {
    let prev = TurnContextItem(json!({ "max_retries": 3 }));
    let next = TurnContextItem(json!({ "max_retries": 5 }));
    let updates = build_settings_update_items(Some(&prev), &next);
    assert_eq!(updates.len(), 1);
    assert_eq!(
        updates[0],
        json!({
            "role": "developer",
            "name": "model_switch_context",
            "content": [{ "type": "input_text", "text": "max_retries changed from 3 to 5" }],
            "update": { "field": "max_retries", "old": 3, "new": 5 },
        })
    );
}
