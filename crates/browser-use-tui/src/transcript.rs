use browser_use_protocol::{normalize_result_text, EventRecord, SessionMeta, WorkbenchState};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthChar;

use crate::markdown::render_markdown_lines;
use crate::theme::{
    activity_group, activity_list, activity_read, activity_run, activity_search, activity_task,
    dim, failed, link, muted, path_reference, text_style, thought, user_prompt_accent,
    user_prompt_muted, user_prompt_text,
};

use super::App;

const GROUP_VALUE_RAIL_PREFIX: &str = "  │ ";
const GROUP_VALUE_LAST_PREFIX: &str = "  └ ";
const ACTIVE_LIVE_LINE_LIMIT: usize = 48;
const ACTIVE_FALLBACK_STATUS: &str = "running browser task";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DisplayMode {
    Scrollback,
    Active,
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptModel {
    pub(crate) session_id: String,
    pub(crate) committed: Vec<TranscriptNode>,
    terminal_committed: Vec<TranscriptNode>,
    pub(crate) active: Option<TranscriptNode>,
    pub(crate) last_event_seq: i64,
    pub(crate) revision: u64,
}

pub(crate) struct TerminalScrollbackEmission {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) last_seq: i64,
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptNode {
    id: String,
    seq: i64,
    revision: u64,
    kind: TranscriptKind,
}

#[derive(Clone, Debug)]
enum TranscriptKind {
    Stack {
        nodes: Vec<TranscriptNode>,
    },
    Prompt {
        text: String,
        followup: bool,
    },
    PendingStatus {
        status: String,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeStyle {
    Normal,
    Muted,
    Failed,
    Thought,
}

impl TranscriptNode {
    fn id(&self) -> &str {
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
            TranscriptKind::Stack { nodes } => cells_to_lines(nodes.iter(), width, mode),
            TranscriptKind::Prompt { text, followup } => prompt_lines(text, *followup, width),
            TranscriptKind::PendingStatus { status } => pending_status_lines(status),
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
            TranscriptKind::Stack { nodes } => {
                nodes.iter().flat_map(|node| node.plain_lines()).collect()
            }
            TranscriptKind::Prompt { text, .. } => prefixed_plain("> ", text),
            TranscriptKind::PendingStatus { status } => vec![status.clone()],
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
                let mut out = vec![format!("• {group}")];
                let last_idx = lines.len().saturating_sub(1);
                out.extend(lines.iter().enumerate().map(|(idx, line)| {
                    let prefix = if idx == last_idx {
                        GROUP_VALUE_LAST_PREFIX
                    } else {
                        GROUP_VALUE_RAIL_PREFIX
                    };
                    format!("{prefix}{line}")
                }));
                out
            }
            TranscriptKind::Error { text } => {
                vec![
                    "• error".to_string(),
                    format!("{GROUP_VALUE_LAST_PREFIX}{}", friendly_error_message(text)),
                ]
            }
            TranscriptKind::Cancelled { text } => {
                vec![
                    "• stopped".to_string(),
                    format!("{GROUP_VALUE_LAST_PREFIX}{text}"),
                ]
            }
        }
    }

    fn is_terminal_scrollback_transient(&self) -> bool {
        matches!(
            &self.kind,
            TranscriptKind::Timeline { group, style, .. }
                if group == "thinking"
                    || (*style == NodeStyle::Thought && group.starts_with("thought"))
        )
    }

    fn is_active_viewport_placeholder(&self) -> bool {
        match &self.kind {
            TranscriptKind::ActiveStatus {
                group,
                lines,
                style,
            } => {
                group == "status"
                    && *style == NodeStyle::Muted
                    && lines.len() == 1
                    && lines[0] == ACTIVE_FALLBACK_STATUS
            }
            TranscriptKind::Stack { nodes } => nodes
                .iter()
                .all(TranscriptNode::is_active_viewport_placeholder),
            _ => false,
        }
    }

    fn is_pending_followup_indicator(&self) -> bool {
        matches!(self.kind, TranscriptKind::PendingStatus { .. })
    }

    fn is_prompt(&self) -> bool {
        matches!(self.kind, TranscriptKind::Prompt { .. })
    }

    fn display_lines_with_stream_skip(
        &self,
        width: u16,
        mode: DisplayMode,
        stream_skip_lines: &mut usize,
    ) -> Vec<Line<'static>> {
        let width = width.max(1);
        match &self.kind {
            TranscriptKind::Stack { nodes } => {
                cells_to_lines_with_stream_skip(nodes.iter(), width, mode, stream_skip_lines)
            }
            TranscriptKind::StreamingAssistant { markdown } => {
                let mut lines = markdown_cell_lines(markdown, width, mode);
                let skip = (*stream_skip_lines).min(lines.len().saturating_sub(1));
                *stream_skip_lines = (*stream_skip_lines).saturating_sub(skip);
                if skip > 0 {
                    lines = lines.into_iter().skip(skip).collect();
                }
                lines
            }
            _ => self.display_lines(width, mode),
        }
    }

    fn streaming_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let width = width.max(1);
        match &self.kind {
            TranscriptKind::Stack { nodes } => nodes
                .iter()
                .flat_map(|node| node.streaming_display_lines(width))
                .collect(),
            TranscriptKind::StreamingAssistant { markdown } => {
                markdown_cell_lines(markdown, width, DisplayMode::Active)
            }
            _ => Vec::new(),
        }
    }
}

pub(crate) fn transcript_model(app: &App, state: &WorkbenchState) -> Option<TranscriptModel> {
    let session = state.current_session.as_ref()?;
    let events = app.cached_events_for_session(&session.id);
    let last_event_seq = events.last().map(|event| event.seq).unwrap_or_default();
    let mut committed = Vec::new();
    let mut terminal_committed = Vec::new();

    for event in events {
        if let Some(node) = committed_node_for_event(app, state, session, event) {
            terminal_committed.push(node.clone());
            push_committed_node(&mut committed, node);
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
        terminal_committed,
        active,
        last_event_seq,
        revision,
    })
}

pub(crate) fn all_scrollback_lines(model: &TranscriptModel, width: u16) -> Vec<Line<'static>> {
    cells_to_lines(model.committed.iter(), width, DisplayMode::Scrollback)
}

pub(crate) fn all_terminal_scrollback_lines(
    model: &TranscriptModel,
    width: u16,
) -> Vec<Line<'static>> {
    cells_to_lines(
        model
            .committed
            .iter()
            .filter(|node| !node.is_terminal_scrollback_transient()),
        width,
        DisplayMode::Scrollback,
    )
}

