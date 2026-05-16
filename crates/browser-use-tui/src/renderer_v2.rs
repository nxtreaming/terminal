use browser_use_protocol::{EventRecord, SessionMeta, WorkbenchState};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::markdown::render_markdown_lines;
use crate::theme::{accent, dim, failed, link, muted, text_style, thought};

use super::App;

pub(crate) const ACTIVE_BODY_RESERVE: u16 = 8;

pub(crate) type RenderCellId = String;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DisplayMode {
    Scrollback,
    Active,
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptModel {
    pub(crate) session_id: String,
    pub(crate) committed: Vec<TranscriptNode>,
    pub(crate) active: Option<TranscriptNode>,
    pub(crate) last_event_seq: i64,
    pub(crate) revision: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptNode {
    id: RenderCellId,
    seq: i64,
    revision: u64,
    kind: TranscriptKind,
}

#[derive(Clone, Debug)]
enum TranscriptKind {
    Prompt {
        text: String,
        followup: bool,
    },
    Assistant {
        markdown: String,
        source: Option<String>,
    },
    StreamingAssistant {
        markdown: String,
    },
    Timeline {
        group: String,
        lines: Vec<String>,
        style: NodeStyle,
    },
    ActiveStatus {
        group: String,
        lines: Vec<String>,
        style: NodeStyle,
    },
    Error {
        text: String,
    },
    Cancelled {
        text: String,
    },
}

#[derive(Clone, Copy, Debug)]
enum NodeStyle {
    Normal,
    Muted,
    Failed,
    Thought,
}

#[allow(dead_code)]
pub(crate) trait RenderCell {
    fn id(&self) -> &RenderCellId;
    fn seq(&self) -> i64;
    fn revision(&self) -> u64;
    fn display_lines(&self, width: u16, mode: DisplayMode) -> Vec<Line<'static>>;
    fn plain_lines(&self) -> Vec<String>;

    fn desired_height(&self, width: u16, mode: DisplayMode) -> u16 {
        self.display_lines(width, mode)
            .len()
            .try_into()
            .unwrap_or(u16::MAX)
    }
}

impl RenderCell for TranscriptNode {
    fn id(&self) -> &RenderCellId {
        &self.id
    }

    fn seq(&self) -> i64 {
        self.seq
    }

    fn revision(&self) -> u64 {
        self.revision
    }

    fn display_lines(&self, width: u16, mode: DisplayMode) -> Vec<Line<'static>> {
        let width = width.max(1);
        match &self.kind {
            TranscriptKind::Prompt { text, followup } => prompt_lines(text, *followup, width),
            TranscriptKind::Assistant { markdown, source } => {
                let mut lines = markdown_cell_lines(markdown, width, mode);
                if let Some(source) = source.as_deref() {
                    lines.extend(source_display_lines(source, width));
                }
                lines
            }
            TranscriptKind::StreamingAssistant { markdown } => {
                markdown_cell_lines(markdown, width, mode)
            }
            TranscriptKind::Timeline {
                group,
                lines,
                style,
            }
            | TranscriptKind::ActiveStatus {
                group,
                lines,
                style,
            } => grouped_lines(group, lines, *style, width),
            TranscriptKind::Error { text } => grouped_lines(
                "error",
                &[friendly_error_message(text)],
                NodeStyle::Failed,
                width,
            ),
            TranscriptKind::Cancelled { text } => grouped_lines(
                "stopped",
                std::slice::from_ref(text),
                NodeStyle::Muted,
                width,
            ),
        }
    }

    fn plain_lines(&self) -> Vec<String> {
        match &self.kind {
            TranscriptKind::Prompt { text, .. } => prefixed_plain("> ", text),
            TranscriptKind::Assistant { markdown, source } => {
                let mut out = markdown.lines().map(str::to_string).collect::<Vec<_>>();
                if let Some(source) = source.as_ref() {
                    out.push(format!("source {source}"));
                }
                out
            }
            TranscriptKind::StreamingAssistant { markdown } => {
                markdown.lines().map(str::to_string).collect()
            }
            TranscriptKind::Timeline { group, lines, .. }
            | TranscriptKind::ActiveStatus { group, lines, .. } => {
                let mut out = vec![format!(": {group}")];
                out.extend(lines.iter().cloned());
                out
            }
            TranscriptKind::Error { text } => {
                vec![": error".to_string(), friendly_error_message(text)]
            }
            TranscriptKind::Cancelled { text } => vec![format!(": stopped"), text.clone()],
        }
    }
}

pub(crate) fn transcript_model(app: &App, state: &WorkbenchState) -> Option<TranscriptModel> {
    let session = state.current_session.as_ref()?;
    let events = app.cached_events_for_session(&session.id);
    let last_event_seq = events.last().map(|event| event.seq).unwrap_or_default();
    let mut committed = Vec::new();

    for event in events {
        if let Some(node) = committed_node_for_event(app, state, session, event) {
            committed.push(node);
        }
    }

    let active = if session.status.is_active() {
        active_node_for_session(app, state, session, events)
    } else {
        None
    };

    let revision = last_event_seq.max(0) as u64;
    Some(TranscriptModel {
        session_id: session.id.clone(),
        committed,
        active,
        last_event_seq,
        revision,
    })
}

pub(crate) fn scrollback_lines_since(
    model: &TranscriptModel,
    after_seq: i64,
    width: u16,
) -> Vec<Line<'static>> {
    cells_to_lines(
        model.committed.iter().filter(|node| node.seq() > after_seq),
        width,
        DisplayMode::Scrollback,
    )
}

