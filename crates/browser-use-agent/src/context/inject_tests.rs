//! Pure parity tests for `context::inject` (WP-A4). No async, no network.
//!
//! Coverage:
//!   * `context_message_name` returns the exact `*_CONTEXT_MESSAGE_NAME`
//!     constant per kind (including the shared `workspace_context` tag for all
//!     three `Workspace*` variants).
//!   * `build_context_message` round-trips through `is_contextual_message`
//!     (true on its own output, false on a plain user message).
//!   * `move_workspace_context_before_first_user` repositions correctly, with
//!     and without a pre-existing user message, and with
//!     `inject_default_permissions` both ways.
//!   * `build_settings_update_items` emits an update only for changed fields
//!     (no-change -> empty Vec; model change -> one update item), asserting the
//!     exact emitted Item shapes.

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
// build_context_message / is_contextual_message round-trip.
// ---------------------------------------------------------------------------

#[test]
fn build_context_message_exact_shape() {
    let msg = build_context_message(ContextKind::Goal, "do the thing".to_string());
    assert_eq!(
        msg,
        json!({
            "type": "message",
            "role": "user",
            "name": "goal_context",
            "content": "do the thing",
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
// move_workspace_context_before_first_user.
// ---------------------------------------------------------------------------

#[test]
fn move_repositions_before_first_user() {
    // [user, workspace, assistant?] -> workspace lands right before the user.
    let mut items = vec![
        user_message("first ask"),
        build_context_message(ContextKind::WorkspaceEnv, "ws".to_string()),
    ];
    move_workspace_context_before_first_user(&mut items, true);
    assert_eq!(
        names_of(&items),
        vec![Some("workspace_context".to_string()), None]
    );
    // The user message is the second element now.
    assert_eq!(items[1], user_message("first ask"));
}

#[test]
fn move_keeps_relative_order_of_moved_block() {
    let mut items = vec![
        user_message("ask"),
        build_context_message(ContextKind::WorkspaceEnv, "env".to_string()),
        build_context_message(ContextKind::Permissions, "perms".to_string()),
        build_context_message(ContextKind::WorkspaceAgents, "agents".to_string()),
    ];
    move_workspace_context_before_first_user(&mut items, true);
    assert_eq!(
        names_of(&items),
        vec![
            Some("workspace_context".to_string()),
            Some("permissions_context".to_string()),
            Some("workspace_context".to_string()),
            None,
        ]
    );
}

#[test]
fn move_with_no_user_message_appends_block_at_end() {
    // No real user message: the block is preserved (re-appended), nothing lost.
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
    // Permissions dropped; workspace repositioned before the user message.
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
fn move_does_not_treat_contextual_user_message_as_turn_boundary() {
    // A goal_context message has role=user but is contextual; the workspace
    // block must skip past it and anchor before the *real* user message.
    let mut items = vec![
        build_context_message(ContextKind::Goal, "goal".to_string()),
        user_message("real ask"),
        build_context_message(ContextKind::WorkspaceEnv, "ws".to_string()),
    ];
    move_workspace_context_before_first_user(&mut items, true);
    assert_eq!(
        names_of(&items),
        vec![
            Some("goal_context".to_string()),
            Some("workspace_context".to_string()),
            None,
        ]
    );
    assert_eq!(items[2], user_message("real ask"));
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
    assert_eq!(
        updates[0],
        json!({
            "type": "message",
            "role": "user",
            "name": "model_switch_context",
            "content": "model changed from gpt-5 to o3",
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
            "type": "message",
            "role": "user",
            "name": "model_switch_context",
            "content": "approval changed from (unset) to on-request",
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
            "type": "message",
            "role": "user",
            "name": "model_switch_context",
            "content": "approval changed from on-request to (unset)",
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
            "type": "message",
            "role": "user",
            "name": "model_switch_context",
            "content": "max_retries changed from 3 to 5",
            "update": { "field": "max_retries", "old": 3, "new": 5 },
        })
    );
}