pub(crate) fn terminal_scrollback_emission_since(
    model: &TranscriptModel,
    after_seq: i64,
    width: u16,
    defer_open_tail: bool,
) -> TerminalScrollbackEmission {
    let mut nodes = Vec::new();
    let mut last_seq = after_seq;
    for node in model
        .terminal_committed
        .iter()
        .filter(|node| node.seq() > after_seq)
        .filter(|node| !node.is_terminal_scrollback_transient())
    {
        last_seq = node.seq();
        push_committed_node(&mut nodes, node.clone());
    }
    if defer_open_tail && nodes.last().is_some_and(is_open_timeline_node) {
        nodes.pop();
        last_seq = nodes.last().map(TranscriptNode::seq).unwrap_or(after_seq);
    }
    TerminalScrollbackEmission {
        lines: cells_to_lines(nodes.iter(), width, DisplayMode::Scrollback),
        last_seq,
    }
}

pub(crate) fn active_viewport_lines(
    model: Option<&TranscriptModel>,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let Some(active) = model.and_then(|model| model.active.as_ref()) else {
        return Vec::new();
    };
    if active.is_active_viewport_placeholder() {
        return Vec::new();
    }
    let mut lines = active.display_lines(width, DisplayMode::Active);
    if lines.len() > height as usize {
        let start = lines.len().saturating_sub(height as usize);
        lines = lines.into_iter().skip(start).collect();
    }
    lines
}

pub(crate) fn active_viewport_lines_with_stream_skip(
    model: Option<&TranscriptModel>,
    width: u16,
    height: u16,
    stream_skip_lines: usize,
) -> Vec<Line<'static>> {
    let Some(active) = model.and_then(|model| model.active.as_ref()) else {
        return Vec::new();
    };
    if active.is_active_viewport_placeholder() {
        return Vec::new();
    }
    let mut skip = stream_skip_lines;
    let mut lines = active.display_lines_with_stream_skip(width, DisplayMode::Active, &mut skip);
    if lines.len() > height as usize {
        let start = lines.len().saturating_sub(height as usize);
        lines = lines.into_iter().skip(start).collect();
    }
    lines
}

pub(crate) fn active_streaming_lines(
    model: Option<&TranscriptModel>,
    width: u16,
) -> Vec<Line<'static>> {
    model
        .and_then(|model| model.active.as_ref())
        .map(|active| active.streaming_display_lines(width))
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn active_viewport_has_live_content(model: Option<&TranscriptModel>) -> bool {
    model
        .and_then(|model| model.active.as_ref())
        .is_some_and(|active| !active.is_active_viewport_placeholder())
}

pub(crate) fn has_pending_followup_indicator(model: Option<&TranscriptModel>) -> bool {
    model
        .and_then(|model| model.active.as_ref())
        .is_some_and(TranscriptNode::is_pending_followup_indicator)
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
    let mut previous_kind = None;
    for node in nodes {
        let _ = (node.id(), node.revision());
        if !out.is_empty() {
            let gap = previous_kind
                .map(|previous| gap_lines_between(previous, &node.kind))
                .unwrap_or(0);
            out.extend(std::iter::repeat_with(|| Line::from("")).take(gap));
        }
        out.extend(node.display_lines(width, mode));
        previous_kind = Some(&node.kind);
    }
    out
}

fn cells_to_lines_with_stream_skip<'a>(
    nodes: impl Iterator<Item = &'a TranscriptNode>,
    width: u16,
    mode: DisplayMode,
    stream_skip_lines: &mut usize,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut previous_kind = None;
    for node in nodes {
        let _ = (node.id(), node.revision());
        if !out.is_empty() {
            let gap = previous_kind
                .map(|previous| gap_lines_between(previous, &node.kind))
                .unwrap_or(0);
            out.extend(std::iter::repeat_with(|| Line::from("")).take(gap));
        }
        out.extend(node.display_lines_with_stream_skip(width, mode, stream_skip_lines));
        previous_kind = Some(&node.kind);
    }
    out
}

pub(crate) fn gap_before_active(model: &TranscriptModel) -> usize {
    let Some(previous) = model.committed.last() else {
        return 0;
    };
    let Some(active) = model.active.as_ref() else {
        return 0;
    };
    gap_lines_between(&previous.kind, &active.kind)
}