pub(crate) fn all_scrollback_lines(model: &TranscriptModel, width: u16) -> Vec<Line<'static>> {
    cells_to_lines(model.committed.iter(), width, DisplayMode::Scrollback)
}

pub(crate) fn active_viewport_lines(
    model: Option<&TranscriptModel>,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let Some(active) = model.and_then(|model| model.active.as_ref()) else {
        return Vec::new();
    };
    let mut lines = active.display_lines(width, DisplayMode::Active);
    if lines.len() > height as usize {
        let start = lines.len().saturating_sub(height as usize);
        lines = lines.into_iter().skip(start).collect();
    }
    lines
}

pub(crate) fn model_plain_text(model: &TranscriptModel) -> String {
    let mut out = String::new();
    for node in &model.committed {
        for line in node.plain_lines() {
            out.push_str(&line);
            out.push('\n');
        }
        out.push('\n');
    }
    if let Some(active) = model.active.as_ref() {
        for line in active.plain_lines() {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

fn cells_to_lines<'a>(
    nodes: impl Iterator<Item = &'a TranscriptNode>,
    width: u16,
    mode: DisplayMode,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for node in nodes {
        let _ = (node.id(), node.revision());
        if !out.is_empty() && !matches!(node.kind, TranscriptKind::Prompt { .. }) {
            out.push(Line::from(""));
        }
        out.extend(node.display_lines(width, mode));
    }
    out
}

fn committed_node_for_event(
    app: &App,
    state: &WorkbenchState,
    root: &SessionMeta,
    event: &EventRecord,
) -> Option<TranscriptNode> {
    if event.session_id != root.id {
        return None;
    }
    let id = format!("{}:{}", event.session_id, event.seq);
    match event.event_type.as_str() {
        "session.input" | "session.followup" => {
            let text = payload_string(event, "text")?;
            Some(TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::Prompt {
                    text,
                    followup: event.event_type == "session.followup",
                },
            })
        }
        "session.done" => {
            let result = payload_string(event, "result")?;
            Some(TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::Assistant {
                    markdown: result,
                    source: source_for_state(state),
                },
            })
        }
        "session.failed" => {
            let text =
                payload_string(event, "error").unwrap_or_else(|| "The task failed.".to_string());
            Some(TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::Error { text },
            })
        }
        "session.cancelled" => Some(TranscriptNode {
            id,
            seq: event.seq,
            revision: event.seq.max(0) as u64,
            kind: TranscriptKind::Cancelled {
                text: "Progress is saved in history.".to_string(),
            },
        }),
        // Child-agent lifecycle is represented by the active child cell while
        // it is running and by agent.completed once it has a result. Emitting
        // agent.spawned separately makes the transcript look like duplicate
        // subagent blocks.
        "agent.spawned" => None,
        "agent.completed" => {
            let label = event
                .payload
                .get("child_session_id")
                .and_then(serde_json::Value::as_str)
                .map(|id| helper_label_for_session(app, id))
                .unwrap_or_else(|| "subagent".to_string());
            let group = format!("subagent {label}");
            let mut lines = vec!["finished".to_string()];
            if let Some(result) = event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("result"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                lines.extend(markdown_preview_lines(result, 3));
            }
            Some(timeline_node(event, &group, lines, NodeStyle::Normal))
        }
        "agent.failed" => {
            let error = event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("error"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("subagent failed")
                .to_string();
            Some(timeline_node(
                event,
                "error",
                vec![error],
                NodeStyle::Failed,
            ))
        }
        "agent.cancelled" => Some(timeline_node(
            event,
            "subagent",
            vec!["subagent stopped".to_string()],
            NodeStyle::Muted,
        )),
        "model.tool_call" => model_tool_call_node(event),
        "tool.started" => tool_started_node(event),
        "tool.output" => tool_output_node(event),
        "tool.image" => Some(timeline_node(
            event,
            "image",
            vec!["received image artifact".to_string()],
            NodeStyle::Normal,
        )),
        "tool.failed" => {
            let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
            let error = payload_string(event, "error").unwrap_or_else(|| "tool failed".to_string());
            Some(timeline_node(
                event,
                "error",
                vec![format!("{name} failed: {error}")],
                NodeStyle::Failed,
            ))
        }
        "tool.output_spilled" => {
            let path = event
                .payload
                .get("artifact")
                .and_then(|artifact| artifact.get("path"))
                .and_then(serde_json::Value::as_str)
                .map(|path| display_path(path, state))
                .unwrap_or_else(|| "artifact".to_string());
            Some(timeline_node(
                event,
                "run",
                vec![format!("Full output saved to {path}")],
                NodeStyle::Muted,
            ))
        }
        "file.list" => {
            let path = payload_string(event, "path")
                .map(|path| display_path(&path, state))
                .unwrap_or_else(|| ".".to_string());
            Some(timeline_node(event, "list", vec![path], NodeStyle::Normal))
        }
        "file.read" => {
            let path = payload_string(event, "path").map(|path| display_path(&path, state))?;
            Some(timeline_node(event, "read", vec![path], NodeStyle::Normal))
        }
        "file.search" => {
            let query = payload_string(event, "query").unwrap_or_else(|| "files".to_string());
            let matches = event
                .payload
                .get("matches")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            Some(timeline_node(
                event,
                "search",
                vec![format!("Search {query:?} ({matches} matches)")],
                NodeStyle::Normal,
            ))
        }
        "command.started" => {
            let cmd = payload_string(event, "cmd").unwrap_or_else(|| "command".to_string());
            Some(timeline_node(event, "run", vec![cmd], NodeStyle::Normal))
        }
        "command.output" => {
            let text = payload_string(event, "text")?;
            Some(timeline_node(
                event,
                "run",
                preview_lines(&text, 5),
                NodeStyle::Muted,
            ))
        }
        "command.finished" => {
            let failed = event
                .payload
                .get("success")
                .and_then(serde_json::Value::as_bool)
                .is_some_and(|success| !success);
            failed.then(|| {
                let code = event
                    .payload
                    .get("exit_code")
                    .and_then(serde_json::Value::as_i64)
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                timeline_node(
                    event,
                    "run",
                    vec![format!("failed with exit {code}")],
                    NodeStyle::Failed,
                )
            })
        }
        "patch.file_changed" => {
            let kind = payload_string(event, "kind").unwrap_or_else(|| "changed".to_string());
            let path = payload_string(event, "path")
                .map(|path| display_path(&path, state))
                .unwrap_or_else(|| "file".to_string());
            Some(timeline_node(
                event,
                "edit",
                vec![format!("{kind} {path}")],
                NodeStyle::Normal,
            ))
        }
        "browser.connected" | "browser.reconnected" | "browser.target_changed" => {
            Some(timeline_node(
                event,
                "browser",
                vec!["browser connected".to_string()],
                NodeStyle::Normal,
            ))
        }
        "browser.disconnected" => Some(timeline_node(
            event,
            "browser",
            vec!["browser disconnected".to_string()],
            NodeStyle::Muted,
        )),
        "browser.live_url" => Some(timeline_node(
            event,
            "browser",
            vec!["live view available".to_string()],
            NodeStyle::Normal,
        )),
        "browser.page" | "browser.state" => event
            .payload
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(|url| {
                timeline_node(
                    event,
                    "browser",
                    vec![format!("opened {}", compact_url(url))],
                    NodeStyle::Normal,
                )
            }),
        "plan.updated" => Some(timeline_node(
            event,
            "plan",
            vec!["updated plan".to_string()],
            NodeStyle::Normal,
        )),
        "model.turn.error" => Some(timeline_node(
            event,
            "error",
            vec!["model request hit an error".to_string()],
            NodeStyle::Failed,
        )),
        "model.turn.request"
        | "model.turn.response"
        | "model.turn.retry"
        | "model.stream_delta"
        | "model.thinking_delta"
        | "model.usage"
        | "session.created"
        | "session.status"
        | "session.final_answer_ready"
        | "session.final_answer_used"
        | "session.cancel_requested" => None,
        _ => None,
    }
}

