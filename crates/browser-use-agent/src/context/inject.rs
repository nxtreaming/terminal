//! Contextual-message injection + reference-context diffing (codex parity). Pure.
//!
//! `Item = serde_json::Value` (the legacy provider-message currency). A
//! *contextual message* is a provider message tagged with a `name` field whose
//! value is one of the `*_CONTEXT_MESSAGE_NAME` constants defined (once) in
//! [`super::constants`]. WP-A3 owns the constant *definitions*; this module owns
//! their *usage* so the accounting/normalization surface and the injection
//! surface share a single source of truth.
//!
//! ## Single source of truth for context-message shapes (A4/A6 debt resolution)
//!
//! The legacy-faithful context-message builders live (privately) in
//! `browser-use-core/src/lib.rs` (`workspace_context_message`,
//! `permissions_context_message`, `model_switch_context_message`, …,
//! `move_workspace_context_before_first_user_message`, lib.rs ~9676-9935) and
//! were duplicated verbatim into [`crate::session::reconstruct`] (WP-A6) as
//! private fns. WP-A4 originally emitted *generic*, field-agnostic Value-diff
//! context messages here that were **not** byte-identical to those builders.
//!
//! WP-B2 reconciles the divergence: [`build_context_message`] /
//! [`context_message_name`] / [`move_workspace_context_before_first_user`] below
//! now reproduce the EXACT same `{role, name, content}` Value shapes the legacy
//! builders produce (cross-checked against
//! `/home/exedev/new-core/terminal-decodex/crates/browser-use-core/src/lib.rs`).
//! `context::inject` is now the single source of truth for these shapes.
//!
//! NOTE: `session::reconstruct` still keeps its own private copies (it is
//! read-only for this WP); a follow-up should make `reconstruct` import these
//! builders from `inject` rather than re-defining them. The shapes are now
//! reconcilable byte-for-byte, so that import is mechanical.
//!
//! ### Legacy shape table (verbatim)
//!
//! | builder | role | content |
//! |---|---|---|
//! | `workspace_context_message(Vec<String>)` | `user` | array, one `{type:"input_text",text}` per section |
//! | `permissions_context_message` | `developer` | `[{type:"input_text",text}]` |
//! | `multi_agent_usage_hint_context_message` | `developer` | `[{type:"input_text",text}]` |
//! | `model_switch_context_message` | `developer` | `[{type:"input_text",text}]` |
//! | `personality_context_message` | `developer` | `[{type:"input_text",text}]` |
//! | `collaboration_context_message` | `developer` | `[{type:"input_text",text}]` |
//! | `generated_image_context_message` | `developer` | `[{type:"input_text",text}]` |
//! | `goal_context_message` | `user` | `[{type:"input_text",text}]` |
//! | `hook_context_message` | `developer` | bare trimmed string + `hook_event_name` field |
//!
//! `typed_mention_context` has no dedicated `text` builder in legacy (mention
//! messages are passed through from the event payload); we materialize it with
//! the developer/content-array convention shared by the other developer-channel
//! contexts so [`is_contextual_message`] still recognizes it.

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

/// The provider role each context kind is emitted with, byte-identical to the
/// legacy builders. Workspace + goal are `user`; everything else is `developer`.
///
/// Ground: legacy `browser-use-core/src/lib.rs` builders (workspace 9687, goal
/// 9798 are `"user"`; permissions/multi-agent/model-switch/personality/
/// collaboration/generated-image/hook are `"developer"`).
fn context_message_role(k: ContextKind) -> &'static str {
    match k {
        ContextKind::WorkspaceEnv
        | ContextKind::WorkspaceAgents
        | ContextKind::WorkspaceUserShell
        | ContextKind::Goal => "user",
        ContextKind::Permissions
        | ContextKind::MultiAgentUsageHint
        | ContextKind::ModelSwitch
        | ContextKind::Personality
        | ContextKind::GeneratedImage
        | ContextKind::Collaboration
        | ContextKind::Mention
        | ContextKind::Hook => "developer",
    }
}