fn gap_lines_between(previous: &TranscriptKind, next: &TranscriptKind) -> usize {
    match (previous, next) {
        (_, TranscriptKind::Prompt { .. } | TranscriptKind::PendingStatus { .. }) => 1,
        (
            TranscriptKind::Prompt { .. } | TranscriptKind::PendingStatus { .. },
            TranscriptKind::Assistant { .. }
            | TranscriptKind::StreamingAssistant { .. }
            | TranscriptKind::Timeline { .. }
            | TranscriptKind::ActiveStatus { .. }
            | TranscriptKind::Error { .. }
            | TranscriptKind::Cancelled { .. }
            | TranscriptKind::Stack { .. },
        ) => 2,
        _ => 1,
    }
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
            let result = session_done_result_text(event, state)?;
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
        "agent.spawned" => Some(subagent_lifecycle_node(
            app,
            event,
            "started",
            NodeStyle::Normal,
        )),
        "agent.wait.started" | "agent.wait.finished" => None,
        "agent.completed" => Some(subagent_lifecycle_node(
            app,
            event,
            "finished",
            NodeStyle::Normal,
        )),
        "agent.failed" => Some(subagent_lifecycle_node(
            app,
            event,
            "failed",
            NodeStyle::Failed,
        )),
        "agent.cancelled" => Some(subagent_lifecycle_node(
            app,
            event,
            "stopped",
            NodeStyle::Muted,
        )),
        "model.tool_call" | "tool.started" | "tool.finished" => None,
        "tool.batch_started" | "tool.batch_result" | "tool.batch_finished" => None,
        "tool.output" => tool_output_node(event),
        "tool.image" => Some(timeline_node(
            event,
            "image",
            vec![tool_image_label(event, state)],
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
            let item = event
                .payload
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .map(|count| format!("list {path} ({count} items)"))
                .unwrap_or_else(|| format!("list {path}"));
            Some(timeline_node(
                event,
                "explored",
                vec![item],
                NodeStyle::Normal,
            ))
        }
        "file.read" => {
            let path = payload_string(event, "path").map(|path| display_path(&path, state))?;
            Some(timeline_node(
                event,
                "explored",
                vec![format!("read {path}")],
                NodeStyle::Normal,
            ))
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
                "explored",
                vec![format!("search {query:?} ({matches} matches)")],
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
        "patch.started" | "patch.finished" => None,
        "artifact.created" => artifact_created_node(event, state),
        "browser.connected" | "browser.reconnected" | "browser.target_changed" => {
            Some(timeline_node(
                event,
                "browser",
                vec![browser_event_label(event)],
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
        "browser.open_requested" | "browser.reconnect_requested" | "browser.cloud_shutdown" => None,
        "browser.cloud_shutdown_failed" => Some(timeline_node(
            event,
            "error",
            vec![payload_string(event, "error")
                .unwrap_or_else(|| "browser shutdown failed".to_string())],
            NodeStyle::Failed,
        )),
        "plan.updated" => Some(timeline_node(
            event,
            "plan",
            vec!["updated plan".to_string()],
            NodeStyle::Normal,
        )),
        "session.deadline_warning" => Some(timeline_node(
            event,
            "warning",
            vec!["turn budget is nearly exhausted".to_string()],
            NodeStyle::Muted,
        )),
        "session.final_answer_not_ready_at_max_turns" => Some(timeline_node(
            event,
            "error",
            vec![payload_string(event, "error")
                .unwrap_or_else(|| "final answer artifact is not ready".to_string())],
            NodeStyle::Failed,
        )),
        "model.turn.context_overflow" => Some(timeline_node(
            event,
            "context",
            vec!["provider context overflow; compacting history".to_string()],
            NodeStyle::Muted,
        )),
        "session.compaction_failed" => Some(timeline_node(
            event,
            "error",
            vec![payload_string(event, "error").unwrap_or_else(|| "compaction failed".to_string())],
            NodeStyle::Failed,
        )),
        "model.turn.error" => Some(timeline_node(
            event,
            "error",
            vec!["model request hit an error".to_string()],
            NodeStyle::Failed,
        )),
        "command.write_error" => Some(timeline_node(
            event,
            "error",
            vec![payload_string(event, "error")
                .unwrap_or_else(|| "failed to write to command".to_string())],
            NodeStyle::Failed,
        )),
        "model.turn.request"
        | "model.turn.response"
        | "model.thinking_delta"
        | "model.turn.retry"
        | "model.stream_delta"
        | "model.delta"
        | "model.config"
        | "model.usage"
        | "session.compaction_started"
        | "session.compacted"
        | "session.created"
        | "session.status"
        | "session.final_answer_ready"
        | "session.final_answer_used"
        | "session.cancel_requested"
        | "agent.context"
        | "agent.updated"
        | "agent.message"
        | "telemetry.trace"
        | "telemetry.failed"
        | "command.cleaned_up" => None,
        _ => None,
    }
}

fn push_committed_node(committed: &mut Vec<TranscriptNode>, node: TranscriptNode) {
    if let Some(last) = committed.last_mut() {
        if merge_timeline_node(last, &node) {
            return;
        }
    }
    committed.push(node);
}

fn merge_timeline_node(last: &mut TranscriptNode, next: &TranscriptNode) -> bool {
    match (&mut last.kind, &next.kind) {
        (
            TranscriptKind::Timeline {
                group,
                lines,
                style,
            },
            TranscriptKind::Timeline {
                group: next_group,
                lines: next_lines,
                style: next_style,
            },
        ) if group == next_group && style == next_style => {
            if *style == NodeStyle::Thought {
                *lines = next_lines.clone();
            } else {
                lines.extend(next_lines.clone());
                compact_repeated_read_lines(lines);
            }
            last.id = next.id.clone();
            last.seq = next.seq;
            last.revision = next.revision;
            true
        }
        _ => false,
    }
}

fn compact_repeated_read_lines(lines: &mut Vec<String>) {
    let mut compacted = Vec::with_capacity(lines.len());
    let mut reads = Vec::new();

    for line in lines.drain(..) {
        if let Some(path) = read_line_path(&line) {
            reads.push(path.to_string());
        } else {
            flush_read_lines(&mut compacted, &mut reads);
            compacted.push(line);
        }
    }
    flush_read_lines(&mut compacted, &mut reads);

    *lines = compacted;
}

fn read_line_path(line: &str) -> Option<&str> {
    line.strip_prefix("read ")
        .map(str::trim)
        .filter(|path| !path.is_empty())
}

fn flush_read_lines(out: &mut Vec<String>, reads: &mut Vec<String>) {
    match reads.len() {
        0 => {}
        1 => out.push(format!("read {}", reads[0])),
        _ => out.push(format!("read {}", reads.join(", "))),
    }
    reads.clear();
}

fn thinking_delta_label(event: &EventRecord) -> Option<&str> {
    event
        .payload
        .get("label")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty())
}

fn latest_thinking_label(events: &[EventRecord]) -> Option<&str> {
    events
        .iter()
        .rev()
        .find(|event| event.event_type == "model.thinking_delta")
        .and_then(thinking_delta_label)
}

fn model_response_tool_call_count(event: &EventRecord) -> u64 {
    event
        .payload
        .get("tool_call_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn active_node_for_session(
    app: &App,
    state: &WorkbenchState,
    root: &SessionMeta,
    events: &[EventRecord],
) -> Option<TranscriptNode> {
    let live_events = current_turn_events(events);

    if let Some(pending_followup) = pending_followup_active_node(app, state, root, events) {
        return Some(pending_followup);
    }

    if has_active_child_session(app, &root.id) {
        return Some(active_status_node(
            root,
            events,
            "status",
            vec![ACTIVE_FALLBACK_STATUS.to_string()],
            NodeStyle::Muted,
        ));
    }

    let live_thinking_text = state
        .transcript
        .last()
        .and_then(|turn| turn.thinking_text.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let live_streaming_text = state
        .transcript
        .last()
        .and_then(|turn| turn.streaming_text.as_deref())
        .map(str::trim_end)
        .filter(|text| !text.is_empty())
        .filter(|_| !live_stream_has_committed_successor(live_events));

    let mut active_nodes = Vec::new();

    let suppress_model_wait = live_streaming_text.is_some() && live_thinking_text.is_none();
    if let Some(event) = live_events.iter().rev().find(|event| {
        matches!(event.event_type.as_str(), "model.turn.retry")
            && !(suppress_model_wait && matches!(event.event_type.as_str(), "model.turn.retry"))
    }) {
        if let Some(node) = active_node_for_event(root, events, event) {
            active_nodes.push(node);
        }
    }

    if let Some(text) = live_thinking_text {
        let label = latest_thinking_label(events);
        active_nodes.push(active_status_node(
            root,
            events,
            &label
                .map(|label| format!("thought {label}"))
                .unwrap_or_else(|| "thought".to_string()),
            preview_lines(text, ACTIVE_LIVE_LINE_LIMIT),
            NodeStyle::Thought,
        ));
    }

    if let Some(text) = live_streaming_text {
        active_nodes.push(TranscriptNode {
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

    if app.native_scrollback_is_active() && live_streaming_text.is_none() {
        if let Some(node) = active_timeline_tail_node(app, state, root, live_events) {
            active_nodes.push(node);
        }
    }

    if !active_nodes.is_empty() {
        let seq = events.last().map(|event| event.seq).unwrap_or_default();
        return Some(TranscriptNode {
            id: format!("{}:active-stack", root.id),
            seq,
            revision: seq.max(0) as u64,
            kind: TranscriptKind::Stack {
                nodes: active_nodes,
            },
        });
    }

    if let Some(event) = live_events.iter().rev().find(|event| {
        matches!(
            event.event_type.as_str(),
            "command.waiting" | "tool.started" | "browser.page" | "browser.state" | "plan.updated"
        )
    }) {
        return active_node_for_event(root, events, event);
    }

    Some(active_status_node(
        root,
        events,
        "status",
        vec![ACTIVE_FALLBACK_STATUS.to_string()],
        NodeStyle::Muted,
    ))
}

fn active_timeline_tail_node(
    app: &App,
    state: &WorkbenchState,
    root: &SessionMeta,
    live_events: &[EventRecord],
) -> Option<TranscriptNode> {
    let nodes = live_events
        .iter()
        .filter_map(|event| committed_node_for_event(app, state, root, event))
        .filter(|node| !node.is_terminal_scrollback_transient())
        .collect::<Vec<_>>();
    let last = nodes.last()?;
    let key = timeline_merge_key(last)?;
    if !is_open_timeline_node(last) {
        return None;
    }

    let mut start = nodes.len().saturating_sub(1);
    while start > 0 && timeline_merge_key(&nodes[start - 1]) == Some(key) {
        start -= 1;
    }

    let mut tail = Vec::new();
    for node in nodes[start..].iter().cloned() {
        push_committed_node(&mut tail, node);
    }
    tail.into_iter().next()
}

fn is_open_timeline_node(node: &TranscriptNode) -> bool {
    matches!(
        &node.kind,
        TranscriptKind::Timeline { style, .. } if *style != NodeStyle::Failed
    )
}

fn timeline_merge_key(node: &TranscriptNode) -> Option<(&str, NodeStyle)> {
    match &node.kind {
        TranscriptKind::Timeline { group, style, .. } => Some((group.as_str(), *style)),
        _ => None,
    }
}

fn pending_followup_active_node(
    app: &App,
    state: &WorkbenchState,
    root: &SessionMeta,
    events: &[EventRecord],
) -> Option<TranscriptNode> {
    let latest_followup = events
        .iter()
        .rev()
        .find(|event| event.session_id == root.id && event.event_type == "session.followup")?;
    let has_prior_scrollback = events
        .iter()
        .filter(|event| event.seq < latest_followup.seq)
        .filter_map(|event| committed_node_for_event(app, state, root, event))
        .any(|node| !node.is_terminal_scrollback_transient());
    if !has_prior_scrollback {
        return None;
    }
    let has_committed_output_after = events
        .iter()
        .filter(|event| event.seq > latest_followup.seq)
        .filter_map(|event| committed_node_for_event(app, state, root, event))
        .filter(|node| !node.is_terminal_scrollback_transient())
        .any(|node| !node.is_prompt());
    if has_committed_output_after {
        return None;
    }
    let has_live_output_after = events
        .iter()
        .filter(|event| event.seq > latest_followup.seq)
        .any(is_live_output_event);
    if has_live_output_after {
        return None;
    }
    let status = pending_followup_status(app, events, latest_followup.seq);
    Some(TranscriptNode {
        id: format!("{}:active-followup:{}", root.id, latest_followup.seq),
        seq: latest_followup.seq,
        revision: latest_followup.seq.max(0) as u64,
        kind: TranscriptKind::PendingStatus { status },
    })
}

fn live_stream_has_committed_successor(live_events: &[EventRecord]) -> bool {
    let segment_start = live_events
        .iter()
        .rposition(|event| {
            matches!(
                event.event_type.as_str(),
                "model.turn.request" | "model.turn.retry" | "model.turn.error"
            )
        })
        .map(|idx| idx.saturating_add(1))
        .unwrap_or(0);
    let segment = live_events.get(segment_start..).unwrap_or_default();
    let Some(latest_stream_seq) = segment
        .iter()
        .rev()
        .find(|event| {
            event.event_type == "model.stream_delta"
                && event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
        })
        .map(|event| event.seq)
    else {
        return false;
    };
    segment.iter().any(|event| {
        event.seq > latest_stream_seq
            && (matches!(
                event.event_type.as_str(),
                "session.done" | "session.failed" | "session.cancelled"
            ) || (event.event_type == "model.turn.response"
                && model_response_tool_call_count(event) > 0))
    })
}

fn is_live_output_event(event: &EventRecord) -> bool {
    match event.event_type.as_str() {
        "model.stream_delta" | "model.thinking_delta" => event
            .payload
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| !text.trim().is_empty()),
        "command.waiting" | "tool.started" | "browser.page" | "browser.state" | "plan.updated" => {
            true
        }
        _ => false,
    }
}

fn pending_followup_status(app: &App, events: &[EventRecord], after_seq: i64) -> String {
    let label = events
        .iter()
        .filter(|event| event.seq > after_seq)
        .rev()
        .find_map(|event| match event.event_type.as_str() {
            "model.turn.request" => Some("thinking".to_string()),
            "model.turn.retry" => Some("retrying model request".to_string()),
            "command.waiting" => Some("running command".to_string()),
            "tool.started" => payload_string(event, "name")
                .map(|name| format!("running {name}"))
                .or_else(|| Some("running tool".to_string())),
            _ => None,
        })
        .unwrap_or_else(|| "sending".to_string());
    format!("{} {label}", live_spinner_frame(app.live_spinner_frame))
}

fn live_spinner_frame(frame: usize) -> &'static str {
    const FRAMES: [&str; 4] = ["-", "\\", "|", "/"];
    FRAMES[frame % FRAMES.len()]
}

fn current_turn_events(events: &[EventRecord]) -> &[EventRecord] {
    let start = events
        .iter()
        .rposition(|event| {
            matches!(
                event.event_type.as_str(),
                "session.input" | "session.followup"
            )
        })
        .map(|idx| idx.saturating_add(1))
        .unwrap_or(0);
    events.get(start..).unwrap_or_default()
}

fn active_node_for_event(
    root: &SessionMeta,
    events: &[EventRecord],
    event: &EventRecord,
) -> Option<TranscriptNode> {
    match event.event_type.as_str() {
        "model.turn.request" => None,
        "model.turn.retry" => Some(active_status_node(
            root,
            events,
            "thinking",
            vec!["retrying model request".to_string()],
            NodeStyle::Muted,
        )),
        "agent.wait.started" => None,
        "command.waiting" => Some(active_status_node(
            root,
            events,
            "run",
            vec!["command still running".to_string()],
            NodeStyle::Muted,
        )),
        "tool.started" => {
            let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
            active_tool_status(&name).map(|(group, line)| {
                active_status_node(
                    root,
                    events,
                    group,
                    vec![line.to_string()],
                    NodeStyle::Muted,
                )
            })
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

fn tool_output_node(event: &EventRecord) -> Option<TranscriptNode> {
    let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
    if is_subagent_management_tool(&name) {
        return None;
    }
    let mut lines = Vec::new();
    if should_show_generic_tool_output_text(&name) {
        if let Some(text) = payload_string(event, "text").filter(|text| !text.trim().is_empty()) {
            lines.extend(preview_lines(&text, 3));
        }
    }
    if event
        .payload
        .get("text_truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        if let Some(path) = event
            .payload
            .get("text_artifact")
            .and_then(|artifact| artifact.get("path"))
            .and_then(serde_json::Value::as_str)
        {
            lines.push(format!("full output saved to {path}"));
        }
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
    Some(timeline_node(
        event,
        tool_output_group(&name),
        lines,
        NodeStyle::Muted,
    ))
}

fn artifact_created_node(event: &EventRecord, state: &WorkbenchState) -> Option<TranscriptNode> {
    let artifact = event.payload.get("artifact")?;
    let path = artifact
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(|path| display_path(path, state))?;
    let kind = artifact
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("artifact");
    Some(timeline_node(
        event,
        "artifact",
        vec![format!("{kind} {path}")],
        NodeStyle::Normal,
    ))
}

fn active_tool_status(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "python" => Some(("python", "running browser Python")),
        "exec_command" => Some(("run", "running command")),
        "write_stdin" => Some(("run", "writing to command")),
        "apply_patch" => Some(("edit", "applying patch")),
        "view_image" => Some(("image", "inspecting image")),
        "update_plan" => Some(("plan", "updating plan")),
        _ => None,
    }
}

fn should_show_generic_tool_output_text(name: &str) -> bool {
    !is_known_tool_with_domain_events(name)
}

fn tool_output_group(name: &str) -> &str {
    if name == "python" {
        "python"
    } else {
        "tool"
    }
}

fn is_known_tool_with_domain_events(name: &str) -> bool {
    matches!(
        name,
        "done"
            | "python"
            | "exec_command"
            | "write_stdin"
            | "apply_patch"
            | "read_file"
            | "search_files"
            | "list_files"
            | "view_image"
            | "update_plan"
            | "spawn_agent"
            | "wait_agent"
            | "send_input"
            | "send_message"
            | "followup_task"
            | "list_agents"
            | "close_agent"
            | "resume_agent"
    )
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

fn prompt_lines(text: &str, followup: bool, width: u16) -> Vec<Line<'static>> {
    prompt_lines_with_status(text, followup, width, None)
}

fn pending_status_lines(status: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled("  ".to_string(), user_prompt_accent()),
        Span::styled(status.to_string(), user_prompt_muted()),
    ])]
}

fn prompt_lines_with_status(
    text: &str,
    _followup: bool,
    width: u16,
    status: Option<&str>,
) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(2).max(1) as usize;
    // Pad the content to the full width so the highlight background reads as a
    // solid block rather than only wrapping the glyphs.
    let pad_to_width = |value: &str| -> String {
        let used = display_width(value);
        let mut out = value.to_string();
        out.extend(std::iter::repeat(' ').take(content_width.saturating_sub(used)));
        out
    };
    let mut rows = Vec::new();
    for (idx, source) in text.lines().enumerate() {
        let prefix = if idx == 0 { "> " } else { "  " };
        for (wrap_idx, wrapped) in wrap_plain(source, content_width as u16) {
            let visible_prefix = if wrap_idx == 0 { prefix } else { "  " };
            rows.push((visible_prefix.to_string(), wrapped));
        }
    }
    if rows.is_empty() {
        rows.push(("> ".to_string(), String::new()));
    }
    let last_idx = rows.len().saturating_sub(1);
    rows.into_iter()
        .enumerate()
        .map(|(idx, (prefix, wrapped))| {
            let mut spans = vec![Span::styled(prefix, user_prompt_accent())];
            let can_fit_status = status.is_some_and(|status| {
                let status_width = display_width(status).saturating_add(2);
                display_width(&wrapped).saturating_add(status_width) <= content_width
            });
            if idx == last_idx && can_fit_status {
                let status = status.unwrap_or_default();
                let content_used = display_width(&wrapped);
                let status_gap = 2usize;
                let status_width = display_width(status);
                let tail_gap =
                    content_width.saturating_sub(content_used + status_gap + status_width);
                spans.push(Span::styled(wrapped, user_prompt_text()));
                spans.push(Span::styled(" ".repeat(status_gap), user_prompt_text()));
                spans.push(Span::styled(status.to_string(), user_prompt_muted()));
                spans.push(Span::styled(" ".repeat(tail_gap), user_prompt_text()));
            } else {
                spans.push(Span::styled(pad_to_width(&wrapped), user_prompt_text()));
            }
            Line::from(spans)
        })
        .collect()
}

fn display_width(value: &str) -> usize {
    value.chars().map(|ch| ch.width().unwrap_or(0).max(1)).sum()
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
        Span::styled("• ", dim()),
        Span::styled(group.to_string(), group_label_style(group, style)),
    ]));
    let value_style = body_style(style);
    let prefix_width = display_width(GROUP_VALUE_LAST_PREFIX) as u16;
    let content_width = width.saturating_sub(prefix_width).max(1);
    let value_rows = values
        .iter()
        .flat_map(|value| {
            wrap_plain(value, content_width)
                .into_iter()
                .map(|(_, row)| row)
        })
        .collect::<Vec<_>>();
    let last_idx = value_rows.len().saturating_sub(1);
    for (idx, wrapped) in value_rows.into_iter().enumerate() {
        let prefix = if idx == last_idx {
            GROUP_VALUE_LAST_PREFIX
        } else {
            GROUP_VALUE_RAIL_PREFIX
        };
        let mut spans = vec![Span::styled(prefix.to_string(), dim())];
        spans.extend(styled_value_spans(group, &wrapped, value_style));
        lines.push(Line::from(spans));
    }
    lines
}

fn styled_value_spans(_group: &str, text: &str, fallback: Style) -> Vec<Span<'static>> {
    if text.starts_with("https://") || text.starts_with("http://") {
        return vec![Span::styled(text.to_string(), link())];
    }
    if let Some(spans) = styled_activity_line_spans(text, fallback) {
        return spans;
    }
    styled_path_tokens(text, fallback)
}

fn styled_activity_line_spans(text: &str, fallback: Style) -> Option<Vec<Span<'static>>> {
    let (leading, action, rest) = split_activity_line(text)?;
    if action == "run" && looks_like_command_line(rest) {
        let mut spans = Vec::new();
        if !leading.is_empty() {
            spans.push(Span::styled(leading.to_string(), fallback));
        }
        spans.push(Span::styled(
            action.to_string(),
            activity_action_style(action),
        ));
        spans.push(Span::styled(" ".to_string(), fallback));
        spans.extend(styled_path_tokens(rest, fallback));
        return Some(spans);
    }

    if matches!(
        action,
        "read"
            | "list"
            | "search"
            | "task"
            | "follow-up"
            | "waiting"
            | "working"
            | "artifact"
            | "command"
    ) {
        let mut spans = Vec::new();
        if !leading.is_empty() {
            spans.push(Span::styled(leading.to_string(), fallback));
        }
        spans.push(Span::styled(
            action.to_string(),
            activity_action_style(action),
        ));
        if !rest.is_empty() {
            spans.push(Span::styled(" ".to_string(), fallback));
            spans.extend(styled_path_tokens(rest, fallback));
        }
        return Some(spans);
    }

    None
}

