//! `context/user_input.rs` — typed `session.input` event-payload builders.
//!
//! These build the durable, typed `session.input` event payload the tui/cli append
//! when a user submits a turn — from either raw text
//! ([`typed_user_input_payload_from_text_for_cwd`]) or pre-structured items
//! ([`typed_user_input_payload_from_items_for_cwd`]). The payload shape is exactly
//! what the reducer in [`crate::session::reconstruct`] consumes
//! (`collab_input_from_payload`: `{text, content, items?, skill_context_messages?,
//! mention_context_messages?, app_connector_ids?, plugin_mentions?}`).
//!
//! ## Ported faithfully vs deferred
//!
//! Ported verbatim-in-behavior from legacy `browser-use-core` (`lib.rs`):
//!   * `typed_user_input_payload_from_text_for_cwd` (lib.rs:21204) /
//!     `typed_user_input_payload_from_items_for_cwd` (lib.rs:21215) — the public
//!     builders.
//!   * `typed_collab_input_from_text` (lib.rs:21223) — linked-mention parse, else
//!     plain text.
//!   * `collab_input_from_items` (lib.rs:21188) / `collab_input_from_item_values`
//!     (lib.rs:21247) — items -> `CollabInput`.
//!   * `collab_input_event_payload` (lib.rs:21287) — `CollabInput` -> typed payload.
//!   * the linked-mention parser and item helpers (`collab_items_from_linked_mentions`
//!     lib.rs:21447, `collab_item_*` lib.rs:21571-21645) and the item-derived metadata
//!     helpers (`skill_context_messages_from_items` lib.rs:10396,
//!     `app_connector_ids_from_items` lib.rs:10320, `plugin_mentions_from_items`
//!     lib.rs:10339, `unique_non_empty_*` lib.rs:10362).
//!
//! Deferred (documented, NOT silently dropped): the legacy `_for_cwd` variants run
//! `collab_input_event_payload_with_context` (lib.rs:21316), which additionally
//! *materializes* `$skill` / `@plugin` plain-text mentions into
//! `skill_context_messages` / `mention_context_messages` by discovering AGENTS.md
//! config + skill summaries + plugin capability summaries
//! (`load_agents_md_config_for_options`, `available_skill_summaries`,
//! `browser_use_terminal_plugin_capability_summaries_for_config`,
//! `render_explicit_plugin_instructions`). That discovery infra is not present in the
//! agent crate yet, so the cwd is accepted (for signature parity and future wiring)
//! but the plain-mention materialization is a no-op here. Explicit `skill` items
//! still produce `skill_context_messages` (that path reads only the item's own
//! `SKILL.md`). Local-image inlining (`push_collab_local_image_parts`, which needs the
//! legacy `prompt_image` module) is likewise deferred: a `local_image` item renders the
//! same legacy text marker rather than inlining the image bytes.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

/// The structured collaboration input legacy assembles before serializing the
/// `session.input` event payload. Mirrors legacy `CollabInput` (lib.rs:21127).
#[derive(Clone)]
struct CollabInput {
    preview: String,
    content: Value,
    items: Option<Vec<Value>>,
    skill_context_messages: Option<Vec<Value>>,
    mention_context_messages: Option<Vec<Value>>,
    app_connector_ids: Option<Vec<String>>,
    plugin_mentions: Option<Vec<Value>>,
}

/// Build the typed `session.input` payload from raw user `text` for `cwd`.
///
/// Parity: legacy `typed_user_input_payload_from_text_for_cwd` (lib.rs:21204).
/// `cwd` is accepted for signature parity (and future plain-mention materialization);
/// see the module-level note on the deferred discovery infra.
pub fn typed_user_input_payload_from_text_for_cwd(
    text: &str,
    cwd: impl AsRef<Path>,
) -> anyhow::Result<Value> {
    collab_input_event_payload_with_context(&typed_collab_input_from_text(text), cwd.as_ref())
}

/// Build the typed `session.input` payload from structured `items` for `cwd`.
///
/// Parity: legacy `typed_user_input_payload_from_items_for_cwd` (lib.rs:21215):
/// `collab_input_from_items` (mapping its `String` error through `anyhow`) then
/// `collab_input_event_payload_with_context`.
pub fn typed_user_input_payload_from_items_for_cwd(
    items: &Value,
    cwd: impl AsRef<Path>,
) -> anyhow::Result<Value> {
    let input = collab_input_from_items(items).map_err(anyhow::Error::msg)?;
    collab_input_event_payload_with_context(&input, cwd.as_ref())
}