fn active_node_for_session(
    app: &App,
    state: &WorkbenchState,
    root: &SessionMeta,
    events: &[EventRecord],
) -> Option<TranscriptNode> {
    if let Some(child) = active_child_session(app, &root.id) {
        let label = helper_label_for_session(app, &child.id);
        let group = format!("subagent {label}");
        let mut lines = vec!["working".to_string()];
        let recent = recent_child_activity_lines(app, state, &child.id, 6);
        if recent.is_empty() {
            lines.push("waiting for activity".to_string());
        } else {
            lines.extend(recent);
        }
        return Some(active_status_node(
            root,
            events,
            &group,
            lines,
            NodeStyle::Normal,
        ));
    }

    if let Some(text) = state
        .transcript
        .last()
        .and_then(|turn| turn.streaming_text.as_deref())
        .map(str::trim_end)
        .filter(|text| !text.is_empty())
    {
        return Some(TranscriptNode {
            id: format!("{}:active-stream", root.id),
            seq: events.last().map(|event| event.seq).unwrap_or_default(),
            revision: events
                .last()
                .map(|event| event.seq.max(0) as u64)
                .unwrap_or_default(),
            kind: TranscriptKind::StreamingAssistant {
                markdown: text.to_string(),
            },
        });
    }

    if let Some(text) = state
        .transcript
        .last()
        .and_then(|turn| turn.thinking_text.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return Some(active_status_node(
            root,
            events,
            "thinking",
            preview_lines(text, 5),
            NodeStyle::Thought,
        ));
    }

    if let Some(event) = events.iter().rev().find(|event| {
        matches!(
            event.event_type.as_str(),
            "model.turn.request"
                | "model.turn.retry"
                | "command.waiting"
                | "tool.started"
                | "browser.page"
                | "browser.state"
                | "plan.updated"
        )
    }) {
        return active_node_for_event(root, events, event);
    }

    Some(active_status_node(
        root,
        events,
        "status",
        vec!["running browser task".to_string()],
        NodeStyle::Muted,
    ))
}