fn split_activity_line(text: &str) -> Option<(&str, &str, &str)> {
    let leading_len = text
        .chars()
        .take_while(|ch| ch.is_whitespace())
        .map(char::len_utf8)
        .sum::<usize>();
    let leading = &text[..leading_len];
    let body = &text[leading_len..];
    if body.is_empty() {
        return None;
    }
    if body == "working" {
        return Some((leading, body, ""));
    }
    let (action, rest) = body.split_once(' ')?;
    Some((leading, action, rest))
}

fn activity_action_style(action: &str) -> Style {
    match action {
        "read" => activity_read(),
        "run" | "command" => activity_run(),
        "list" => activity_list(),
        "search" => activity_search(),
        "artifact" | "task" | "follow-up" => activity_task(),
        "working" | "waiting" => thought(),
        _ => group_style(NodeStyle::Normal),
    }
}

fn group_label_style(group: &str, style: NodeStyle) -> Style {
    match group.split_whitespace().next() {
        Some("subagent") => thought(),
        Some("run") => activity_run(),
        Some("explored") => activity_group(),
        Some("browser") => activity_search(),
        Some("edit") | Some("plan") | Some("context") => activity_task(),
        _ => group_style(style),
    }
}

fn styled_path_tokens(text: &str, fallback: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut token_start = None;
    for (idx, ch) in text.char_indices() {
        if ch.is_whitespace() {
            if let Some(start) = token_start.take() {
                push_maybe_path_token(&mut spans, &text[start..idx], fallback);
            }
            spans.push(Span::styled(ch.to_string(), fallback));
        } else if token_start.is_none() {
            token_start = Some(idx);
        }
    }
    if let Some(start) = token_start {
        push_maybe_path_token(&mut spans, &text[start..], fallback);
    }
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), fallback));
    }
    spans
}

