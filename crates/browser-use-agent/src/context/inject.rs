//! Contextual-message injection + reference-context diffing (codex parity). Pure.
//!
//! `Item = serde_json::Value` (the legacy provider-message currency). A
//! *contextual message* is a provider message tagged with a `name` field whose
//! value is one of the `*_CONTEXT_MESSAGE_NAME` constants defined (once) in
//! [`super::constants`]. WP-A3 owns the constant *definitions*; this module owns
//! their *usage* so the accounting/normalization surface and the injection
//! surface share a single source of truth.
//!
//! ## Parity notes
//!
//! The frozen contract names two upstream parity sources: codex
//! `context_manager/updates.rs` (the reference_context / settings-update diff)
//! and the legacy `browser-use-core` context-message builders /
//! `move_workspace_context_before_first_user`. Neither source tree is present in
//! this checkout (the codex repo is absent and the Rust `browser-use-core` in
//! this worktree carries none of the Python-derived context-message
//! machinery — it has no `*_CONTEXT_MESSAGE_NAME` constants, no
//! `move_workspace_context_before_first_user`, and no `reference_context`). The
//! only surviving ground truth is the constant *string values* (already copied
//! into [`super::constants`] by WP-A3). The message *shape* and the diff
//! behavior below therefore follow the documented frozen contract and the
//! provider-message Value convention used throughout
//! [`super::assembly`] (items are `{"type":"message","role":..,"content":..}`,
//! contextual ones additionally tagged with `name`). See the work-package
//! report for the parity caveat.

use serde_json::{json, Value};

use super::constants::{
    COLLABORATION_CONTEXT_MESSAGE_NAME, GENERATED_IMAGE_CONTEXT_MESSAGE_NAME,
    GOAL_CONTEXT_MESSAGE_NAME, HOOK_CONTEXT_MESSAGE_NAME, MENTION_CONTEXT_MESSAGE_NAME,
    MODEL_SWITCH_CONTEXT_MESSAGE_NAME, MULTI_AGENT_USAGE_HINT_CONTEXT_MESSAGE_NAME,
    PERMISSIONS_CONTEXT_MESSAGE_NAME, PERSONALITY_CONTEXT_MESSAGE_NAME,
    WORKSPACE_CONTEXT_MESSAGE_NAME,
};
use super::Item;

/// The kind of contextual message being injected.
///
/// The three `Workspace*` variants all share the single
/// [`WORKSPACE_CONTEXT_MESSAGE_NAME`] tag (they are distinct *sources* of the
/// same workspace-context channel), matching the legacy single
/// `workspace_context` name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextKind {
    WorkspaceEnv,
    WorkspaceAgents,
    WorkspaceUserShell,
    Permissions,
    MultiAgentUsageHint,
    ModelSwitch,
    Personality,
    Goal,
    GeneratedImage,
    Collaboration,
    Mention,
    Hook,
}

/// The `name` tag carried by a context message of this kind.
///
/// Returns the exact string values from [`super::constants`] (which copied them
/// verbatim from the legacy `*_CONTEXT_MESSAGE_NAME` family).
pub fn context_message_name(k: ContextKind) -> &'static str {
    match k {
        ContextKind::WorkspaceEnv
        | ContextKind::WorkspaceAgents
        | ContextKind::WorkspaceUserShell => WORKSPACE_CONTEXT_MESSAGE_NAME,
        ContextKind::Permissions => PERMISSIONS_CONTEXT_MESSAGE_NAME,
        ContextKind::MultiAgentUsageHint => MULTI_AGENT_USAGE_HINT_CONTEXT_MESSAGE_NAME,
        ContextKind::ModelSwitch => MODEL_SWITCH_CONTEXT_MESSAGE_NAME,
        ContextKind::Personality => PERSONALITY_CONTEXT_MESSAGE_NAME,
        ContextKind::Goal => GOAL_CONTEXT_MESSAGE_NAME,
        ContextKind::GeneratedImage => GENERATED_IMAGE_CONTEXT_MESSAGE_NAME,
        ContextKind::Collaboration => COLLABORATION_CONTEXT_MESSAGE_NAME,
        ContextKind::Mention => MENTION_CONTEXT_MESSAGE_NAME,
        ContextKind::Hook => HOOK_CONTEXT_MESSAGE_NAME,
    }
}