fn active_node_for_event(
    root: &SessionMeta,
    events: &[EventRecord],
    event: &EventRecord,
) -> Option<TranscriptNode> {
    match event.event_type.as_str() {
        "model.turn.request" => {
            let model = payload_string(event, "model").unwrap_or_else(|| "model".to_string());
            Some(active_status_node(
                root,
                events,
                "thinking",
                vec![format!("waiting for {model}")],
                NodeStyle::Muted,
            ))
        }
        "model.turn.retry" => Some(active_status_node(
            root,
            events,
            "thinking",
            vec!["retrying model request".to_string()],
            NodeStyle::Muted,
        )),
        "command.waiting" => Some(active_status_node(
            root,
            events,
            "run",
            vec!["command still running".to_string()],
            NodeStyle::Muted,
        )),
        "tool.started" => {
            let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
            Some(active_status_node(
                root,
                events,
                &name,
                vec!["running".to_string()],
                NodeStyle::Muted,
            ))
        }
        "browser.page" | "browser.state" => event
            .payload
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(|url| {
                active_status_node(
                    root,
                    events,
                    "browser",
                    vec![format!("opened {}", compact_url(url))],
                    NodeStyle::Muted,
                )
            }),
        "plan.updated" => Some(active_status_node(
            root,
            events,
            "plan",
            vec!["updated plan".to_string()],
            NodeStyle::Muted,
        )),
        _ => None,
    }
}

