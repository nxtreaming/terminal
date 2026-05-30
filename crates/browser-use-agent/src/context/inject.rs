//! Contextual-message injection + reference-context diffing (codex parity). Pure.

use super::Item;

#[derive(Clone, Copy, Debug)]
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

/// == core `constants.rs` values.
pub fn context_message_name(_k: ContextKind) -> &'static str {
    unimplemented!()
}

pub fn build_context_message(_k: ContextKind, _text: String) -> Item {
    unimplemented!()
}

/// Keyed off `name`.
pub fn is_contextual_message(_i: &Item) -> bool {
    unimplemented!()
}

pub fn move_workspace_context_before_first_user(
    _items: &mut Vec<Item>,
    _inject_default_permissions: bool,
) {
    unimplemented!()
}

/// Baseline snapshot (open q: fields).
#[derive(Clone, Debug)]
pub struct TurnContextItem(pub serde_json::Value);

pub fn build_settings_update_items(
    _prev: Option<&TurnContextItem>,
    _next: &TurnContextItem,
) -> Vec<Item> {
    unimplemented!()
}