fn push_maybe_path_token(spans: &mut Vec<Span<'static>>, token: &str, fallback: Style) {
    let leading = token
        .chars()
        .take_while(|ch| matches!(ch, '"' | '\'' | '`' | '(' | '[' | '{' | '<'))
        .map(char::len_utf8)
        .sum::<usize>();
    let trailing = token
        .chars()
        .rev()
        .take_while(|ch| {
            matches!(
                ch,
                '"' | '\'' | '`' | ')' | ']' | '}' | '>' | ',' | ':' | ';'
            )
        })
        .map(char::len_utf8)
        .sum::<usize>();
    let core_end = token.len().saturating_sub(trailing);
    if leading >= core_end {
        spans.push(Span::styled(token.to_string(), fallback));
        return;
    }
    let (prefix, rest) = token.split_at(leading);
    let (core, suffix) = rest.split_at(core_end - leading);
    if looks_like_path_token(core) {
        if !prefix.is_empty() {
            spans.push(Span::styled(prefix.to_string(), fallback));
        }
        spans.push(Span::styled(core.to_string(), reference_token_style(core)));
        if !suffix.is_empty() {
            spans.push(Span::styled(suffix.to_string(), fallback));
        }
    } else {
        spans.push(Span::styled(token.to_string(), fallback));
    }
}