fn active_status_node(
    root: &SessionMeta,
    events: &[EventRecord],
    group: &str,
    lines: Vec<String>,
    style: NodeStyle,
) -> TranscriptNode {
    let seq = events.last().map(|event| event.seq).unwrap_or_default();
    TranscriptNode {
        id: format!("{}:active:{group}", root.id),
        seq,
        revision: seq.max(0) as u64,
        kind: TranscriptKind::ActiveStatus {
            group: group.to_string(),
            lines,
            style,
        },
    }
}

fn model_tool_call_node(event: &EventRecord) -> Option<TranscriptNode> {
    let name = event
        .payload
        .get("call")
        .and_then(|call| call.get("name"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| payload_string(event, "name"))
        .unwrap_or_else(|| "tool".to_string());
    if is_subagent_management_tool(&name) {
        return None;
    }
    Some(timeline_node(
        event,
        "tool",
        vec![format!("{name} requested")],
        NodeStyle::Muted,
    ))
}

fn tool_started_node(event: &EventRecord) -> Option<TranscriptNode> {
    let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
    if name == "exec_command" || is_subagent_management_tool(&name) {
        return None;
    }
    Some(timeline_node(
        event,
        &name,
        vec!["started".to_string()],
        NodeStyle::Muted,
    ))
}

fn tool_output_node(event: &EventRecord) -> Option<TranscriptNode> {
    let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
    if is_subagent_management_tool(&name) {
        return None;
    }
    let mut lines = Vec::new();
    if let Some(text) = payload_string(event, "text").filter(|text| !text.trim().is_empty()) {
        lines.extend(preview_lines(&text, 8));
    }
    if let Some(images) = event
        .payload
        .get("images")
        .and_then(serde_json::Value::as_array)
    {
        if !images.is_empty() {
            lines.push(format!(
                "{} image artifact{}",
                images.len(),
                plural(images.len())
            ));
        }
    }
    if let Some(artifacts) = event
        .payload
        .get("artifacts")
        .and_then(serde_json::Value::as_array)
    {
        if !artifacts.is_empty() {
            lines.push(format!(
                "{} file artifact{}",
                artifacts.len(),
                plural(artifacts.len())
            ));
        }
    }
    if lines.is_empty() {
        return None;
    }
    Some(timeline_node(event, &name, lines, NodeStyle::Muted))
}

fn timeline_node(
    event: &EventRecord,
    group: &str,
    lines: Vec<String>,
    style: NodeStyle,
) -> TranscriptNode {
    TranscriptNode {
        id: format!("{}:{}", event.session_id, event.seq),
        seq: event.seq,
        revision: event.seq.max(0) as u64,
        kind: TranscriptKind::Timeline {
            group: group.to_string(),
            lines,
            style,
        },
    }
}

fn prompt_lines(text: &str, _followup: bool, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (idx, source) in text.lines().enumerate() {
        let prefix = if idx == 0 { "> " } else { "  " };
        let content_width = width.saturating_sub(2).max(1);
        for (wrap_idx, wrapped) in wrap_plain(source, content_width) {
            let visible_prefix = if wrap_idx == 0 { prefix } else { "  " };
            lines.push(Line::from(vec![
                Span::styled(visible_prefix.to_string(), accent()),
                Span::styled(wrapped, text_style()),
            ]));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("> ", accent())));
    }
    lines
}