/// All known context-message `name` tags. The complete set used by
/// [`is_contextual_message`].
const ALL_CONTEXT_MESSAGE_NAMES: &[&str] = &[
    WORKSPACE_CONTEXT_MESSAGE_NAME,
    PERMISSIONS_CONTEXT_MESSAGE_NAME,
    MULTI_AGENT_USAGE_HINT_CONTEXT_MESSAGE_NAME,
    MODEL_SWITCH_CONTEXT_MESSAGE_NAME,
    PERSONALITY_CONTEXT_MESSAGE_NAME,
    GOAL_CONTEXT_MESSAGE_NAME,
    GENERATED_IMAGE_CONTEXT_MESSAGE_NAME,
    COLLABORATION_CONTEXT_MESSAGE_NAME,
    MENTION_CONTEXT_MESSAGE_NAME,
    HOOK_CONTEXT_MESSAGE_NAME,
];

/// Build the provider message Value for a context message.
///
/// Shape: a user-role provider message whose textual `content` is `text` and
/// which carries the kind's `name` tag — the discriminator
/// [`is_contextual_message`] keys off. This mirrors the
/// `{"type":"message","role":..,"content":..}` shape that
/// [`super::assembly`] estimates/normalizes over.
pub fn build_context_message(k: ContextKind, text: String) -> Item {
    json!({
        "type": "message",
        "role": "user",
        "name": context_message_name(k),
        "content": text,
    })
}

/// True iff `i` carries a known context-message `name` tag.
pub fn is_contextual_message(i: &Item) -> bool {
    item_context_name(i)
        .map(|name| ALL_CONTEXT_MESSAGE_NAMES.contains(&name))
        .unwrap_or(false)
}

/// The `name` tag of an item, if present.
fn item_context_name(i: &Item) -> Option<&str> {
    i.get("name").and_then(Value::as_str)
}

/// True iff `i` is a workspace-context or permissions-context message — the
/// items repositioned by [`move_workspace_context_before_first_user`].
fn is_workspace_or_permissions_context(i: &Item) -> bool {
    matches!(
        item_context_name(i),
        Some(WORKSPACE_CONTEXT_MESSAGE_NAME) | Some(PERMISSIONS_CONTEXT_MESSAGE_NAME)
    )
}

/// True iff `i` is a real user-authored message (a turn boundary): a `user`-role
/// message that is *not* itself an injected contextual message.
fn is_real_user_message(i: &Item) -> bool {
    i.get("role").and_then(Value::as_str) == Some("user") && !is_contextual_message(i)
}

/// Reposition workspace/permissions context to immediately before the first real
/// user message (codex/core behavior).
///
/// All `workspace_context` and `permissions_context` items are extracted (in
/// their original relative order) and re-inserted as a block directly before the
/// first real user message. When there is no real user message yet, the block is
/// re-appended at the end (preserving relative order). When
/// `inject_default_permissions` is `false`, `permissions_context` items are
/// dropped entirely rather than repositioned.
pub fn move_workspace_context_before_first_user(
    items: &mut Vec<Item>,
    inject_default_permissions: bool,
) {
    // Pull out the context block, preserving relative order. Permissions context
    // is dropped when default-permission injection is disabled.
    let mut moved: Vec<Item> = Vec::new();
    let mut remaining: Vec<Item> = Vec::with_capacity(items.len());
    for item in items.drain(..) {
        if is_workspace_or_permissions_context(&item) {
            let is_permissions = item_context_name(&item) == Some(PERMISSIONS_CONTEXT_MESSAGE_NAME);
            if is_permissions && !inject_default_permissions {
                // drop
                continue;
            }
            moved.push(item);
        } else {
            remaining.push(item);
        }
    }

    if moved.is_empty() {
        *items = remaining;
        return;
    }

    // Find the first real user message in the remaining items.
    let insert_at = remaining
        .iter()
        .position(is_real_user_message)
        .unwrap_or(remaining.len());

    let mut rebuilt: Vec<Item> = Vec::with_capacity(remaining.len() + moved.len());
    rebuilt.extend(remaining.drain(..insert_at));
    rebuilt.append(&mut moved);
    rebuilt.extend(remaining.drain(..));
    *items = rebuilt;
}