/// Plain-text -> `CollabInput`: linked-mention parse if any links resolve, else a
/// single text input. Parity: legacy `typed_collab_input_from_text` (lib.rs:21223).
fn typed_collab_input_from_text(text: &str) -> CollabInput {
    if let Some(items) = collab_items_from_linked_mentions(text) {
        return collab_input_from_item_values(text.to_string(), items);
    }
    CollabInput {
        preview: text.to_string(),
        content: Value::String(text.to_string()),
        items: None,
        skill_context_messages: None,
        mention_context_messages: None,
        app_connector_ids: None,
        plugin_mentions: None,
    }
}

/// Structured items -> `CollabInput`. Parity: legacy `collab_input_from_items`
/// (lib.rs:21188): rejects non-arrays / empty, builds a preview, then folds the items.
fn collab_input_from_items(items: &Value) -> Result<CollabInput, String> {
    let Some(items) = items.as_array() else {
        return Err("items must be an array".to_string());
    };
    if items.is_empty() {
        return Err("Items can't be empty".to_string());
    }
    let item_values = items.clone();
    let preview = collab_items_preview(&item_values);
    Ok(collab_input_from_item_values(preview, item_values))
}

/// Newline-join the per-item previews (skipping empties). Parity: legacy
/// `collab_items_preview` (lib.rs:21238).
fn collab_items_preview(items: &[Value]) -> String {
    items
        .iter()
        .map(collab_item_preview)
        .filter(|preview| !preview.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Items -> `CollabInput`: content parts, fallback preview, skill-context messages,
/// app-connector ids, plugin mentions. Parity: legacy `collab_input_from_item_values`
/// (lib.rs:21247).
fn collab_input_from_item_values(preview: String, item_values: Vec<Value>) -> CollabInput {
    let mut parts = Vec::new();
    for item in &item_values {
        collab_item_content_parts(item, &mut parts);
    }
    let fallback_preview = item_values
        .iter()
        .filter(|item| collab_item_allows_preview_fallback(item))
        .map(collab_item_preview)
        .filter(|preview| !preview.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if parts.is_empty() && !fallback_preview.trim().is_empty() {
        parts.push(serde_json::json!({
            "type": "input_text",
            "text": fallback_preview,
        }));
    }
    let skill_context_messages = if collab_items_include_skill(&item_values) {
        unique_non_empty_array(skill_context_messages_from_items(&item_values))
    } else {
        None
    };
    let mut app_connector_ids = app_connector_ids_from_items(&item_values);
    if let Some(messages) = skill_context_messages.as_ref() {
        app_connector_ids.extend(app_connector_ids_from_skill_context_messages(messages));
    }
    let app_connector_ids = unique_non_empty_strings(app_connector_ids);
    let plugin_mentions = unique_non_empty_array(plugin_mentions_from_items(&item_values));
    CollabInput {
        preview,
        content: Value::Array(parts),
        items: Some(item_values),
        skill_context_messages,
        mention_context_messages: None,
        app_connector_ids,
        plugin_mentions,
    }
}

/// `CollabInput` -> typed `session.input` payload. Parity: legacy
/// `collab_input_event_payload` (lib.rs:21287): always emits `text` (trimmed preview)
/// + `content`, and each optional metadata field only when present.
fn collab_input_event_payload(input: &CollabInput) -> Value {
    let mut payload = serde_json::json!({
        "text": input.preview.trim(),
        "content": input.content.clone(),
    });
    if let Some(items) = input.items.as_ref() {
        payload["items"] = Value::Array(items.clone());
    }
    if let Some(messages) = input.skill_context_messages.as_ref() {
        payload["skill_context_messages"] = Value::Array(messages.clone());
    }
    if let Some(messages) = input.mention_context_messages.as_ref() {
        payload["mention_context_messages"] = Value::Array(messages.clone());
    }
    if let Some(connector_ids) = input.app_connector_ids.as_ref() {
        payload["app_connector_ids"] = Value::Array(
            connector_ids
                .iter()
                .cloned()
                .map(Value::String)
                .collect::<Vec<_>>(),
        );
    }
    if let Some(mentions) = input.plugin_mentions.as_ref() {
        payload["plugin_mentions"] = Value::Array(mentions.clone());
    }
    payload
}

/// `CollabInput` + `cwd` -> typed payload. Parity: legacy
/// `collab_input_event_payload_with_context` (lib.rs:21316) — which clones the input,
/// materializes plain `$skill` / `@plugin` mentions against the cwd, then serializes.
///
/// The plain-mention materialization needs AGENTS.md / skill-summary / plugin-summary
/// discovery infra not yet in the agent crate (see the module note), so here it is a
/// no-op: `cwd` is bound (suppressing the unused warning) and the input is serialized
/// directly. Explicit `skill` items already produced their `skill_context_messages`
/// upstream, so the typed shape is preserved for the common cases.
fn collab_input_event_payload_with_context(
    input: &CollabInput,
    cwd: &Path,
) -> anyhow::Result<Value> {
    let _ = cwd; // Deferred: plain-mention materialization (skill/plugin discovery).
    Ok(collab_input_event_payload(input))
}

// ---- linked-mention parsing (markdown `[label](target)` -> items) ----

/// Parse `[label](target)` links into collab items, splicing the surrounding text as
/// `text` items. Returns `None` if no link resolves to an item. Parity: legacy
/// `collab_items_from_linked_mentions` (lib.rs:21447).
fn collab_items_from_linked_mentions(text: &str) -> Option<Vec<Value>> {
    let mut items = Vec::new();
    let mut search_from = 0;
    let mut last_emit = 0;
    let mut found = false;

    while let Some(open_rel) = text[search_from..].find('[') {
        let open = search_from + open_rel;
        let Some(close_rel) = text[open + 1..].find(']') else {
            break;
        };
        let close = open + 1 + close_rel;
        if !text[close + 1..].starts_with('(') {
            search_from = open + 1;
            continue;
        }
        let target_start = close + 2;
        let Some(target_rel) = text[target_start..].find(')') else {
            break;
        };
        let target_end = target_start + target_rel;
        let label = &text[open + 1..close];
        let target = &text[target_start..target_end];
        let Some(item) = collab_item_from_linked_target(label, target) else {
            search_from = open + 1;
            continue;
        };
        push_collab_text_item(&mut items, &text[last_emit..open]);
        items.push(item);
        found = true;
        last_emit = target_end + 1;
        search_from = last_emit;
    }

    if !found {
        return None;
    }
    push_collab_text_item(&mut items, &text[last_emit..]);
    Some(items)
}

/// `[label](target)` -> a `mention`/`skill` item, or `None`. Parity: legacy
/// `collab_item_from_linked_target` (lib.rs:21488): `app://`/`plugin://` -> mention
/// (only with `$`/`@` label prefixes resp.), `skill://…/SKILL.md` -> skill (with `$`).
fn collab_item_from_linked_target(label: &str, target: &str) -> Option<Value> {
    let target = target.trim();
    let display_name = linked_target_display_name(label, target);
    let label = label.trim();
    if target.starts_with("app://") {
        if !label.starts_with('$') {
            return None;
        }
        return Some(serde_json::json!({
            "type": "mention",
            "name": display_name,
            "path": target,
        }));
    }
    if target.starts_with("plugin://") {
        if !label.starts_with('@') {
            return None;
        }
        return Some(serde_json::json!({
            "type": "mention",
            "name": display_name,
            "path": target,
        }));
    }
    let Some(skill_path) = target.strip_prefix("skill://") else {
        return None;
    };
    if !label.starts_with('$') {
        return None;
    }
    let skill_path = skill_path.trim();
    if skill_path.is_empty()
        || !Path::new(skill_path)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "SKILL.md")
    {
        return None;
    }
    Some(serde_json::json!({
        "type": "skill",
        "name": display_name,
        "path": skill_path,
    }))
}

/// Display name for a linked mention: the label (minus a leading `$`/`@`), else a
/// derived id from the target. Parity: legacy `linked_target_display_name`
/// (lib.rs:21534).
fn linked_target_display_name(label: &str, target: &str) -> String {
    let label = label
        .trim()
        .trim_start_matches(['$', '@'])
        .trim()
        .to_string();
    if !label.is_empty() {
        return label;
    }
    if let Some(connector_id) = target.strip_prefix("app://") {
        return connector_id.to_string();
    }
    if let Some(plugin_id) = target.strip_prefix("plugin://") {
        return plugin_id.to_string();
    }
    target
        .strip_prefix("skill://")
        .and_then(|path| {
            Path::new(path)
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
        })
        .unwrap_or_default()
        .to_string()
}

/// Push a `text` item for `text` (skipping empties). Parity: legacy
/// `push_collab_text_item` (lib.rs:21561).
fn push_collab_text_item(items: &mut Vec<Value>, text: &str) {
    if text.is_empty() {
        return;
    }
    items.push(serde_json::json!({
        "type": "text",
        "text": text,
    }));
}

// ---- per-item helpers (preview / fallback / content parts) ----

/// One-line preview for an item. Parity: legacy `collab_item_preview` (lib.rs:21571).
fn collab_item_preview(item: &Value) -> String {
    match item.get("type").and_then(Value::as_str) {
        Some("text") => item
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Some("image") => "[image]".to_string(),
        Some("local_image") => format!(
            "[local_image:{}]",
            item.get("path").and_then(Value::as_str).unwrap_or_default()
        ),
        Some("skill") => format!(
            "[skill:${}]({})",
            item.get("name").and_then(Value::as_str).unwrap_or_default(),
            item.get("path").and_then(Value::as_str).unwrap_or_default()
        ),
        Some("mention") => format!(
            "[mention:${}]({})",
            item.get("name").and_then(Value::as_str).unwrap_or_default(),
            item.get("path").and_then(Value::as_str).unwrap_or_default()
        ),
        _ => "[input]".to_string(),
    }
}

/// Whether an item may seed the fallback preview (skill/mention do not). Parity:
/// legacy `collab_item_allows_preview_fallback` (lib.rs:21597).
fn collab_item_allows_preview_fallback(item: &Value) -> bool {
    !matches!(
        item.get("type").and_then(Value::as_str),
        Some("skill") | Some("mention")
    )
}

/// Append an item's provider content parts. Parity: legacy
/// `collab_item_content_parts` (lib.rs:21604).
///
/// `local_image` differs from legacy in the deferred case: legacy
/// `push_collab_local_image_parts` inlines the image bytes via the `prompt_image`
/// module (not in the agent crate). Here it emits the same legacy text marker the
/// preview uses, so the typed shape stays text-only rather than referencing a missing
/// module.
fn collab_item_content_parts(item: &Value, parts: &mut Vec<Value>) {
    match item.get("type").and_then(Value::as_str) {
        Some("text") => {
            if let Some(text) = item
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                parts.push(serde_json::json!({
                    "type": "input_text",
                    "text": text,
                }));
            }
        }
        Some("image") => {
            if let Some(image_url) = item
                .get("image_url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
            {
                push_collab_image_parts(
                    parts,
                    None,
                    image_url.to_string(),
                    item.get("detail").and_then(Value::as_str).unwrap_or("high"),
                );
            }
        }
        Some("local_image") => {
            // Deferred: legacy inlines the image via `prompt_image`. Emit the marker.
            parts.push(serde_json::json!({
                "type": "input_text",
                "text": collab_item_preview(item),
            }));
        }
        Some("skill") | Some("mention") => {}
        _ => {
            parts.push(serde_json::json!({
                "type": "input_text",
                "text": collab_item_preview(item),
            }));
        }
    }
}

/// Emit the `<image …>` text / `input_image` / `</image>` part triple for a remote
/// image. Parity: legacy `push_collab_image_parts` (lib.rs:21647).
fn push_collab_image_parts(
    parts: &mut Vec<Value>,
    label: Option<String>,
    image_url: String,
    detail: &str,
) {
    parts.push(serde_json::json!({
        "type": "input_text",
        "text": label
            .map(|label| format!("<image name={label}>"))
            .unwrap_or_else(|| "<image>".to_string()),
    }));
    parts.push(serde_json::json!({
        "type": "input_image",
        "image_url": image_url,
        "detail": detail,
    }));
    parts.push(serde_json::json!({
        "type": "input_text",
        "text": "</image>",
    }));
}

// ---- item-derived metadata (skill ctx / connectors / plugin mentions) ----

/// Whether any item is a `skill` item. Parity: legacy `collab_items_include_skill`
/// (lib.rs:21441).
fn collab_items_include_skill(items: &[Value]) -> bool {
    items
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("skill"))
}

/// `skill` items -> developer/user `skill_context_messages`, reading each item's own
/// `SKILL.md`. Parity: legacy `skill_context_messages_from_items` (lib.rs:10396):
/// de-dups by path, requires the file to be named `SKILL.md`, skips unreadable files.
fn skill_context_messages_from_items(items: &[Value]) -> Vec<Value> {
    let mut seen_paths = HashSet::new();
    let mut messages = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("skill") {
            continue;
        }
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        let path = item
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if path.is_empty() || !seen_paths.insert(path.to_string()) {
            continue;
        }
        if !Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "SKILL.md")
        {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        messages.push(serde_json::json!({
            "role": "user",
            "content": format!(
                "<skill>\n<name>{}</name>\n<path>{}</path>\n{}\n</skill>",
                name,
                path,
                contents,
            ),
        }));
    }
    messages
}