fn markdown_cell_lines(markdown: &str, width: u16, mode: DisplayMode) -> Vec<Line<'static>> {
    let _ = mode;
    let mut lines = render_markdown_lines(markdown.trim_end(), width);
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

fn source_display_lines(source: &str, width: u16) -> Vec<Line<'static>> {
    let prefix = "source ";
    let first_width = width.saturating_sub(prefix.chars().count() as u16).max(1);
    let continuation_prefix = "       ";
    let continuation_width = width
        .saturating_sub(continuation_prefix.chars().count() as u16)
        .max(1);
    let mut lines = Vec::new();
    for (idx, wrapped) in wrap_plain(source, first_width) {
        let prefix_text = if idx == 0 {
            prefix
        } else {
            continuation_prefix
        };
        let available = if idx == 0 {
            first_width
        } else {
            continuation_width
        };
        let fragments = if idx == 0 {
            vec![wrapped]
        } else {
            wrap_plain(&wrapped, available)
                .into_iter()
                .map(|(_, line)| line)
                .collect()
        };
        for (fragment_idx, fragment) in fragments.into_iter().enumerate() {
            let visible_prefix = if idx == 0 && fragment_idx == 0 {
                prefix_text
            } else {
                continuation_prefix
            };
            lines.push(Line::from(vec![
                Span::styled(visible_prefix.to_string(), muted()),
                Span::styled(fragment, link()),
            ]));
        }
    }
    lines
}

fn grouped_lines(
    group: &str,
    values: &[String],
    style: NodeStyle,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(": ", dim()),
        Span::styled(group.to_string(), group_style(style)),
    ]));
    let value_style = body_style(style);
    let content_width = width.saturating_sub(2).max(1);
    for value in values {
        for (_, wrapped) in wrap_plain(value, content_width) {
            lines.push(Line::from(vec![
                Span::raw("  "),
                styled_value(&wrapped, value_style),
            ]));
        }
    }
    lines
}

fn styled_value(text: &str, fallback: Style) -> Span<'static> {
    if text.starts_with("https://") || text.starts_with("http://") {
        Span::styled(text.to_string(), link())
    } else {
        Span::styled(text.to_string(), fallback)
    }
}

fn group_style(style: NodeStyle) -> Style {
    match style {
        NodeStyle::Normal => crate::theme::done(),
        NodeStyle::Muted => muted(),
        NodeStyle::Failed => failed(),
        NodeStyle::Thought => thought(),
    }
}

fn body_style(style: NodeStyle) -> Style {
    match style {
        NodeStyle::Normal => text_style(),
        NodeStyle::Muted => muted(),
        NodeStyle::Failed => failed(),
        NodeStyle::Thought => muted(),
    }
}