/// Baseline snapshot of the per-turn settings the model is "told about".
///
/// The inner `Value` is a JSON object whose keys are setting fields (e.g.
/// `model`, `cwd`, `approval_policy`, ...). [`build_settings_update_items`]
/// diffs two snapshots field-by-field. (Codex's `ReferenceContext`; the exact
/// field set is an open question carried over from the frozen sketch — the diff
/// is field-agnostic so any object shape works.)
#[derive(Clone, Debug)]
pub struct TurnContextItem(pub serde_json::Value);

/// The **reference_context diff**: emit a "settings update" context item for each
/// field that changed between two snapshots (codex's reference_context/updates
/// mechanism).
///
/// Rules:
///   * `prev == None` -> empty Vec (the first turn has no baseline to diff
///     against, so nothing is "updated").
///   * a field present in `next` whose value differs from `prev` (or is absent
///     from `prev`) -> one update item.
///   * a field present in `prev` but absent from `next` -> one update item
///     recording its removal (`new` is JSON `null`).
///   * no differences -> empty Vec.
///
/// Each update item is a [`ContextKind::ModelSwitch`]-tagged context message
/// (the `model_switch_context` channel codex uses for settings updates) whose
/// `content` is a stable, human-readable one-liner and which additionally
/// carries a structured `update` object (`{field, old, new}`) for machine
/// consumers. Items are emitted in the sorted-key order of the changed fields so
/// the output is deterministic.
pub fn build_settings_update_items(
    prev: Option<&TurnContextItem>,
    next: &TurnContextItem,
) -> Vec<Item> {
    let Some(prev) = prev else {
        return Vec::new();
    };

    let empty = serde_json::Map::new();
    let prev_obj = prev.0.as_object().unwrap_or(&empty);
    let next_obj = next.0.as_object().unwrap_or(&empty);

    // Union of keys, sorted for deterministic output (serde_json::Map preserves
    // insertion order; we sort explicitly).
    let mut keys: Vec<&String> = prev_obj.keys().chain(next_obj.keys()).collect();
    keys.sort_unstable();
    keys.dedup();

    let mut updates: Vec<Item> = Vec::new();
    for key in keys {
        let old = prev_obj.get(key);
        let new = next_obj.get(key);
        if old == new {
            continue;
        }
        let old_v = old.cloned().unwrap_or(Value::Null);
        let new_v = new.cloned().unwrap_or(Value::Null);
        updates.push(build_settings_update_item(key, &old_v, &new_v));
    }
    updates
}

/// Build a single settings-update context item for one changed field.
fn build_settings_update_item(field: &str, old: &Value, new: &Value) -> Item {
    let content = format!(
        "{field} changed from {} to {}",
        render_scalar(old),
        render_scalar(new)
    );
    json!({
        "type": "message",
        "role": "user",
        "name": MODEL_SWITCH_CONTEXT_MESSAGE_NAME,
        "content": content,
        "update": {
            "field": field,
            "old": old,
            "new": new,
        },
    })
}

/// Render a JSON value for the human-readable update sentence: strings without
/// quotes, everything else via its compact JSON form.
fn render_scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "(unset)".to_string(),
        other => other.to_string(),
    }
}