/// `app://` connector ids declared by items (de-duped, in order). Parity: legacy
/// `app_connector_ids_from_items` (lib.rs:10320).
fn app_connector_ids_from_items(items: &[Value]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut connectors = Vec::new();
    for item in items {
        let Some(path) = item.get("path").and_then(Value::as_str) else {
            continue;
        };
        let Some(connector_id) = path.strip_prefix("app://") else {
            continue;
        };
        let connector_id = connector_id.trim();
        if connector_id.is_empty() || !seen.insert(connector_id.to_string()) {
            continue;
        }
        connectors.push(connector_id.to_string());
    }
    connectors
}

/// `app://` connector ids referenced by links *inside* skill-context message text.
/// Parity: legacy `app_connector_ids_from_skill_context_messages` (lib.rs:10439) ->
/// `app_connector_ids_from_linked_text` (lib.rs:10449): re-parse each message's text
/// for linked mentions and collect their connector ids.
fn app_connector_ids_from_skill_context_messages(messages: &[Value]) -> Vec<String> {
    let mut connector_ids = Vec::new();
    for message in messages {
        let text = message_content_text(message);
        let ids = collab_items_from_linked_mentions(&text)
            .map(|items| app_connector_ids_from_items(&items))
            .unwrap_or_default();
        connector_ids.extend(ids);
    }
    connector_ids
}