fn wrap_plain(value: &str, width: u16) -> Vec<(usize, String)> {
    let width = width.max(1) as usize;
    if value.is_empty() {
        return vec![(0, String::new())];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    let mut line_width = 0usize;
    let mut wrap_idx = 0usize;
    for ch in value.chars() {
        let ch_width = ch.width().unwrap_or(0).max(1);
        if line_width > 0 && line_width + ch_width > width {
            out.push((wrap_idx, std::mem::take(&mut line)));
            wrap_idx += 1;
            line_width = 0;
        }
        line.push(ch);
        line_width += ch_width;
    }
    out.push((wrap_idx, line));
    out
}

fn prefixed_plain(prefix: &str, text: &str) -> Vec<String> {
    text.lines()
        .enumerate()
        .map(|(idx, line)| {
            if idx == 0 {
                format!("{prefix}{line}")
            } else {
                format!("  {line}")
            }
        })
        .collect()
}

fn payload_string(event: &EventRecord, key: &str) -> Option<String> {
    event
        .payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn source_for_state(state: &WorkbenchState) -> Option<String> {
    state
        .browser
        .url
        .as_deref()
        .or(state.browser.live_url.as_deref())
        .filter(|source| is_useful_source(source))
        .map(ToOwned::to_owned)
}

fn is_useful_source(source: &str) -> bool {
    let source = source.trim();
    !source.is_empty() && source != "about:blank"
}

fn preview_lines(text: &str, limit: usize) -> Vec<String> {
    let mut out = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .take(limit)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if text
        .lines()
        .filter(|line| !line.trim_end().is_empty())
        .count()
        > out.len()
    {
        out.push(format!(
            "... +{} lines",
            text.lines().count().saturating_sub(out.len())
        ));
    }
    out
}

fn markdown_preview_lines(markdown: &str, limit: usize) -> Vec<String> {
    let rendered = render_markdown_lines(markdown.trim_end(), 100);
    let mut out = rendered
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .map(|line| line.trim_end().to_string())
        .filter(|line| !line.is_empty())
        .take(limit)
        .collect::<Vec<_>>();
    let total = rendered
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .filter(|line| !line.trim().is_empty())
        .count();
    if total > out.len() {
        out.push(format!("... +{} lines", total.saturating_sub(out.len())));
    }
    out
}

fn friendly_error_message(error: &str) -> String {
    let error = error.trim();
    if error.is_empty() {
        "The task failed.".to_string()
    } else {
        error.to_string()
    }
}

fn display_path(path: &str, state: &WorkbenchState) -> String {
    let Some(session) = state.current_session.as_ref() else {
        return path.to_string();
    };
    let cwd = session.cwd.trim_end_matches('/');
    path.strip_prefix(cwd)
        .and_then(|path| path.strip_prefix('/').or(Some(path)))
        .filter(|path| !path.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn compact_url(url: &str) -> String {
    const MAX: usize = 72;
    let compact = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))
        .unwrap_or_else(|| url.trim());
    if compact.chars().count() <= MAX {
        return compact.to_string();
    }
    let mut out = compact
        .chars()
        .take(MAX.saturating_sub(1))
        .collect::<String>();
    out.push_str("...");
    out
}

fn helper_label_for_session(app: &App, session_id: &str) -> String {
    if let Some(label) = app
        .cached_events_for_session(session_id)
        .iter()
        .find_map(|event| {
            if event.event_type != "agent.context" {
                return None;
            }
            event
                .payload
                .get("nickname")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    event
                        .payload
                        .get("role")
                        .and_then(serde_json::Value::as_str)
                })
                .or_else(|| {
                    event
                        .payload
                        .get("agent_path")
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .map(ToOwned::to_owned)
        })
    {
        return label;
    }

    if let Some(label) = app
        .state_cache
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .and_then(|session| session.parent_id.as_deref())
        .and_then(|parent_id| {
            app.cached_events_for_session(parent_id)
                .iter()
                .find(|event| {
                    event.event_type == "agent.spawned"
                        && event
                            .payload
                            .get("child_session_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(session_id)
                })
        })
        .and_then(|event| {
            event
                .payload
                .get("nickname")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    event
                        .payload
                        .get("role")
                        .and_then(serde_json::Value::as_str)
                })
        })
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(ToOwned::to_owned)
    {
        return label;
    }

    app.state_cache
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .and_then(|session| {
            session
                .parent_id
                .as_ref()
                .map(|_| session.id.chars().take(6).collect::<String>())
        })
        .unwrap_or_else(|| "subagent".to_string())
}

fn is_subagent_management_tool(name: &str) -> bool {
    matches!(
        name,
        "spawn_agent"
            | "wait_agent"
            | "send_input"
            | "send_message"
            | "followup_task"
            | "close_agent"
            | "resume_agent"
            | "list_agents"
    )
}

fn active_child_session<'a>(app: &'a App, root_id: &str) -> Option<&'a SessionMeta> {
    app.state_cache
        .sessions
        .iter()
        .filter(|session| session.parent_id.as_deref() == Some(root_id))
        .find(|session| session.status.is_active())
}