/// Build the provider message Value for a context message, byte-identical to the
/// legacy-faithful builders in `browser-use-core`.
///
/// The common shape is `{role, name, content:[{type:"input_text",text}]}` — the
/// exact output of `workspace_context_message`/`permissions_context_message`/…
/// in legacy `lib.rs`. The two exceptions follow legacy verbatim:
///   * [`ContextKind::Hook`] uses a *bare trimmed string* `content` and adds a
///     `hook_event_name` field (legacy `hook_context_message`, lib.rs:7452).
///     The hook event name is not knowable from a free-text `text` argument, so
///     it is omitted here (a reconstruct caller that has the event will set it);
///     the role/name/`content` (trimmed string) still match legacy exactly.
///   * Workspace kinds wrap the single section into the one-element
///     `input_text` array `workspace_context_message(vec![text])` produces.
pub fn build_context_message(k: ContextKind, text: String) -> Item {
    let role = context_message_role(k);
    let name = context_message_name(k);

    if matches!(k, ContextKind::Hook) {
        // Legacy `hook_context_message`: bare trimmed string content + the
        // `hook_event_name` tag. The event name isn't carried by `text`; leave
        // it absent (callers with the event populate it). Role/name/content
        // (trimmed) are byte-identical to legacy.
        return json!({
            "role": role,
            "name": name,
            "content": text.trim(),
        });
    }

    json!({
        "role": role,
        "name": name,
        "content": [{
            "type": "input_text",
            "text": text,
        }],
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

/// The textual content of a message, joining `input_text` parts.
///
/// Ground: legacy `message_content_text` (lib.rs:10455): string content is
/// returned as-is; an array joins each part's `text` (falling back to the bare
/// part-as-string) with `"\n"`; everything else stringifies.
fn message_content_text(message: &Item) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

/// True iff `i` is a workspace-context message (legacy `is_workspace_context_message`).
fn is_workspace_context_message(i: &Item) -> bool {
    item_context_name(i) == Some(WORKSPACE_CONTEXT_MESSAGE_NAME)
}

/// True iff `i` is a permissions-context message (legacy `is_permissions_context_message`).
fn is_permissions_context_message(i: &Item) -> bool {
    item_context_name(i) == Some(PERMISSIONS_CONTEXT_MESSAGE_NAME)
}

/// True iff a workspace-context content section is the environment block, which
/// legacy floats to the *end* of the workspace sections.
///
/// Ground: legacy `is_environment_context_section` (lib.rs:9937).
fn is_environment_context_section(content: &str) -> bool {
    content.contains("<environment_context>")
}

/// Reposition workspace/permissions context to immediately before the first
/// user message, **collapsing** them into the canonical legacy block shape.
///
/// This is now byte-identical to legacy
/// `move_workspace_context_before_first_user_message` (lib.rs:9889-9935):
///   * all workspace-context sections are gathered (non-empty, trimmed),
///     with any `<environment_context>` section floated to the end;
///   * all permissions-context sections are gathered (non-empty);
///   * if both are empty, the messages pass through unchanged;
///   * otherwise a single rebuilt **permissions** block (sections joined by
///     `"\n\n"`) and a single rebuilt **workspace** block are spliced in
///     immediately before the first `user`-role message (or appended at the end
///     when there is none), in that order.
///
/// The `inject_default_permissions` flag drops the rebuilt permissions block
/// when `false` (parity with the reducer's `initial_context` path, which only
/// emits a default-permissions message when the flag is set).
pub fn move_workspace_context_before_first_user(
    items: &mut Vec<Item>,
    inject_default_permissions: bool,
) {
    let mut context_sections: Vec<String> = Vec::new();
    let mut environment_context_section: Option<String> = None;
    let mut permissions_sections: Vec<String> = Vec::new();
    let mut other_messages: Vec<Item> = Vec::with_capacity(items.len());

    for message in std::mem::take(items) {
        if is_workspace_context_message(&message) {
            let content = message_content_text(&message);
            if !content.trim().is_empty() {
                if is_environment_context_section(&content) {
                    environment_context_section = Some(content);
                } else {
                    context_sections.push(content);
                }
            }
        } else if is_permissions_context_message(&message) {
            let content = message_content_text(&message);
            if !content.trim().is_empty() {
                permissions_sections.push(content);
            }
        } else {
            other_messages.push(message);
        }
    }

    if let Some(environment_context_section) = environment_context_section {
        context_sections.push(environment_context_section);
    }

    // Permissions are dropped entirely when default-permission injection is off.
    if !inject_default_permissions {
        permissions_sections.clear();
    }

    if context_sections.is_empty() && permissions_sections.is_empty() {
        *items = other_messages;
        return;
    }

    let insert_at = other_messages
        .iter()
        .position(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .unwrap_or(other_messages.len());

    let mut insert_messages: Vec<Item> = Vec::new();
    if !permissions_sections.is_empty() {
        insert_messages.push(build_permissions_context_message(
            permissions_sections.join("\n\n"),
        ));
    }
    if !context_sections.is_empty() {
        insert_messages.push(build_workspace_context_message(context_sections));
    }
    other_messages.splice(insert_at..insert_at, insert_messages);
    *items = other_messages;
}

/// Legacy `permissions_context_message` (lib.rs:9752), byte-identical.
fn build_permissions_context_message(text: String) -> Item {
    build_context_message(ContextKind::Permissions, text)
}

/// Legacy `workspace_context_message(Vec<String>)` (lib.rs:9676), byte-identical:
/// a `user`-role message whose `content` is one `input_text` part per section.
fn build_workspace_context_message(sections: Vec<String>) -> Item {
    let content = sections
        .into_iter()
        .map(|text| json!({ "type": "input_text", "text": text }))
        .collect::<Vec<_>>();
    json!({
        "role": "user",
        "name": WORKSPACE_CONTEXT_MESSAGE_NAME,
        "content": content,
    })
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
/// `content` is the legacy `model_switch_context_message` `input_text` array
/// carrying a stable, human-readable one-liner, and which additionally carries a
/// structured `update` object (`{field, old, new}`) for machine consumers. Items
/// are emitted in the sorted-key order of the changed fields so the output is
/// deterministic.
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
///
/// Uses the legacy `model_switch_context_message` shape
/// (`{role:"developer", name:"model_switch_context", content:[{input_text}]}`)
/// for the message envelope and appends a structured `update` object.
fn build_settings_update_item(field: &str, old: &Value, new: &Value) -> Item {
    let content = format!(
        "{field} changed from {} to {}",
        render_scalar(old),
        render_scalar(new)
    );
    let mut item = build_context_message(ContextKind::ModelSwitch, content);
    if let Some(obj) = item.as_object_mut() {
        obj.insert(
            "update".to_string(),
            json!({
                "field": field,
                "old": old,
                "new": new,
            }),
        );
    }
    item
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