fn looks_like_command_line(text: &str) -> bool {
    matches!(
        text.trim_start()
            .trim_start_matches("$ ")
            .split_whitespace()
            .next(),
        Some(
            "cargo"
                | "git"
                | "rg"
                | "grep"
                | "find"
                | "sed"
                | "awk"
                | "cat"
                | "ls"
                | "cd"
                | "pwd"
                | "uv"
                | "python"
                | "python3"
                | "node"
                | "npm"
                | "pnpm"
                | "yarn"
                | "bun"
                | "curl"
                | "ssh"
                | "docker"
                | "task"
                | "sqlite3"
        )
    )
}

fn looks_like_path_token(token: &str) -> bool {
    if looks_like_url_token(token) {
        return true;
    }
    let has_path_character = token
        .chars()
        .any(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    token.starts_with('/')
        || (has_path_character
            && (token.starts_with("~/") || token.starts_with("./") || token.starts_with("../")))
        || source_extension(token).is_some()
}

fn reference_token_style(token: &str) -> Style {
    if looks_like_url_token(token) {
        link()
    } else {
        path_reference()
    }
}

fn looks_like_url_token(token: &str) -> bool {
    token.starts_with("http://") || token.starts_with("https://")
}

fn source_extension(token: &str) -> Option<&str> {
    let extension = token.rsplit_once('.')?.1;
    matches!(
        extension,
        "rs" | "toml"
            | "lock"
            | "md"
            | "py"
            | "json"
            | "jsonl"
            | "yaml"
            | "yml"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "css"
            | "scss"
            | "html"
            | "sql"
            | "sh"
            | "zsh"
            | "fish"
            | "txt"
            | "log"
            | "xml"
            | "svg"
            | "diff"
            | "patch"
    )
    .then_some(extension)
}

fn group_style(style: NodeStyle) -> Style {
    match style {
        NodeStyle::Normal => activity_group(),
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

fn session_done_result_text(event: &EventRecord, state: &WorkbenchState) -> Option<String> {
    if event.payload.get("result_file").is_some() {
        return Some(session_done_result_file_text(event, state));
    }
    payload_string(event, "result").map(|result| normalize_result_text(&result))
}

fn session_done_result_file_text(event: &EventRecord, state: &WorkbenchState) -> String {
    let file_path = payload_string(event, "result_file_path")
        .or_else(|| resolved_result_file_path(event, state).map(|path| path.display().to_string()))
        .or_else(|| payload_string(event, "result_file"))
        .unwrap_or_else(|| "<unknown>".to_string());
    let directory_path = payload_string(event, "result_file_directory").or_else(|| {
        resolved_result_file_path(event, state)
            .and_then(|path| path.parent().map(|path| path.display().to_string()))
    });
    let bytes = event
        .payload
        .get("result_file_bytes")
        .and_then(serde_json::Value::as_u64);
    let mime = payload_string(event, "result_file_mime");

    let file_display = display_path(&file_path, state);
    let mut text = format!("Saved result file\n\nFile      {file_display}");
    if let Some(directory_path) = directory_path {
        text.push_str(&format!(
            "\nFolder    {}",
            display_path(&directory_path, state)
        ));
    }
    match (bytes, mime.as_deref()) {
        (Some(bytes), Some(mime)) => {
            text.push_str(&format!("\nSize      {} · {mime}", format_bytes(bytes)));
        }
        (Some(bytes), None) => {
            text.push_str(&format!("\nSize      {}", format_bytes(bytes)));
        }
        (None, Some(mime)) => {
            text.push_str(&format!("\nType      {mime}"));
        }
        (None, None) => {}
    }
    text.push_str("\n\nFull contents are saved on disk, not inlined into the terminal.");
    text
}

fn resolved_result_file_path(event: &EventRecord, state: &WorkbenchState) -> Option<PathBuf> {
    if let Some(path) = payload_string(event, "result_file_path") {
        return Some(PathBuf::from(path));
    }
    let requested = payload_string(event, "result_file")?;
    let requested_path = Path::new(&requested);
    if requested_path.is_absolute() {
        return Some(requested_path.to_path_buf());
    }
    let session = state.current_session.as_ref()?;
    let candidates = [
        Path::new(&session.cwd).join(&requested),
        Path::new(&session.artifact_root).join(&requested),
        requested_path.to_path_buf(),
    ];
    candidates
        .iter()
        .find(|candidate| candidate.exists())
        .cloned()
        .or_else(|| Some(Path::new(&session.artifact_root).join(&requested)))
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= GB {
        format!("{:.1} GB", bytes_f / GB)
    } else if bytes_f >= MB {
        format!("{:.1} MB", bytes_f / MB)
    } else if bytes_f >= KB {
        format!("{:.1} KB", bytes_f / KB)
    } else {
        format!("{bytes} B")
    }
}

fn compact_url(url: &str) -> String {
    const MAX: usize = 72;
    let compact = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))
        .unwrap_or_else(|| url.trim())
        .trim_end_matches('/');
    let compact = if let Some((prefix, _)) = compact.split_once('?') {
        format!("{prefix}?...")
    } else {
        compact.to_string()
    };
    if compact.chars().count() <= MAX {
        return compact;
    }
    let mut out = compact
        .chars()
        .take(MAX.saturating_sub(1))
        .collect::<String>();
    out.push_str("...");
    out
}

fn helper_label_for_child(app: &App, parent_id: &str, child_id: &str) -> String {
    app.cached_events_for_session(parent_id)
        .iter()
        .find(|event| {
            event.event_type == "agent.spawned"
                && event
                    .payload
                    .get("child_session_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(child_id)
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
        .unwrap_or_else(|| helper_label_for_session(app, child_id))
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

fn subagent_lifecycle_node(
    app: &App,
    event: &EventRecord,
    status: &str,
    style: NodeStyle,
) -> TranscriptNode {
    let group = subagent_label_for_event(app, event)
        .map(|label| format!("subagent {label} {status}"))
        .unwrap_or_else(|| format!("subagent {status}"));
    timeline_node(event, &group, Vec::new(), style)
}

fn subagent_label_for_event(app: &App, event: &EventRecord) -> Option<String> {
    if let Some(child_id) = event
        .payload
        .get("child_session_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("child_session_id"))
                .and_then(serde_json::Value::as_str)
        })
    {
        if let Some(label) =
            normalize_subagent_label(&helper_label_for_child(app, &event.session_id, child_id))
        {
            return Some(label);
        }
    }

    ["nickname", "role", "task_name", "agent_path"]
        .into_iter()
        .find_map(|key| {
            event
                .payload
                .get(key)
                .and_then(serde_json::Value::as_str)
                .and_then(normalize_subagent_label)
                .or_else(|| {
                    event
                        .payload
                        .get("payload")
                        .and_then(|payload| payload.get(key))
                        .and_then(serde_json::Value::as_str)
                        .and_then(normalize_subagent_label)
                })
        })
}

fn normalize_subagent_label(value: &str) -> Option<String> {
    let label = value
        .trim()
        .trim_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(value)
        .trim();
    (!label.is_empty() && label != "root" && label != "subagent").then(|| label.to_string())
}

fn has_active_child_session(app: &App, root_id: &str) -> bool {
    app.state_cache
        .sessions
        .iter()
        .any(|session| session.parent_id.as_deref() == Some(root_id) && session.status.is_active())
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn tool_image_label(event: &EventRecord, state: &WorkbenchState) -> String {
    event
        .payload
        .get("image")
        .and_then(|image| image.get("path"))
        .and_then(serde_json::Value::as_str)
        .map(|path| format!("image {}", display_path(path, state)))
        .unwrap_or_else(|| "received image artifact".to_string())
}

fn browser_event_label(event: &EventRecord) -> String {
    match event.event_type.as_str() {
        "browser.reconnected" => "browser reconnected",
        "browser.target_changed" => "browser target changed",
        _ => "browser connected",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn prompt_output_pairs_have_extra_vertical_space() {
        let prompt = TranscriptNode {
            id: "prompt".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Prompt {
                text: "go to gusto".to_string(),
                followup: false,
            },
        };
        let answer = TranscriptNode {
            id: "answer".to_string(),
            seq: 2,
            revision: 2,
            kind: TranscriptKind::Assistant {
                markdown: "Please open Chrome first.".to_string(),
                source: None,
            },
        };

        let lines = cells_to_lines([&prompt, &answer].into_iter(), 80, DisplayMode::Scrollback);

        assert_eq!(line_text(&lines[0]), "> go to gusto");
        assert_eq!(line_text(&lines[1]), "");
        assert_eq!(line_text(&lines[2]), "");
        assert_eq!(line_text(&lines[3]), "Please open Chrome first.");
    }

    #[test]
    fn followup_prompts_keep_a_gap_after_previous_output() {
        let answer = TranscriptNode {
            id: "answer".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Assistant {
                markdown: "First answer.".to_string(),
                source: None,
            },
        };
        let followup = TranscriptNode {
            id: "followup".to_string(),
            seq: 2,
            revision: 2,
            kind: TranscriptKind::Prompt {
                text: "which chrome profiles do i have".to_string(),
                followup: true,
            },
        };

        let lines = cells_to_lines(
            [&answer, &followup].into_iter(),
            80,
            DisplayMode::Scrollback,
        );

        assert_eq!(line_text(&lines[0]), "First answer.");
        assert_eq!(line_text(&lines[1]), "");
        assert_eq!(line_text(&lines[2]), "> which chrome profiles do i have");
    }

    #[test]
    fn merging_timeline_nodes_compacts_consecutive_reads() {
        let mut last = TranscriptNode {
            id: "first".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Timeline {
                group: "explored".to_string(),
                lines: vec!["read README.md".to_string()],
                style: NodeStyle::Normal,
            },
        };
        let next = TranscriptNode {
            id: "second".to_string(),
            seq: 2,
            revision: 2,
            kind: TranscriptKind::Timeline {
                group: "explored".to_string(),
                lines: vec![
                    "read Cargo.toml".to_string(),
                    "list . (10 items)".to_string(),
                    "read Taskfile.yml".to_string(),
                ],
                style: NodeStyle::Normal,
            },
        };

        assert!(merge_timeline_node(&mut last, &next));
        let TranscriptKind::Timeline { lines, .. } = &last.kind else {
            panic!("expected timeline node");
        };
        assert_eq!(
            lines,
            &[
                "read README.md, Cargo.toml".to_string(),
                "list . (10 items)".to_string(),
                "read Taskfile.yml".to_string(),
            ]
        );
    }

    #[test]
    fn terminal_scrollback_emits_only_new_timeline_delta() {
        let raw_nodes = vec![
            TranscriptNode {
                id: "first".to_string(),
                seq: 1,
                revision: 1,
                kind: TranscriptKind::Timeline {
                    group: "explored".to_string(),
                    lines: vec!["read README.md".to_string()],
                    style: NodeStyle::Normal,
                },
            },
            TranscriptNode {
                id: "second".to_string(),
                seq: 2,
                revision: 2,
                kind: TranscriptKind::Timeline {
                    group: "explored".to_string(),
                    lines: vec!["read Cargo.toml".to_string()],
                    style: NodeStyle::Normal,
                },
            },
            TranscriptNode {
                id: "third".to_string(),
                seq: 3,
                revision: 3,
                kind: TranscriptKind::Timeline {
                    group: "explored".to_string(),
                    lines: vec!["read Taskfile.yml".to_string()],
                    style: NodeStyle::Normal,
                },
            },
        ];
        let mut committed = Vec::new();
        for node in raw_nodes.clone() {
            push_committed_node(&mut committed, node);
        }
        let model = TranscriptModel {
            session_id: "session".to_string(),
            committed,
            terminal_committed: raw_nodes,
            active: None,
            last_event_seq: 3,
            revision: 3,
        };

        let full = terminal_scrollback_emission_since(&model, 0, 120, false);
        let full_text = full
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(full_text.contains("read README.md, Cargo.toml, Taskfile.yml"));

        let delta = terminal_scrollback_emission_since(&model, 1, 120, false);
        let delta_text = delta
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(delta_text.contains("read Cargo.toml, Taskfile.yml"));
        assert!(!delta_text.contains("README.md"), "{delta_text}");
    }

    #[test]
    fn grouped_timeline_values_are_visually_nested_under_header() {
        let node = TranscriptNode {
            id: "test".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Timeline {
                group: "explored".to_string(),
                lines: vec![
                    "read Taskfile.yml Cargo.toml README.md".to_string(),
                    "list . (200 items)".to_string(),
                ],
                style: NodeStyle::Normal,
            },
        };

        let lines = node.display_lines(24, DisplayMode::Scrollback);
        assert_eq!(line_text(&lines[0]), "• explored");
        assert!(line_text(&lines[1]).starts_with(GROUP_VALUE_RAIL_PREFIX));
        assert!(line_text(&lines[1]).contains("read"));
        assert!(line_text(&lines[2]).starts_with(GROUP_VALUE_RAIL_PREFIX));
        assert!(line_text(&lines[3]).starts_with(GROUP_VALUE_LAST_PREFIX));
        assert!(line_text(&lines[3]).contains("list"));
    }

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

    #[test]
    fn run_values_style_paths_without_command_syntax_highlighting() {
        let command_spans = styled_value_spans(
            "run",
            "find crates -maxdepth 3 -type f | sort",
            text_style(),
        );
        assert!(command_spans
            .iter()
            .any(|span| span.content.as_ref() == "find" && span.style == text_style()));

        let path_spans = styled_value_spans(
            "run",
            "crates/browser-use-tui/src/markdown.rs",
            text_style(),
        );
        assert!(path_spans
            .iter()
            .any(|span| span.content.contains("markdown.rs") && span.style == path_reference()));
        assert!(!path_spans
            .iter()
            .any(|span| span.content.contains("markdown.rs") && span.style == link()));
    }

    #[test]
    fn nested_activity_run_lines_style_action_but_not_command_syntax() {
        let spans = styled_value_spans(
            "subagent repo explorer",
            "run pwd && find . -maxdepth 2 -type f | sed 's# ./##' | sort | head -200",
            text_style(),
        );
        assert!(spans
            .iter()
            .any(|span| span.content.as_ref() == "run" && span.style == activity_run()));
        assert!(spans
            .iter()
            .any(|span| span.content.as_ref() == "find" && span.style == text_style()));
        assert!(!spans
            .iter()
            .any(|span| span.content.contains("./##") && span.style == link()));
        assert!(!spans
            .iter()
            .any(|span| span.content.contains("./##") && span.style == path_reference()));
    }

    #[test]
    fn prose_slash_tokens_do_not_become_paths() {
        let spans = styled_value_spans(
            "subagent repo explorer",
            "task Inspect the repo: languages/frameworks...",
            text_style(),
        );
        assert!(spans
            .iter()
            .any(|span| span.content.as_ref() == "task" && span.style == activity_task()));
        assert!(!spans
            .iter()
            .any(|span| span.content.contains("languages/frameworks") && span.style == link()));
        assert!(!spans
            .iter()
            .any(|span| span.content.contains("languages/frameworks")
                && span.style == path_reference()));
    }

    #[test]
    fn child_activity_state_words_are_highlighted() {
        for (line, expected_style) in [
            ("working", thought()),
            ("list .", activity_list()),
            ("read Taskfile.yml", activity_read()),
        ] {
            let spans = styled_value_spans("subagent repo explorer", line, text_style());
            let action = line.split_whitespace().next().unwrap_or(line);
            assert!(
                spans
                    .iter()
                    .any(|span| span.content.as_ref() == action && span.style == expected_style),
                "{line:?} did not highlight {action:?}"
            );
        }
    }

    #[test]
    fn activity_roles_use_distinct_styles() {
        let group_style = group_style(NodeStyle::Normal);
        for style in [
            activity_read(),
            activity_run(),
            activity_list(),
            activity_search(),
            activity_task(),
        ] {
            assert_ne!(group_style, style);
        }
        assert_ne!(activity_read(), activity_run());
        assert_ne!(activity_read(), activity_list());
        assert_ne!(activity_read(), activity_search());
        assert_ne!(activity_read(), activity_task());
        assert_ne!(activity_run(), activity_list());
        assert_ne!(activity_run(), activity_search());
        assert_ne!(activity_run(), activity_task());
        assert_ne!(activity_list(), activity_search());
        assert_ne!(activity_list(), activity_task());
        assert_ne!(activity_search(), activity_task());
    }

    #[test]
    fn timeline_group_labels_use_domain_styles() {
        assert_eq!(
            group_label_style("subagent repo_explorer started", NodeStyle::Normal),
            thought()
        );
        assert_eq!(group_label_style("run", NodeStyle::Normal), activity_run());
        assert_eq!(group_label_style("run", NodeStyle::Muted), activity_run());
        assert_eq!(
            group_label_style("explored", NodeStyle::Normal),
            activity_group()
        );
        assert_ne!(
            group_label_style("subagent repo_explorer started", NodeStyle::Normal),
            group_label_style("explored", NodeStyle::Normal)
        );
        assert_ne!(
            group_label_style("run", NodeStyle::Normal),
            group_label_style("explored", NodeStyle::Normal)
        );
    }
}