fn recent_child_activity_lines(
    app: &App,
    state: &WorkbenchState,
    child_id: &str,
    limit: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    for event in app.cached_events_for_session(child_id).iter().rev() {
        if let Some(line) = child_activity_line(event, state) {
            lines.push(line);
        }
        if lines.len() >= limit {
            break;
        }
    }
    lines.reverse();
    lines
}

fn child_activity_line(event: &EventRecord, state: &WorkbenchState) -> Option<String> {
    match event.event_type.as_str() {
        "session.input" => payload_string(event, "text")
            .map(|text| format!("task {}", truncate_text(text.trim(), 96))),
        "session.followup" => payload_string(event, "text")
            .map(|text| format!("follow-up {}", truncate_text(text.trim(), 96))),
        "file.read" => {
            payload_string(event, "path").map(|path| format!("read {}", display_path(&path, state)))
        }
        "file.list" => {
            let path = payload_string(event, "path").unwrap_or_else(|| ".".to_string());
            Some(format!("list {}", display_path(&path, state)))
        }
        "file.search" => {
            let query = payload_string(event, "query").unwrap_or_else(|| "files".to_string());
            let suffix = event
                .payload
                .get("matches")
                .and_then(serde_json::Value::as_u64)
                .map(|matches| format!(" ({matches} matches)"))
                .unwrap_or_default();
            Some(format!("search {query:?}{suffix}"))
        }
        "command.started" => {
            payload_string(event, "cmd").map(|cmd| format!("run {}", truncate_text(cmd.trim(), 96)))
        }
        "command.finished" => event
            .payload
            .get("success")
            .and_then(serde_json::Value::as_bool)
            .and_then(|success| {
                if success {
                    None
                } else {
                    let code = event
                        .payload
                        .get("exit_code")
                        .and_then(serde_json::Value::as_i64)
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    Some(format!("command failed with exit {code}"))
                }
            }),
        "model.turn.request" => {
            let model = payload_string(event, "model").unwrap_or_else(|| "model".to_string());
            Some(format!("waiting for {model}"))
        }
        "model.turn.retry" => Some("retrying model request".to_string()),
        "model.thinking_delta" | "model.stream_delta" => None,
        "model.tool_call" => {
            let name = event
                .payload
                .get("call")
                .and_then(|call| call.get("name"))
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| payload_string(event, "name"))
                .unwrap_or_else(|| "tool".to_string());
            Some(format!("{name} requested"))
        }
        "tool.started" => {
            let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
            Some(format!("{name} started"))
        }
        "tool.output" => payload_string(event, "text")
            .map(|text| truncate_text(text.trim(), 120))
            .filter(|text| !text.is_empty()),
        "tool.failed" => {
            let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
            let error = payload_string(event, "error").unwrap_or_else(|| "tool failed".to_string());
            Some(format!("{name} failed: {}", truncate_text(&error, 96)))
        }
        "plan.updated" => Some("updated plan".to_string()),
        _ => None,
    }
}

fn truncate_text(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    if max <= 3 {
        return value.chars().take(max).collect();
    }
    let mut out = value.chars().take(max - 3).collect::<String>();
    out.push_str("...");
    out
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_lines_keep_link_style_after_wrapping() {
        let node = TranscriptNode {
            id: "test".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Timeline {
                group: "link".to_string(),
                lines: vec!["https://example.com/some/very/long/path".to_string()],
                style: NodeStyle::Normal,
            },
        };
        let lines = node.display_lines(20, DisplayMode::Scrollback);
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("https://") && span.style == link())
        }));
    }
}