/// `plugin://` mentions declared by items. Parity: legacy `plugin_mentions_from_items`
/// (lib.rs:10339): de-duped by path, each carrying `{name, path, plugin}`.
fn plugin_mentions_from_items(items: &[Value]) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut mentions = Vec::new();
    for item in items {
        let Some(path) = item.get("path").and_then(Value::as_str) else {
            continue;
        };
        let Some(plugin_id) = path.strip_prefix("plugin://") else {
            continue;
        };
        let plugin_id = plugin_id.trim();
        if plugin_id.is_empty() || !seen.insert(path.to_string()) {
            continue;
        }
        mentions.push(serde_json::json!({
            "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
            "path": path,
            "plugin": plugin_id,
        }));
    }
    mentions
}

/// Flatten a message's `content` to text (string, or joined `text` parts). Parity:
/// legacy `message_content_text` (lib.rs:10256). Mirrors the same flattening the
/// reconstruct reducer uses on provider messages.
fn message_content_text(message: &Value) -> String {
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
            .join(""),
        _ => String::new(),
    }
}

/// `Some(values)` iff non-empty. Parity: legacy `unique_non_empty_array`
/// (lib.rs:10362).
fn unique_non_empty_array<T>(values: Vec<T>) -> Option<Vec<T>> {
    (!values.is_empty()).then_some(values)
}

