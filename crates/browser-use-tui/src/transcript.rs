use browser_use_protocol::{normalize_result_text, EventRecord, SessionMeta, WorkbenchState};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthChar;

use crate::markdown::{highlight_code_line_spans, render_markdown_lines};
use crate::theme::{
    dim, failed, link, muted, path_reference, running, text_style, thought, user_prompt_accent,
    user_prompt_muted, user_prompt_text,
};

use super::App;

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
    PendingPrompt {
        text: String,
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
            TranscriptKind::PendingPrompt { text, status } => {
                prompt_lines_with_status(text, true, width, Some(status))
            }
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
            TranscriptKind::Prompt { text, .. } | TranscriptKind::PendingPrompt { text, .. } => {
                prefixed_plain("> ", text)
            }
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
        matches!(self.kind, TranscriptKind::PendingPrompt { .. })
    }

    fn is_prompt(&self) -> bool {
        matches!(self.kind, TranscriptKind::Prompt { .. })
    }

    fn is_followup_prompt(&self) -> bool {
        matches!(self.kind, TranscriptKind::Prompt { followup: true, .. })
    }
}

pub(crate) fn transcript_model(app: &App, state: &WorkbenchState) -> Option<TranscriptModel> {
    let session = state.current_session.as_ref()?;
    let events = app.cached_events_for_session(&session.id);
    let last_event_seq = events.last().map(|event| event.seq).unwrap_or_default();
    let mut committed = Vec::new();

    for event in events {
        if let Some(node) = committed_node_for_event(app, state, session, event) {
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
    defer_pending_prompt: bool,
) -> TerminalScrollbackEmission {
    let pending_prompt_seq = defer_pending_prompt
        .then(|| pending_prompt_seq(model))
        .flatten();
    let nodes = model
        .committed
        .iter()
        .filter(|node| node.seq() > after_seq)
        .filter(|node| !node.is_terminal_scrollback_transient())
        .filter(|node| Some(node.seq()) != pending_prompt_seq)
        .collect::<Vec<_>>();
    let last_seq = nodes.last().map(|node| node.seq()).unwrap_or(after_seq);
    TerminalScrollbackEmission {
        lines: cells_to_lines(nodes.iter().copied(), width, DisplayMode::Scrollback),
        last_seq,
    }
}

fn pending_prompt_seq(model: &TranscriptModel) -> Option<i64> {
    if !model
        .active
        .as_ref()
        .is_some_and(TranscriptNode::is_pending_followup_indicator)
    {
        return None;
    }
    let latest_prompt_idx = latest_followup_prompt_over_scrollback_idx(model)?;
    let has_non_prompt_after = model
        .committed
        .iter()
        .skip(latest_prompt_idx.saturating_add(1))
        .filter(|node| !node.is_terminal_scrollback_transient())
        .any(|node| !node.is_prompt());
    (!has_non_prompt_after).then(|| model.committed[latest_prompt_idx].seq())
}

pub(crate) fn has_followup_over_scrollback(model: Option<&TranscriptModel>) -> bool {
    model.is_some_and(|model| latest_followup_prompt_over_scrollback_idx(model).is_some())
}

fn latest_followup_prompt_over_scrollback_idx(model: &TranscriptModel) -> Option<usize> {
    let latest_prompt_idx = model
        .committed
        .iter()
        .rposition(TranscriptNode::is_prompt)?;
    if !model.committed[latest_prompt_idx].is_followup_prompt() {
        return None;
    }
    model
        .committed
        .iter()
        .take(latest_prompt_idx)
        .any(|node| !node.is_terminal_scrollback_transient())
        .then_some(latest_prompt_idx)
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
        (_, TranscriptKind::Prompt { .. } | TranscriptKind::PendingPrompt { .. }) => 1,
        (
            TranscriptKind::Prompt { .. } | TranscriptKind::PendingPrompt { .. },
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
        // Child-agent lifecycle is represented by the active child cell while
        // it is running and by agent.completed once it has a result. Emitting
        // agent.spawned separately makes the transcript look like duplicate
        // subagent blocks.
        "agent.spawned" => None,
        "agent.wait.started" => None,
        "agent.wait.finished" => {
            let timed_out = event
                .payload
                .get("timed_out")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            timed_out.then(|| {
                timeline_node(
                    event,
                    "subagent",
                    vec!["wait timed out".to_string()],
                    NodeStyle::Muted,
                )
            })
        }
        "agent.completed" => {
            let child_id = event
                .payload
                .get("child_session_id")
                .and_then(serde_json::Value::as_str);
            let label = child_id
                .map(|id| helper_label_for_child(app, &event.session_id, id))
                .unwrap_or_else(|| "subagent".to_string());
            let group = format!("subagent {label}");
            let mut lines = child_id
                .map(|id| completed_child_activity_lines(app, state, id, 4))
                .unwrap_or_default();
            lines.push("finished".to_string());
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
            vec!["stopped".to_string()],
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
        "model.turn.response" => model_turn_response_node(app, event),
        "model.turn.request"
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
            }
            last.id = next.id.clone();
            last.seq = next.seq;
            last.revision = next.revision;
            true
        }
        _ => false,
    }
}

fn model_turn_response_node(app: &App, event: &EventRecord) -> Option<TranscriptNode> {
    if model_response_tool_call_count(event) == 0 {
        return None;
    }
    let text = model_stream_text_for_response(app, event)?;
    let mut lines = Vec::new();
    for (idx, raw_line) in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        if idx == 0 {
            lines.push(format!("note: {raw_line}"));
        } else {
            lines.push(raw_line.to_string());
        }
    }
    (!lines.is_empty()).then(|| timeline_node(event, "note", lines, NodeStyle::Muted))
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

fn model_stream_text_for_response(app: &App, response_event: &EventRecord) -> Option<String> {
    let turn_idx = event_turn_idx(response_event)?;
    let request_seq = app
        .cached_events_for_session(&response_event.session_id)
        .iter()
        .rev()
        .find(|event| {
            event.seq < response_event.seq
                && event.event_type == "model.turn.request"
                && event_turn_idx(event) == Some(turn_idx)
        })
        .map(|event| event.seq)?;
    let mut text = String::new();
    for event in app.cached_events_for_session(&response_event.session_id) {
        if event.seq <= request_seq {
            continue;
        }
        if event.seq > response_event.seq {
            break;
        }
        if event.event_type != "model.stream_delta" || event_turn_idx(event) != Some(turn_idx) {
            continue;
        }
        if let Some(delta) = event_text_payload(event) {
            append_live_delta_text(&mut text, delta);
        }
    }
    let text = text.trim_end().to_string();
    (!text.trim().is_empty()).then_some(text)
}

fn event_turn_idx(event: &EventRecord) -> Option<i64> {
    event
        .payload
        .get("turn_idx")
        .and_then(serde_json::Value::as_i64)
}

fn event_text_payload(event: &EventRecord) -> Option<&str> {
    event
        .payload
        .get("text")
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

fn append_live_delta_text(current: &mut String, incoming: &str) {
    if current.is_empty() {
        current.push_str(incoming);
        return;
    }
    if incoming == current || incoming.trim() == current.trim() {
        return;
    }
    if let Some(suffix) = incoming.strip_prefix(current.as_str()) {
        current.push_str(suffix);
        return;
    }
    if incoming.chars().count() >= 24 && current.ends_with(incoming) {
        return;
    }
    current.push_str(incoming);
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

    if let Some(child) = active_child_session(app, &root.id) {
        let label = helper_label_for_session(app, &child.id);
        let group = format!("subagent {label}");
        let mut lines = vec!["working".to_string()];
        if let Some(wait_event) = live_events
            .iter()
            .rev()
            .find(|event| event.event_type == "agent.wait.started")
        {
            lines.push(wait_agent_started_label(&wait_event.payload));
        }
        let recent = recent_child_activity_lines(app, state, &child.id, ACTIVE_LIVE_LINE_LIMIT);
        if recent.is_empty() {
            if lines.len() == 1 {
                lines.push("waiting for activity".to_string());
            }
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
        .filter(|text| !text.is_empty());

    let mut active_nodes = Vec::new();

    let suppress_model_wait = live_streaming_text.is_some() && live_thinking_text.is_none();
    if let Some(event) = live_events.iter().rev().find(|event| {
        matches!(
            event.event_type.as_str(),
            "model.turn.request" | "model.turn.retry" | "agent.wait.started"
        ) && !(suppress_model_wait
            && matches!(
                event.event_type.as_str(),
                "model.turn.request" | "model.turn.retry"
            ))
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
    let text = payload_string(latest_followup, "text")?;
    let status = pending_followup_status(app, events, latest_followup.seq);
    Some(TranscriptNode {
        id: format!("{}:active-followup:{}", root.id, latest_followup.seq),
        seq: latest_followup.seq,
        revision: latest_followup.seq.max(0) as u64,
        kind: TranscriptKind::PendingPrompt { text, status },
    })
}

fn is_live_output_event(event: &EventRecord) -> bool {
    match event.event_type.as_str() {
        "model.stream_delta" | "model.thinking_delta" => event
            .payload
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| !text.trim().is_empty()),
        "agent.wait.started" | "command.waiting" | "tool.started" | "browser.page"
        | "browser.state" | "plan.updated" => true,
        _ => false,
    }
}

fn pending_followup_status(app: &App, events: &[EventRecord], after_seq: i64) -> String {
    let label = events
        .iter()
        .filter(|event| event.seq > after_seq)
        .rev()
        .find_map(|event| match event.event_type.as_str() {
            "model.turn.request" => {
                let model = payload_string(event, "model").unwrap_or_else(|| "model".to_string());
                Some(format!("waiting for {model}"))
            }
            "model.turn.retry" => Some("retrying model request".to_string()),
            "agent.wait.started" => Some("waiting for subagent".to_string()),
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
        "agent.wait.started" => Some(active_status_node(
            root,
            events,
            "subagent",
            vec![wait_agent_started_label(&event.payload)],
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
        Span::styled(": ", dim()),
        Span::styled(group.to_string(), group_style(style)),
    ]));
    let value_style = body_style(style);
    let content_width = width.saturating_sub(2).max(1);
    for value in values {
        for (_, wrapped) in wrap_plain(value, content_width) {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(styled_value_spans(group, &wrapped, value_style));
            lines.push(Line::from(spans));
        }
    }
    lines
}

fn styled_value_spans(group: &str, text: &str, fallback: Style) -> Vec<Span<'static>> {
    if text.starts_with("https://") || text.starts_with("http://") {
        return vec![Span::styled(text.to_string(), link())];
    }
    if group == "run" && looks_like_shell_line(text) {
        if let Some(spans) = highlight_code_line_spans(text, Some("bash")) {
            return spans;
        }
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
        if let Some(command_spans) = highlight_code_line_spans(rest, Some("bash")) {
            spans.extend(command_spans);
        } else {
            spans.extend(styled_path_tokens(rest, fallback));
        }
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
        "run" | "read" | "list" | "search" | "artifact" | "command" => running(),
        "working" | "waiting" => thought(),
        "task" | "follow-up" => user_prompt_accent(),
        _ => group_style(NodeStyle::Normal),
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

fn looks_like_shell_line(text: &str) -> bool {
    let trimmed = text.trim_start();
    looks_like_command_line(trimmed)
        || trimmed.starts_with('|')
        || trimmed.starts_with("&&")
        || trimmed.starts_with("||")
        || trimmed.contains(" | ")
        || trimmed.contains(" && ")
        || trimmed.contains(" || ")
        || trimmed.contains(" > ")
        || trimmed.contains(" < ")
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

fn wait_agent_started_label(payload: &serde_json::Value) -> String {
    if let Some(target) = payload
        .get("target")
        .and_then(serde_json::Value::as_str)
        .map(short_agent_label)
    {
        return format!("waiting on {target}");
    }
    if let Some(targets) = payload.get("targets").and_then(serde_json::Value::as_array) {
        return match targets.len() {
            0 => "waiting on subagents".to_string(),
            1 => {
                let target = targets
                    .first()
                    .and_then(|target| {
                        target
                            .get("nickname")
                            .and_then(serde_json::Value::as_str)
                            .or_else(|| target.get("task_name").and_then(serde_json::Value::as_str))
                    })
                    .map(short_agent_label)
                    .unwrap_or_else(|| "subagent".to_string());
                format!("waiting on {target}")
            }
            count => format!("waiting on {count} subagents"),
        };
    }
    "waiting on subagents".to_string()
}

fn short_agent_label(value: &str) -> String {
    value
        .trim()
        .trim_matches('/')
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty() && *segment != "root")
        .unwrap_or(value)
        .to_string()
}

fn active_child_session<'a>(app: &'a App, root_id: &str) -> Option<&'a SessionMeta> {
    app.state_cache
        .sessions
        .iter()
        .filter(|session| session.parent_id.as_deref() == Some(root_id))
        .find(|session| session.status.is_active())
}

fn completed_child_activity_lines(
    app: &App,
    state: &WorkbenchState,
    child_id: &str,
    limit: usize,
) -> Vec<String> {
    let child_events = app.cached_events_for_session(child_id);
    if !child_session_has_terminal_event(child_events) {
        return Vec::new();
    }

    let mut lines = Vec::new();
    for event in child_events.iter().rev() {
        if matches!(
            event.event_type.as_str(),
            "session.input"
                | "session.followup"
                | "session.done"
                | "session.failed"
                | "session.cancelled"
                | "model.thinking_delta"
                | "model.stream_delta"
        ) {
            continue;
        }
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

fn child_session_has_terminal_event(events: &[EventRecord]) -> bool {
    events.iter().any(|event| {
        matches!(
            event.event_type.as_str(),
            "session.done" | "session.failed" | "session.cancelled"
        )
    })
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
        "model.thinking_delta" | "model.stream_delta" | "model.tool_call" | "tool.started" => None,
        "tool.output" => {
            let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
            should_show_generic_tool_output_text(&name)
                .then(|| payload_string(event, "text"))
                .flatten()
                .map(|text| truncate_text(text.trim(), 120))
                .filter(|text| !text.is_empty())
        }
        "tool.failed" => {
            let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
            let error = payload_string(event, "error").unwrap_or_else(|| "tool failed".to_string());
            Some(format!("{name} failed: {}", truncate_text(&error, 96)))
        }
        "tool.image" => Some(tool_image_label(event, state)),
        "artifact.created" => event
            .payload
            .get("artifact")
            .and_then(|artifact| artifact.get("path"))
            .and_then(serde_json::Value::as_str)
            .map(|path| format!("artifact {}", display_path(path, state))),
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
    fn run_values_highlight_commands_and_paths() {
        let command_spans = styled_value_spans(
            "run",
            "find crates -maxdepth 3 -type f | sort",
            text_style(),
        );
        assert!(command_spans.iter().any(|span| {
            span.content.as_ref() == "find"
                && span
                    .style
                    .fg
                    .is_some_and(|color| color != crate::theme::text())
        }));

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
    fn nested_activity_run_lines_highlight_commands() {
        let spans = styled_value_spans(
            "subagent repo explorer",
            "run pwd && find . -maxdepth 2 -type f | sed 's# ./##' | sort | head -200",
            text_style(),
        );
        assert!(spans
            .iter()
            .any(|span| span.content.as_ref() == "run" && span.style == running()));
        assert!(spans.iter().any(|span| {
            span.content.as_ref() == "find"
                && span
                    .style
                    .fg
                    .is_some_and(|color| color != crate::theme::text())
        }));
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
            .any(|span| span.content.as_ref() == "task" && span.style == user_prompt_accent()));
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
            ("waiting for gpt-5.5", thought()),
            ("list .", running()),
            ("read Taskfile.yml", running()),
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
}