/// Trim + de-dup strings, then `Some` iff non-empty. Parity: legacy
/// `unique_non_empty_strings` (lib.rs:10366).
fn unique_non_empty_strings(values: Vec<String>) -> Option<Vec<String>> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() || !seen.insert(value.to_string()) {
            continue;
        }
        unique.push(value.to_string());
    }
    unique_non_empty_array(unique)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_payload_has_text_and_string_content() {
        let payload = typed_user_input_payload_from_text_for_cwd("hello world", "/tmp").unwrap();
        assert_eq!(payload["text"], "hello world");
        assert_eq!(payload["content"], "hello world");
        // No items / metadata for plain text.
        assert!(payload.get("items").is_none());
        assert!(payload.get("skill_context_messages").is_none());
        assert!(payload.get("plugin_mentions").is_none());
        assert!(payload.get("app_connector_ids").is_none());
    }

    #[test]
    fn plain_text_payload_trims_preview_but_not_content() {
        // Parity: `text` is the trimmed preview; `content` keeps the raw string.
        let payload = typed_user_input_payload_from_text_for_cwd("  spaced  ", "/tmp").unwrap();
        assert_eq!(payload["text"], "spaced");
        assert_eq!(payload["content"], "  spaced  ");
    }

    #[test]
    fn linked_skill_mention_becomes_skill_item_and_content() {
        // A `$label](skill://…/SKILL.md)` link splits into text + skill items. The
        // skill item carries no content part (skill/mention emit none), so the
        // surrounding text drives `content`.
        let text = "see [$mySkill](skill:///nope/SKILL.md) now";
        let payload = typed_user_input_payload_from_text_for_cwd(text, "/tmp").unwrap();
        // preview is the full raw text (linked-mention path keeps the original text).
        assert_eq!(payload["text"], text);
        // items: text("see ") + skill + text(" now")
        let items = payload["items"].as_array().expect("items array");
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["type"], "text");
        assert_eq!(items[1]["type"], "skill");
        assert_eq!(items[1]["name"], "mySkill");
        assert_eq!(items[1]["path"], "/nope/SKILL.md");
        assert_eq!(items[2]["type"], "text");
        // content: input_text("see ") + input_text(" now") (skill emits no part).
        let content = payload["content"].as_array().expect("content array");
        assert_eq!(content.len(), 2);
        assert!(content.iter().all(|part| part["type"] == "input_text"));
        // The unreadable SKILL.md is skipped -> no skill_context_messages.
        assert!(payload.get("skill_context_messages").is_none());
    }

    #[test]
    fn linked_app_mention_yields_connector_id_and_plugin_mentions_for_plugin() {
        let text = "[$db](app://my-connector) and [@tool](plugin://my-plugin)";
        let payload = typed_user_input_payload_from_text_for_cwd(text, "/tmp").unwrap();
        let items = payload["items"].as_array().expect("items array");
        // Both are mention items.
        let mention_count = items
            .iter()
            .filter(|item| item["type"] == "mention")
            .count();
        assert_eq!(mention_count, 2);
        // app:// -> app_connector_ids.
        let connectors = payload["app_connector_ids"].as_array().expect("connectors");
        assert_eq!(connectors.len(), 1);
        assert_eq!(connectors[0], "my-connector");
        // plugin:// -> plugin_mentions {name, path, plugin}.
        let plugins = payload["plugin_mentions"].as_array().expect("plugins");
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0]["plugin"], "my-plugin");
        assert_eq!(plugins[0]["path"], "plugin://my-plugin");
    }

    #[test]
    fn items_payload_builds_content_parts() {
        let items = serde_json::json!([
            { "type": "text", "text": "first" },
            { "type": "image", "image_url": "https://x/y.png" },
        ]);
        let payload = typed_user_input_payload_from_items_for_cwd(&items, "/tmp").unwrap();
        // preview newline-joins per-item previews (text + "[image]").
        assert_eq!(payload["text"], "first\n[image]");
        let content = payload["content"].as_array().expect("content array");
        // input_text("first") + <image> triple = 4 parts.
        assert_eq!(content.len(), 4);
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[0]["text"], "first");
        assert_eq!(content[1]["type"], "input_text");
        assert_eq!(content[1]["text"], "<image>");
        assert_eq!(content[2]["type"], "input_image");
        assert_eq!(content[2]["image_url"], "https://x/y.png");
        assert_eq!(content[3]["text"], "</image>");
        // Original items are echoed back.
        assert_eq!(payload["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn items_payload_rejects_empty_and_non_array() {
        assert!(
            typed_user_input_payload_from_items_for_cwd(&serde_json::json!([]), "/tmp").is_err()
        );
        assert!(typed_user_input_payload_from_items_for_cwd(
            &serde_json::json!("not an array"),
            "/tmp"
        )
        .is_err());
    }

    #[test]
    fn skill_item_reads_skill_md_into_context_messages() {
        // A real SKILL.md on disk -> a skill_context_message with the wrapped body.
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("mySkill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        std::fs::write(&skill_md, "do the thing").unwrap();
        let items = serde_json::json!([
            { "type": "skill", "name": "mySkill", "path": skill_md.to_str().unwrap() },
        ]);
        let payload = typed_user_input_payload_from_items_for_cwd(&items, dir.path()).unwrap();
        let messages = payload["skill_context_messages"]
            .as_array()
            .expect("skill_context_messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.starts_with("<skill>\n<name>mySkill</name>"));
        assert!(content.contains("do the thing"));
        assert!(content.ends_with("</skill>"));
    }
}
