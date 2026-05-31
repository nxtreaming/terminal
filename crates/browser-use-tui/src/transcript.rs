use browser_use_agent::prompts::CollaborationModeKind;
use browser_use_protocol::{
    normalize_result_text, turn_streaming_text_from_events, EventRecord, SessionMeta,
    WorkbenchState,
};
use browser_use_store::now_ms;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthChar;

use crate::markdown::render_markdown_lines;
use crate::theme::{
    accent, activity_group, activity_list, activity_read, activity_run, activity_search,
    activity_task, dim, failed, link, muted, path_reference, text_style, thought,
    user_prompt_accent, user_prompt_muted, user_prompt_text,
};

use super::{
    user_input_display_text_from_payload, App, PendingRequestUserInput, RequestUserInputFocus,
    RequestUserInputQuestion, RequestUserInputState, REQUEST_USER_INPUT_OTHER_LABEL,
    SESSION_QUEUED_FOLLOWUP_EVENT,
};

const GROUP_VALUE_RAIL_PREFIX: &str = "  │ ";
const GROUP_VALUE_LAST_PREFIX: &str = "  └ ";
const ACTIVE_FALLBACK_STATUS: &str = "running browser task";
const LIVE_STREAM_QUIET_STATUS_DELAY_MS: i64 = 500;

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
    live_phase: usize,
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
        detail: Option<String>,
    },
    Assistant {
        markdown: String,
        source: Option<String>,
    },
    StreamingAssistant {
        markdown: String,
    },
    ProposedPlan {
        markdown: String,
    },
    RequestUserInput {
        request: PendingRequestUserInput,
        state: RequestUserInputState,
    },
    ResultFile {
        file_path: String,
        bytes: Option<u64>,
        mime: Option<String>,
        source: Option<String>,
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
            TranscriptKind::PendingStatus { status, detail } => {
                pending_status_lines(status, detail.as_deref(), ShimmerMode::Static)
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
            TranscriptKind::ProposedPlan { markdown } => proposed_plan_lines(markdown, width),
            TranscriptKind::RequestUserInput { request, state } => {
                request_user_input_lines(request, state, width)
            }
            TranscriptKind::ResultFile {
                file_path,
                bytes,
                mime,
                source,
            } => {
                let mut lines = result_file_lines(file_path, *bytes, mime.as_deref(), width);
                if let Some(source) = source.as_deref() {
                    lines.extend(source_display_lines(source, width));
                }
                lines
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
            TranscriptKind::PendingStatus { status, detail } => {
                vec![pending_status_text(status, detail.as_deref())]
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
            TranscriptKind::ProposedPlan { markdown } => {
                let mut out = vec!["Proposed Plan".to_string()];
                out.extend(markdown.lines().map(str::to_string));
                out
            }
            TranscriptKind::RequestUserInput { request, state } => {
                request_user_input_plain_lines(request, state)
            }
            TranscriptKind::ResultFile {
                file_path,
                bytes,
                mime,
                source,
            } => {
                let mut out = result_file_plain_lines(file_path, *bytes, mime.as_deref());
                if let Some(source) = source.as_ref() {
                    out.push(format!("source {source}"));
                }
                out
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

    fn has_shimmering_live_status(&self) -> bool {
        match &self.kind {
            TranscriptKind::PendingStatus { .. } => true,
            TranscriptKind::Stack { nodes } => {
                nodes.iter().any(TranscriptNode::has_shimmering_live_status)
            }
            _ => false,
        }
    }

    fn needs_leading_status_padding(&self) -> bool {
        match &self.kind {
            TranscriptKind::PendingStatus { .. } => true,
            TranscriptKind::StreamingAssistant { .. } => true,
            TranscriptKind::Stack { nodes } => nodes
                .iter()
                .find(|node| !node.is_active_viewport_placeholder())
                .is_some_and(TranscriptNode::needs_leading_status_padding),
            _ => false,
        }
    }

    fn is_prompt(&self) -> bool {
        matches!(self.kind, TranscriptKind::Prompt { .. })
    }

    fn active_display_lines(
        &self,
        width: u16,
        shimmer_phase: usize,
        stream_skip_lines: Option<&mut usize>,
        allow_empty_stream: bool,
    ) -> Vec<Line<'static>> {
        let width = width.max(1);
        match &self.kind {
            TranscriptKind::Stack { nodes } => {
                let mut out = Vec::new();
                let mut previous_kind = None;
                let mut stream_skip_lines = stream_skip_lines;
                for (idx, node) in nodes.iter().enumerate() {
                    let _ = (node.id(), node.revision());
                    let child_allow_empty_stream =
                        matches!(node.kind, TranscriptKind::StreamingAssistant { .. })
                            && nodes[idx + 1..].iter().any(|node| {
                                matches!(node.kind, TranscriptKind::PendingStatus { .. })
                            });
                    let child_lines = node.active_display_lines(
                        width,
                        shimmer_phase,
                        stream_skip_lines.as_deref_mut(),
                        child_allow_empty_stream,
                    );
                    if child_lines.is_empty() {
                        continue;
                    }
                    if !out.is_empty() {
                        let gap = previous_kind
                            .map(|previous| gap_lines_between(previous, &node.kind))
                            .unwrap_or(0);
                        out.extend(std::iter::repeat_with(|| Line::from("")).take(gap));
                    }
                    out.extend(child_lines);
                    previous_kind = Some(&node.kind);
                }
                out
            }
            TranscriptKind::PendingStatus { status, detail } => pending_status_lines(
                status,
                detail.as_deref(),
                ShimmerMode::AnimatedAt(shimmer_phase),
            ),
            TranscriptKind::ActiveStatus {
                group,
                lines,
                style,
            } => grouped_lines(group, lines, *style, width),
            TranscriptKind::StreamingAssistant { markdown } => {
                let mut lines = markdown_cell_lines(markdown, width, DisplayMode::Active);
                if let Some(stream_skip_lines) = stream_skip_lines {
                    let max_skip = if allow_empty_stream {
                        lines.len()
                    } else {
                        lines.len().saturating_sub(1)
                    };
                    let skip = (*stream_skip_lines).min(max_skip);
                    *stream_skip_lines = (*stream_skip_lines).saturating_sub(skip);
                    if skip > 0 {
                        lines = lines.into_iter().skip(skip).collect();
                    }
                }
                lines
            }
            TranscriptKind::ProposedPlan { markdown } => proposed_plan_lines(markdown, width),
            TranscriptKind::RequestUserInput { request, state } => {
                request_user_input_lines(request, state, width)
            }
            _ => self.display_lines(width, DisplayMode::Active),
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
            TranscriptKind::ProposedPlan { markdown } => proposed_plan_lines(markdown, width),
            _ => Vec::new(),
        }
    }

    fn can_commit_full_live_stream(&self) -> bool {
        match &self.kind {
            TranscriptKind::Stack { nodes } => nodes.iter().enumerate().any(|(idx, node)| {
                matches!(node.kind, TranscriptKind::StreamingAssistant { .. })
                    && nodes[idx + 1..]
                        .iter()
                        .any(|node| matches!(node.kind, TranscriptKind::PendingStatus { .. }))
            }),
            _ => false,
        }
    }
}

pub(crate) fn transcript_model(app: &App, state: &WorkbenchState) -> Option<TranscriptModel> {
    let session = state.current_session.as_ref()?;
    let raw_events = app.cached_events_for_session(&session.id);
    let last_event_seq = raw_events.last().map(|event| event.seq).unwrap_or_default();
    let events =
        browser_use_agent::context::workspace_context::rollback_filtered_event_records(raw_events)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
    let events = events.as_slice();
    let mut committed = Vec::new();
    let mut terminal_committed = Vec::new();

    for event in events {
        if let Some(node) = committed_node_for_event(app, state, session, events, event) {
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
        live_phase: app.live_spinner_frame,
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
    let mut lines = active.active_display_lines(
        width,
        model.map(|model| model.live_phase).unwrap_or(0),
        None,
        false,
    );
    if active.needs_leading_status_padding() && !lines.is_empty() {
        lines.insert(0, Line::from(""));
    }
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
    let mut lines = active.active_display_lines(
        width,
        model.map(|model| model.live_phase).unwrap_or(0),
        Some(&mut skip),
        false,
    );
    let consumed_stream_lines = stream_skip_lines > skip;
    if active.needs_leading_status_padding() && !lines.is_empty() && !consumed_stream_lines {
        lines.insert(0, Line::from(""));
    }
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

pub(crate) fn active_streaming_can_commit_all(model: Option<&TranscriptModel>) -> bool {
    model
        .and_then(|model| model.active.as_ref())
        .is_some_and(TranscriptNode::can_commit_full_live_stream)
}

#[cfg(test)]
pub(crate) fn active_viewport_has_live_content(model: Option<&TranscriptModel>) -> bool {
    model
        .and_then(|model| model.active.as_ref())
        .is_some_and(|active| !active.is_active_viewport_placeholder())
}

pub(crate) fn has_shimmering_live_status(model: Option<&TranscriptModel>) -> bool {
    model
        .and_then(|model| model.active.as_ref())
        .is_some_and(TranscriptNode::has_shimmering_live_status)
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
        (TranscriptKind::StreamingAssistant { .. }, TranscriptKind::PendingStatus { .. }) => 0,
        (_, TranscriptKind::Prompt { .. } | TranscriptKind::PendingStatus { .. }) => 1,
        (
            TranscriptKind::Prompt { .. } | TranscriptKind::PendingStatus { .. },
            TranscriptKind::Timeline { .. } | TranscriptKind::ActiveStatus { .. },
        ) => 1,
        (
            TranscriptKind::Prompt { .. } | TranscriptKind::PendingStatus { .. },
            TranscriptKind::Assistant { .. }
            | TranscriptKind::ProposedPlan { .. }
            | TranscriptKind::RequestUserInput { .. }
            | TranscriptKind::ResultFile { .. }
            | TranscriptKind::StreamingAssistant { .. }
            | TranscriptKind::Error { .. }
            | TranscriptKind::Cancelled { .. }
            | TranscriptKind::Stack { .. },
        ) => 1,
        _ => 1,
    }
}

fn committed_node_for_event(
    app: &App,
    state: &WorkbenchState,
    root: &SessionMeta,
    events: &[EventRecord],
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
        SESSION_QUEUED_FOLLOWUP_EVENT => {
            if !app.queued_followup_is_pending(root.id.as_str(), event.seq) {
                return None;
            }
            let text = payload_string(event, "text")?;
            Some(TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::PendingStatus {
                    status: "queued follow-up".to_string(),
                    detail: Some(text),
                },
            })
        }
        "session.done" => {
            if let Some(result_file) = session_done_result_file(event, state) {
                return Some(TranscriptNode {
                    id,
                    seq: event.seq,
                    revision: event.seq.max(0) as u64,
                    kind: TranscriptKind::ResultFile {
                        file_path: result_file.file_path,
                        bytes: result_file.bytes,
                        mime: result_file.mime,
                        source: source_for_state(state),
                    },
                });
            }
            let result = session_done_result_text(event)?;
            if result.trim().is_empty() {
                return None;
            }
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
        "model.turn.response" => pre_tool_commentary_node(root, events, event),
        "plan.proposed" => {
            let text = payload_string(event, "text")?;
            Some(TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::ProposedPlan { markdown: text },
            })
        }
        "session.failed" => {
            let text =
                payload_string(event, "error").unwrap_or_else(|| "The task failed.".to_string());
            let node = TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::Error { text },
            };
            Some(with_streaming_commentary_before_event(
                root, events, event, node,
            ))
        }
        "session.cancelled" => {
            let node = TranscriptNode {
                id,
                seq: event.seq,
                revision: event.seq.max(0) as u64,
                kind: TranscriptKind::Cancelled {
                    text: "Progress is saved in history.".to_string(),
                },
            };
            Some(with_streaming_commentary_before_event(
                root, events, event, node,
            ))
        }
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
        "tool.failed" => Some(timeline_node(
            event,
            "error",
            tool_failed_lines(event),
            NodeStyle::Failed,
        )),
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
            vec![payload_string(event, "live_url")
                .or_else(|| payload_string(event, "url"))
                .map(|url| format!("live view {}", compact_url(&url)))
                .unwrap_or_else(|| "live view available".to_string())],
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
        "request_user_input.requested" => None,
        "request_user_input.response" => Some(request_user_input_response_node(event)),
        "session.deadline_warning" => Some(timeline_node(
            event,
            "warning",
            vec!["turn budget is nearly exhausted".to_string()],
            NodeStyle::Muted,
        )),
        "session.startup_warning" => Some(timeline_node(
            event,
            "warning",
            vec![payload_string(event, "message").unwrap_or_else(|| "startup warning".to_string())],
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
        | "model.thinking_delta"
        | "model.turn.retry"
        | "model.stream_delta"
        | "model.delta"
        | "model.response.output_item.completed"
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

fn model_response_tool_call_count(event: &EventRecord) -> u64 {
    event
        .payload
        .get("tool_call_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn pre_tool_commentary_node(
    root: &SessionMeta,
    events: &[EventRecord],
    event: &EventRecord,
) -> Option<TranscriptNode> {
    if event.session_id != root.id || model_response_tool_call_count(event) == 0 {
        return None;
    }
    streaming_commentary_node_before_event(root, events, event)
}

fn with_streaming_commentary_before_event(
    root: &SessionMeta,
    events: &[EventRecord],
    event: &EventRecord,
    node: TranscriptNode,
) -> TranscriptNode {
    let Some(commentary) = streaming_commentary_node_before_event(root, events, event) else {
        return node;
    };
    TranscriptNode {
        id: format!("{}:{}:stack", event.session_id, event.seq),
        seq: event.seq,
        revision: event.seq.max(0) as u64,
        kind: TranscriptKind::Stack {
            nodes: vec![commentary, node],
        },
    }
}

fn streaming_commentary_node_before_event(
    root: &SessionMeta,
    events: &[EventRecord],
    event: &EventRecord,
) -> Option<TranscriptNode> {
    if event.session_id != root.id {
        return None;
    }
    let event_idx = events.iter().position(|candidate| {
        candidate.session_id == event.session_id
            && candidate.seq == event.seq
            && candidate.event_type == event.event_type
    })?;
    let turn_start = events[..event_idx]
        .iter()
        .rposition(|candidate| {
            candidate.session_id == root.id
                && (matches!(
                    candidate.event_type.as_str(),
                    "model.turn.request" | "model.turn.retry" | "model.turn.error"
                ) || (candidate.event_type == "model.turn.response"
                    && model_response_tool_call_count(candidate) > 0))
        })
        .map(|idx| idx.saturating_add(1))
        .unwrap_or(0);
    let markdown = turn_streaming_text_from_events(&events[turn_start..event_idx])?;
    let markdown = markdown.trim_end().to_string();
    if markdown.trim().is_empty() {
        return None;
    }
    Some(TranscriptNode {
        id: format!("{}:{}:commentary", event.session_id, event.seq),
        seq: event.seq,
        revision: event.seq.max(0) as u64,
        kind: TranscriptKind::Assistant {
            markdown,
            source: None,
        },
    })
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

    let active_child_count = active_child_session_count(app, &root.id);
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
    let live_status = live_status_for_session(active_child_count, live_thinking_text, live_events);

    if let Some(text) = live_streaming_text {
        let seq = events.last().map(|event| event.seq).unwrap_or_default();
        let mode = collaboration_mode_from_events(events);
        let (visible_text, proposed_plan) = if mode == CollaborationModeKind::Plan {
            split_proposed_plan_blocks(text)
        } else {
            (text.to_string(), None)
        };
        if !visible_text.trim().is_empty() {
            active_nodes.push(TranscriptNode {
                id: format!("{}:active-stream", root.id),
                seq,
                revision: seq.max(0) as u64,
                kind: TranscriptKind::StreamingAssistant {
                    markdown: visible_text,
                },
            });
        }
        if let Some(plan) = proposed_plan.filter(|plan| !plan.trim().is_empty()) {
            active_nodes.push(TranscriptNode {
                id: format!("{}:active-plan", root.id),
                seq,
                revision: seq.max(0) as u64,
                kind: TranscriptKind::ProposedPlan { markdown: plan },
            });
        }
    }

    if let Some(request) = app.pending_request_user_input(&root.id) {
        let seq = events.last().map(|event| event.seq).unwrap_or_default();
        let state = app.request_input_display_state(&root.id, &request);
        active_nodes.push(TranscriptNode {
            id: format!("{}:active-request-user-input:{}", root.id, request.call_id),
            seq,
            revision: seq.max(0) as u64,
            kind: TranscriptKind::RequestUserInput { request, state },
        });
    }

    if app.native_scrollback_is_active() && live_streaming_text.is_none() {
        if let Some(node) = active_timeline_tail_node(app, state, root, live_events) {
            active_nodes.push(node);
        }
    }

    if !app.native_scrollback_is_active() && live_streaming_text.is_none() {
        if let Some(event) = live_events.iter().rev().find(|event| {
            matches!(
                event.event_type.as_str(),
                "command.waiting"
                    | "tool.started"
                    | "browser.page"
                    | "browser.state"
                    | "plan.updated"
                    | "request_user_input.requested"
            )
        }) {
            if let Some(node) = active_node_for_event(root, events, event) {
                active_nodes.push(node);
            }
        }
    }
    if live_streaming_text.is_none()
        || (live_status == "Thinking..." && live_stream_pending_status_allowed(live_events))
    {
        active_nodes.push(pending_status_node(
            root,
            events,
            live_status,
            active_subagent_summary(active_child_count).as_deref(),
        ));
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

    Some(pending_status_node(
        root,
        events,
        live_status,
        active_subagent_summary(active_child_count).as_deref(),
    ))
}

fn live_status_for_session(
    active_child_count: usize,
    live_thinking_text: Option<&str>,
    live_events: &[EventRecord],
) -> &'static str {
    if active_child_count > 0 {
        return "Working...";
    }
    if live_events
        .iter()
        .rev()
        .any(|event| event.event_type == "model.turn.retry")
    {
        return "Retrying...";
    }
    if live_thinking_text.is_some()
        || live_events
            .iter()
            .rev()
            .any(|event| event.event_type == "model.turn.request")
    {
        return "Thinking...";
    }
    "Working..."
}

fn active_child_session_count(app: &App, root_id: &str) -> usize {
    app.state_cache
        .sessions
        .iter()
        .filter(|session| {
            session.parent_id.as_deref() == Some(root_id) && session.status.is_active()
        })
        .count()
}

fn active_subagent_summary(active_child_count: usize) -> Option<String> {
    if active_child_count == 0 {
        return None;
    }
    let noun = if active_child_count == 1 {
        "subagent"
    } else {
        "subagents"
    };
    Some(format!("({active_child_count} {noun} running)"))
}

fn active_timeline_tail_node(
    app: &App,
    state: &WorkbenchState,
    root: &SessionMeta,
    live_events: &[EventRecord],
) -> Option<TranscriptNode> {
    let after_seq = app.native_history.last_seq;
    let nodes = live_events
        .iter()
        .filter(|event| event.seq > after_seq)
        .filter_map(|event| committed_node_for_event(app, state, root, live_events, event))
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
        .filter_map(|event| committed_node_for_event(app, state, root, events, event))
        .any(|node| !node.is_terminal_scrollback_transient());
    if !has_prior_scrollback {
        return None;
    }
    let has_committed_output_after = events
        .iter()
        .filter(|event| event.seq > latest_followup.seq)
        .filter_map(|event| committed_node_for_event(app, state, root, events, event))
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
    let status = pending_followup_status(events, latest_followup.seq);
    Some(TranscriptNode {
        id: format!("{}:active-followup:{}", root.id, latest_followup.seq),
        seq: latest_followup.seq,
        revision: latest_followup.seq.max(0) as u64,
        kind: TranscriptKind::PendingStatus {
            status,
            detail: None,
        },
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

fn live_stream_pending_status_allowed(live_events: &[EventRecord]) -> bool {
    let Some(latest_stream) = latest_nonempty_stream_event(live_events) else {
        return false;
    };
    if live_events
        .iter()
        .any(|event| event.seq > latest_stream.seq && event.event_type != "model.stream_delta")
    {
        return true;
    }
    now_ms().saturating_sub(latest_stream.ts_ms) >= LIVE_STREAM_QUIET_STATUS_DELAY_MS
}

fn latest_nonempty_stream_event(live_events: &[EventRecord]) -> Option<&EventRecord> {
    live_events.iter().rev().find(|event| {
        event.event_type == "model.stream_delta"
            && event
                .payload
                .get("text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
    })
}

fn is_live_output_event(event: &EventRecord) -> bool {
    match event.event_type.as_str() {
        "model.stream_delta" | "model.thinking_delta" => event
            .payload
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| !text.trim().is_empty()),
        "command.waiting"
        | "tool.started"
        | "browser.page"
        | "browser.state"
        | "plan.updated"
        | "request_user_input.requested" => true,
        _ => false,
    }
}

fn pending_followup_status(events: &[EventRecord], after_seq: i64) -> String {
    events
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
        .unwrap_or_else(|| "sending".to_string())
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
        "request_user_input.requested" => None,
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

fn pending_status_node(
    root: &SessionMeta,
    events: &[EventRecord],
    status: &str,
    detail: Option<&str>,
) -> TranscriptNode {
    let seq = events.last().map(|event| event.seq).unwrap_or_default();
    TranscriptNode {
        id: format!("{}:active-status", root.id),
        seq,
        revision: seq.max(0) as u64,
        kind: TranscriptKind::PendingStatus {
            status: status.to_string(),
            detail: detail.map(str::to_string),
        },
    }
}

fn tool_output_node(event: &EventRecord) -> Option<TranscriptNode> {
    let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
    if is_subagent_management_tool(&name) || name == "request_user_input" {
        return None;
    }
    let mut lines = Vec::new();
    let browser_script_summary_lines = if name == "browser_script" {
        browser_script_summary_lines(event)
    } else {
        Vec::new()
    };
    let has_browser_script_summary = !browser_script_summary_lines.is_empty();
    lines.extend(browser_script_summary_lines);
    if name == "browser_script" && !has_browser_script_summary {
        lines.extend(browser_script_structured_output_lines(event));
    }
    if should_show_generic_tool_output_text(&name)
        && !(name == "browser_script" && has_browser_script_summary)
    {
        if let Some(text) = payload_string(event, "text").filter(|text| !text.trim().is_empty()) {
            if name == "browser_script" {
                lines.extend(browser_script_text_preview_lines(&text));
            } else {
                lines.extend(preview_lines(&text, 3));
            }
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

fn browser_script_summary_lines(event: &EventRecord) -> Vec<String> {
    let Some(summary) = event
        .payload
        .get("summary")
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    let mut lines = summary
        .iter()
        .filter_map(browser_script_summary_record_line)
        .take(6)
        .collect::<Vec<_>>();
    if summary.len() > lines.len() {
        lines.push(format!("... +{} summaries", summary.len() - lines.len()));
    }
    lines
}

fn browser_script_summary_record_line(value: &serde_json::Value) -> Option<String> {
    let kind = summary_value_string(value, "kind").unwrap_or_else(|| "summary".to_string());
    let message = summary_value_string(value, "message");
    match kind.as_str() {
        "page" | "opened" | "navigation" | "navigated" => {
            let mut line = if let Some(url) = summary_value_string(value, "url") {
                format!("page: {}", compact_url(&url))
            } else if let Some(message) = message.as_deref() {
                format!("page: {}", truncate_inline(message, 140))
            } else {
                "page: updated".to_string()
            };
            if let Some(title) = summary_value_string(value, "title") {
                line.push_str(" - ");
                line.push_str(&truncate_inline(&title, 80));
            }
            Some(line)
        }
        "click" | "clicked" => {
            let target = summary_value_string(value, "text")
                .or_else(|| summary_value_string(value, "label"))
                .or_else(|| summary_value_string(value, "selector"))
                .or(message)
                .unwrap_or_else(|| "target".to_string());
            let mut line = format!("clicked: {}", truncate_inline(&target, 100));
            if let Some(url) =
                summary_value_string(value, "href").or_else(|| summary_value_string(value, "url"))
            {
                line.push_str(" -> ");
                line.push_str(&compact_url(&url));
            }
            Some(line)
        }
        "input" | "typed" | "fill" | "filled" => {
            let target = summary_value_string(value, "label")
                .or_else(|| summary_value_string(value, "selector"))
                .or(message)
                .unwrap_or_else(|| "field".to_string());
            Some(format!("filled: {}", truncate_inline(&target, 120)))
        }
        "extract" | "extracted" => {
            if let Some(message) = message {
                return Some(truncate_inline(&message, 140));
            }
            if let Some(count) = summary_value_string(value, "count") {
                return Some(format!("extracted: {count} items"));
            }
            Some("extracted: data".to_string())
        }
        "screenshot" | "image" => {
            let label = summary_value_string(value, "label")
                .or(message)
                .unwrap_or_else(|| "screenshot".to_string());
            Some(format!("screenshot: {}", truncate_inline(&label, 120)))
        }
        _ => {
            if let Some(message) = message {
                return Some(truncate_inline(&message, 140));
            }
            if let Some(url) = summary_value_string(value, "url") {
                return Some(format!("{kind}: {}", compact_url(&url)));
            }
            compact_summary_json(value).map(|summary| format!("{kind}: {summary}"))
        }
    }
}

fn browser_script_structured_output_lines(event: &EventRecord) -> Vec<String> {
    let Some(outputs) = event
        .payload
        .get("outputs")
        .and_then(serde_json::Value::as_array)
        .filter(|outputs| !outputs.is_empty())
    else {
        return Vec::new();
    };
    let labels = outputs
        .iter()
        .filter_map(|output| summary_value_string(output, "label"))
        .take(3)
        .collect::<Vec<_>>();
    if labels.is_empty() {
        return vec![format!(
            "{} structured output{}",
            outputs.len(),
            plural(outputs.len())
        )];
    }
    let mut line = format!("structured output: {}", labels.join(", "));
    if outputs.len() > labels.len() {
        line.push_str(&format!(" (+{})", outputs.len() - labels.len()));
    }
    vec![line]
}

fn browser_script_text_preview_lines(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.starts_with("browser_script is still running.") {
        return Vec::new();
    }
    let visible = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .filter(|line| !is_browser_script_runtime_instruction_line(line))
        .collect::<Vec<_>>();
    let mut out = visible
        .iter()
        .take(2)
        .map(|line| truncate_inline(line, 180))
        .collect::<Vec<_>>();
    if visible.len() > out.len() {
        out.push(format!("... +{} lines", visible.len() - out.len()));
    }
    out
}

fn is_browser_script_runtime_instruction_line(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("run_id:")
        || line.starts_with("Next:")
        || line.starts_with("Next step:")
        || line == "browser_script is still running."
}

fn summary_value_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|value| match value {
        serde_json::Value::String(text) => {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_string())
        }
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => Some(value.to_string()),
        _ => None,
    })
}

fn compact_summary_json(value: &serde_json::Value) -> Option<String> {
    serde_json::to_string(value)
        .ok()
        .map(|text| truncate_inline(&text, 160))
        .filter(|text| !text.is_empty())
}

#[derive(Debug, Deserialize)]
struct TranscriptRequestUserInputAnswer {
    answers: Vec<String>,
}

fn request_user_input_response_node(event: &EventRecord) -> TranscriptNode {
    let mut lines = Vec::new();
    let answers = event
        .payload
        .get("answers")
        .cloned()
        .and_then(|value| {
            serde_json::from_value::<HashMap<String, TranscriptRequestUserInputAnswer>>(value).ok()
        })
        .unwrap_or_default();
    let questions = event
        .payload
        .get("questions")
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<RequestUserInputQuestion>>(value).ok())
        .unwrap_or_default();
    if !questions.is_empty() {
        let answered = questions
            .iter()
            .filter(|question| {
                answers
                    .get(&question.id)
                    .is_some_and(|answer| !answer.answers.is_empty())
            })
            .count();
        lines.push(format!("Questions {answered}/{} answered", questions.len()));
        for question in questions {
            let label = request_user_input_question_label(&question);
            lines.push(format!("{label}: {}", question.question));
            let values = answers
                .get(&question.id)
                .map(|answer| answer.answers.clone())
                .unwrap_or_default();
            if values.is_empty() {
                lines.push("  unanswered".to_string());
                continue;
            }
            if question.is_secret {
                lines.push("  answer: ******".to_string());
                continue;
            }
            let (choices, note) = split_request_user_input_values(&values);
            for choice in choices {
                lines.push(format!("  answer: {choice}"));
            }
            if let Some(note) = note {
                let label = if question.options.is_some() {
                    "note"
                } else {
                    "answer"
                };
                lines.push(format!("  {label}: {note}"));
            }
        }
    } else {
        for (id, answer) in answers {
            let values = answer
                .answers
                .into_iter()
                .filter(|value| !value.trim().is_empty())
                .collect::<Vec<_>>();
            if values.is_empty() {
                lines.push(format!("{id}: unanswered"));
            } else if values.len() == 1 {
                lines.push(format!("{id}: {}", values[0]));
            } else {
                lines.push(format!("{id}: {}", values.join("; ")));
            }
        }
    }
    if lines.is_empty() {
        lines.push("answered request".to_string());
    }
    timeline_node(event, "questions", lines, NodeStyle::Muted)
}

fn split_request_user_input_values(values: &[String]) -> (Vec<String>, Option<String>) {
    let mut choices = Vec::new();
    let mut note = None;
    for value in values {
        if let Some(rest) = value.strip_prefix("user_note: ") {
            note = Some(rest.to_string());
        } else {
            choices.push(value.clone());
        }
    }
    (choices, note)
}

fn artifact_created_node(event: &EventRecord, _state: &WorkbenchState) -> Option<TranscriptNode> {
    let artifact = event.payload.get("artifact")?;
    let path = artifact
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)?;
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
        "browser_script" => Some(("browser", "running browser script")),
        "python" => Some(("python", "running browser Python")),
        "exec_command" => Some(("run", "running command")),
        "write_stdin" => Some(("run", "writing to command")),
        "apply_patch" => Some(("edit", "applying patch")),
        "view_image" => Some(("image", "inspecting image")),
        "update_plan" => Some(("plan", "updating plan")),
        "request_user_input" => Some(("questions", "waiting for your answer")),
        _ => None,
    }
}

fn should_show_generic_tool_output_text(name: &str) -> bool {
    !is_known_tool_with_domain_events(name)
}

fn tool_output_group(name: &str) -> &str {
    match name {
        "browser_script" => "browser",
        "python" => "python",
        _ => "tool",
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

#[derive(Clone, Copy)]
enum ShimmerMode {
    Static,
    AnimatedAt(usize),
}

fn pending_status_lines(
    status: &str,
    detail: Option<&str>,
    shimmer: ShimmerMode,
) -> Vec<Line<'static>> {
    let mut spans = vec![Span::styled("• ".to_string(), dim())];
    spans.extend(match shimmer {
        ShimmerMode::Static => vec![Span::styled(status.to_string(), muted())],
        ShimmerMode::AnimatedAt(phase) => shimmer_spans(status, phase, muted()),
    });
    if let Some(detail) = detail.filter(|detail| !detail.trim().is_empty()) {
        spans.push(Span::styled("  ".to_string(), dim()));
        spans.push(Span::styled(detail.to_string(), muted()));
    }
    vec![Line::from(spans)]
}

fn pending_status_text(status: &str, detail: Option<&str>) -> String {
    match detail.filter(|detail| !detail.trim().is_empty()) {
        Some(detail) => format!("• {status}  {detail}"),
        None => format!("• {status}"),
    }
}

fn shimmer_spans(text: &str, phase: usize, base: Style) -> Vec<Span<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    let chars = text.chars().collect::<Vec<_>>();
    let center = (phase % chars.len().max(1)) as isize;
    let mut spans = Vec::new();
    let mut pending = String::new();
    let mut pending_style = base;
    let mut have_pending = false;

    for (idx, ch) in chars.into_iter().enumerate() {
        let distance = (idx as isize - center).unsigned_abs();
        let style = if distance <= 1 {
            accent()
        } else if distance <= 3 {
            text_style()
        } else {
            base
        };
        if have_pending && style == pending_style {
            pending.push(ch);
        } else {
            if have_pending {
                spans.push(Span::styled(std::mem::take(&mut pending), pending_style));
            }
            pending.push(ch);
            pending_style = style;
            have_pending = true;
        }
    }
    if have_pending {
        spans.push(Span::styled(pending, pending_style));
    }
    spans
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

fn proposed_plan_lines(markdown: &str, width: u16) -> Vec<Line<'static>> {
    let plan_lines = markdown_cell_lines(
        markdown,
        width.saturating_sub(2).max(1),
        DisplayMode::Active,
    )
    .into_iter()
    .map(|line| {
        let mut spans = vec![Span::styled("  ".to_string(), muted())];
        spans.extend(line.spans);
        Line::from(spans)
    })
    .collect::<Vec<_>>();
    let mut out = vec![Line::from(Span::styled("Proposed Plan", accent()))];
    out.extend(plan_lines);
    out
}

fn wrap_with_prefix(
    text: &str,
    width: usize,
    initial_prefix: Span<'static>,
    subsequent_prefix: Span<'static>,
    style: Style,
) -> Vec<Line<'static>> {
    let initial_width = display_width(initial_prefix.content.as_ref());
    let subsequent_width = display_width(subsequent_prefix.content.as_ref());
    let first_width = (width.saturating_sub(initial_width)).max(1) as u16;
    let next_width = (width.saturating_sub(subsequent_width)).max(1) as u16;
    let mut out = Vec::new();
    for (idx, wrapped) in wrap_plain(text, first_width) {
        let prefix = if idx == 0 {
            initial_prefix.clone()
        } else {
            subsequent_prefix.clone()
        };
        let available = if idx == 0 { first_width } else { next_width };
        for (nested_idx, nested) in wrap_plain(&wrapped, available) {
            let prefix = if nested_idx == 0 {
                prefix.clone()
            } else {
                subsequent_prefix.clone()
            };
            out.push(Line::from(vec![prefix, Span::styled(nested, style)]));
        }
    }
    if out.is_empty() {
        out.push(Line::from(vec![
            initial_prefix,
            Span::styled(String::new(), style),
        ]));
    }
    out
}

fn request_user_input_lines(
    request: &PendingRequestUserInput,
    state: &RequestUserInputState,
    width: u16,
) -> Vec<Line<'static>> {
    let answered = request_user_input_answered_count(request, state);
    let mut out = vec![Line::from(Span::styled(
        format!("Questions {answered}/{} answered", request.questions.len()),
        accent(),
    ))];
    for (idx, question) in request.questions.iter().enumerate() {
        let current = idx == state.current_idx;
        let question_marker = if current { ">" } else { " " };
        let label = request_user_input_question_label(question);
        let unanswered = if request_user_input_answered(question, state, idx) {
            ""
        } else {
            " (unanswered)"
        };
        let prefix = format!("{question_marker} {}. ", idx + 1);
        out.extend(wrap_with_prefix(
            &format!("{label}: {}{unanswered}", question.question),
            width as usize,
            Span::styled(prefix, user_prompt_accent()),
            Span::styled("   ".to_string(), muted()),
            text_style(),
        ));
        if let Some(options) = question.options.as_ref() {
            for (option_idx, option) in options.iter().enumerate() {
                let marker = request_user_input_option_marker(state, idx, option_idx, current);
                out.extend(wrap_with_prefix(
                    &format!(
                        "{marker} {}. {} - {}",
                        option_idx + 1,
                        option.label,
                        option.description
                    ),
                    width as usize,
                    Span::styled("   ".to_string(), muted()),
                    Span::styled("     ".to_string(), muted()),
                    muted(),
                ));
            }
            if question.is_other {
                let other_idx = options.len();
                let marker = request_user_input_option_marker(state, idx, other_idx, current);
                out.extend(wrap_with_prefix(
                    &format!(
                        "{marker} {}. {REQUEST_USER_INPUT_OTHER_LABEL}",
                        other_idx + 1
                    ),
                    width as usize,
                    Span::styled("   ".to_string(), muted()),
                    Span::styled("     ".to_string(), muted()),
                    muted(),
                ));
            }
        }
        if let Some(note) = request_user_input_note_for_display(question, state, idx) {
            out.extend(wrap_with_prefix(
                &format!("note: {note}"),
                width as usize,
                Span::styled("   ".to_string(), muted()),
                Span::styled("     ".to_string(), muted()),
                muted(),
            ));
        }
    }
    if state.confirm_unanswered {
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(
            "Submit with unanswered questions?",
            accent(),
        )));
        for (idx, (label, description)) in [
            ("Proceed", "Submit with empty answers."),
            ("Go back", "Return to the first unanswered question."),
        ]
        .iter()
        .enumerate()
        {
            let marker = if state.confirm_selected == idx {
                ">"
            } else {
                " "
            };
            out.extend(wrap_with_prefix(
                &format!("{marker} {}. {label} - {description}", idx + 1),
                width as usize,
                Span::styled("   ".to_string(), muted()),
                Span::styled("     ".to_string(), muted()),
                muted(),
            ));
        }
    }
    out.push(Line::from(Span::styled(
        request_user_input_footer_hint(state),
        muted(),
    )));
    out
}

fn request_user_input_plain_lines(
    request: &PendingRequestUserInput,
    state: &RequestUserInputState,
) -> Vec<String> {
    let answered = request_user_input_answered_count(request, state);
    let mut out = vec![format!(
        "Questions {answered}/{} answered",
        request.questions.len()
    )];
    for (idx, question) in request.questions.iter().enumerate() {
        let label = request_user_input_question_label(question);
        out.push(format!("{label}: {}", question.question));
        if let Some(options) = question.options.as_ref() {
            for (idx, option) in options.iter().enumerate() {
                out.push(format!(
                    "{}. {} - {}",
                    idx + 1,
                    option.label,
                    option.description
                ));
            }
            if question.is_other {
                out.push(format!(
                    "{}. {REQUEST_USER_INPUT_OTHER_LABEL}",
                    options.len() + 1
                ));
            }
        }
        if let Some(note) = request_user_input_note_for_display(question, state, idx) {
            out.push(format!("note: {note}"));
        }
    }
    out
}

fn request_user_input_question_label(question: &RequestUserInputQuestion) -> String {
    if question.header.trim().is_empty() {
        format!("[{}]", question.id)
    } else {
        format!("{} [{}]", question.header, question.id)
    }
}

fn request_user_input_answered_count(
    request: &PendingRequestUserInput,
    state: &RequestUserInputState,
) -> usize {
    request
        .questions
        .iter()
        .enumerate()
        .filter(|(idx, question)| request_user_input_answered(question, state, *idx))
        .count()
}

fn request_user_input_answered(
    question: &RequestUserInputQuestion,
    state: &RequestUserInputState,
    idx: usize,
) -> bool {
    let Some(answer) = state.answers.get(idx) else {
        return false;
    };
    if !answer.answer_committed {
        return false;
    }
    let has_options = question
        .options
        .as_ref()
        .is_some_and(|options| !options.is_empty());
    if has_options {
        answer.committed_option.is_some()
    } else {
        !answer.notes.trim().is_empty()
    }
}

fn request_user_input_option_marker(
    state: &RequestUserInputState,
    question_idx: usize,
    option_idx: usize,
    current: bool,
) -> &'static str {
    let Some(answer) = state.answers.get(question_idx) else {
        return " ";
    };
    if current
        && state.focus == RequestUserInputFocus::Options
        && answer.option_cursor == Some(option_idx)
    {
        ">"
    } else if answer.answer_committed && answer.committed_option == Some(option_idx) {
        "*"
    } else {
        " "
    }
}

fn request_user_input_note_for_display(
    question: &RequestUserInputQuestion,
    state: &RequestUserInputState,
    idx: usize,
) -> Option<String> {
    let answer = state.answers.get(idx)?;
    let note = answer.notes.trim();
    if note.is_empty() {
        return None;
    }
    if question.is_secret {
        Some("******".to_string())
    } else {
        Some(note.to_string())
    }
}

fn request_user_input_footer_hint(state: &RequestUserInputState) -> &'static str {
    if state.confirm_unanswered {
        "Enter:confirm | Up/Down:choose | Esc:go back"
    } else if state.focus == RequestUserInputFocus::Notes {
        "Enter:save notes | Tab/Esc:options | Ctrl+C:interrupt"
    } else {
        "Enter:select | Tab:notes | 1-9:select | Backspace:clear | Esc:interrupt"
    }
}

fn split_proposed_plan_blocks(text: &str) -> (String, Option<String>) {
    let mut visible = String::new();
    let mut latest_plan = None;
    let mut current_plan = String::new();
    let mut in_plan = false;
    let mut saw_plan = false;

    for line in text.split_inclusive('\n') {
        let line_without_newline = line.strip_suffix('\n').unwrap_or(line);
        let slug = line_without_newline.trim_start().trim_end();
        if !in_plan && slug == "<proposed_plan>" {
            in_plan = true;
            saw_plan = true;
            current_plan.clear();
            continue;
        }
        if in_plan && slug == "</proposed_plan>" {
            in_plan = false;
            latest_plan = Some(current_plan.clone());
            continue;
        }
        if in_plan {
            current_plan.push_str(line);
        } else {
            visible.push_str(line);
        }
    }

    if in_plan && saw_plan {
        latest_plan = Some(current_plan);
    }
    (visible, latest_plan)
}

fn collaboration_mode_from_events(events: &[EventRecord]) -> CollaborationModeKind {
    events
        .iter()
        .rev()
        .find_map(|event| {
            if event.event_type != "session.collaboration_mode" {
                return None;
            }
            match event
                .payload
                .get("mode")
                .and_then(serde_json::Value::as_str)
            {
                Some("plan") => Some(CollaborationModeKind::Plan),
                Some("default") => Some(CollaborationModeKind::Default),
                _ => None,
            }
        })
        .unwrap_or(CollaborationModeKind::Default)
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
    if looks_like_url_token(token) || looks_like_absolute_path_token(token) {
        link()
    } else {
        path_reference()
    }
}

fn looks_like_url_token(token: &str) -> bool {
    token.starts_with("http://") || token.starts_with("https://") || token.starts_with("file://")
}

fn looks_like_absolute_path_token(token: &str) -> bool {
    token.starts_with('/')
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
    if key == "text" {
        if let Some(text) = user_input_display_text_from_payload(&event.payload) {
            return Some(text);
        }
    }
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

fn tool_failed_lines(event: &EventRecord) -> Vec<String> {
    let name = payload_string(event, "name").unwrap_or_else(|| "tool".to_string());
    let Some(diagnosis) = event
        .payload
        .get("diagnosis")
        .filter(|value| value.is_object())
    else {
        let error = payload_string(event, "error").unwrap_or_else(|| "tool failed".to_string());
        return vec![format!("{name} failed: {}", friendly_error_message(&error))];
    };

    let mut lines = vec![format!("{name} failed")];
    if let Some(summary) = diagnosis_text(diagnosis, "summary") {
        lines.push(summary);
    }
    if let Some(what_happened) = diagnosis_text(diagnosis, "what_happened") {
        lines.push(format!("What happened: {what_happened}"));
    }
    if let Some(next_step) = diagnosis_text(diagnosis, "next_step") {
        lines.push(format!("Next: {next_step}"));
    }
    if let Some(error) = payload_string(event, "error") {
        let detail = last_error_line(&error);
        if !detail.is_empty() {
            lines.push(format!("Details: {}", truncate_inline(&detail, 180)));
        }
    }
    lines
}

fn diagnosis_text(diagnosis: &serde_json::Value, key: &str) -> Option<String> {
    diagnosis
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn last_error_line(error: &str) -> String {
    error
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(error.trim())
        .to_string()
}

fn truncate_inline(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut out = value
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
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

#[derive(Clone, Debug)]
struct ResultFileDisplay {
    file_path: String,
    bytes: Option<u64>,
    mime: Option<String>,
}

fn session_done_result_text(event: &EventRecord) -> Option<String> {
    payload_string(event, "result").map(|result| normalize_result_text(&result))
}

fn session_done_result_file(
    event: &EventRecord,
    state: &WorkbenchState,
) -> Option<ResultFileDisplay> {
    event.payload.get("result_file")?;
    let file_path = payload_string(event, "result_file_path")
        .or_else(|| resolved_result_file_path(event, state).map(|path| path.display().to_string()))
        .or_else(|| payload_string(event, "result_file"))
        .unwrap_or_else(|| "<unknown>".to_string());
    let bytes = event
        .payload
        .get("result_file_bytes")
        .and_then(serde_json::Value::as_u64);
    let mime = payload_string(event, "result_file_mime");

    Some(ResultFileDisplay {
        file_path,
        bytes,
        mime,
    })
}

fn result_file_lines(
    file_path: &str,
    bytes: Option<u64>,
    mime: Option<&str>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Saved result file", text_style())),
        Line::from(""),
    ];
    let path_style = result_file_path_style(file_path);
    lines.extend(
        wrap_plain(file_path, width)
            .into_iter()
            .map(|(_, line)| Line::from(Span::styled(line, path_style))),
    );
    if let Some(metadata) = result_file_metadata(bytes, mime) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(metadata, muted())));
    }
    lines
}

fn result_file_plain_lines(file_path: &str, bytes: Option<u64>, mime: Option<&str>) -> Vec<String> {
    let mut lines = vec![
        "Saved result file".to_string(),
        String::new(),
        file_path.to_string(),
    ];
    if let Some(metadata) = result_file_metadata(bytes, mime) {
        lines.push(String::new());
        lines.push(metadata);
    }
    lines
}

fn result_file_path_style(file_path: &str) -> Style {
    if file_path.starts_with('/') || file_path.starts_with("file://") {
        link()
    } else {
        path_reference()
    }
}

fn result_file_metadata(bytes: Option<u64>, mime: Option<&str>) -> Option<String> {
    let mime = mime.and_then(display_result_file_mime);
    match (bytes, mime) {
        (Some(bytes), Some(mime)) => Some(format!("{} · {mime}", format_bytes(bytes))),
        (Some(bytes), None) => Some(format_bytes(bytes)),
        (None, Some(mime)) => Some(mime.to_string()),
        (None, None) => None,
    }
}

fn display_result_file_mime(mime: &str) -> Option<&str> {
    (mime != "application/octet-stream").then_some(mime)
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

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn tool_image_label(event: &EventRecord, state: &WorkbenchState) -> String {
    let image = event.payload.get("image");
    let path = image
        .and_then(|image| image.get("path"))
        .and_then(serde_json::Value::as_str);
    let label = image
        .and_then(|image| image.get("label"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty());
    let source = image
        .and_then(|image| image.get("source"))
        .and_then(serde_json::Value::as_str);
    let target = label
        .map(ToOwned::to_owned)
        .or_else(|| path.map(|path| display_path(path, state)));
    match source {
        Some("screenshot") => target
            .map(|target| format!("screenshot: {target}"))
            .unwrap_or_else(|| "screenshot captured".to_string()),
        Some("emit_image") => target
            .map(|target| format!("attached image: {target}"))
            .unwrap_or_else(|| "attached image".to_string()),
        _ => event
            .payload
            .get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| name == "browser_script")
            .then(|| {
                target
                    .clone()
                    .map(|target| format!("browser image: {target}"))
                    .unwrap_or_else(|| "browser image attached".to_string())
            })
            .or_else(|| target.map(|target| format!("image: {target}")))
            .unwrap_or_else(|| "received image artifact".to_string()),
    }
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

    fn test_workbench_state() -> WorkbenchState {
        WorkbenchState {
            setup_complete: true,
            current_session: None,
            task: None,
            result: None,
            failure: None,
            activity: Vec::new(),
            transcript: Vec::new(),
            browser: Default::default(),
            telemetry: Default::default(),
            history: Vec::new(),
        }
    }

    #[test]
    fn prompt_output_pairs_use_one_blank_line() {
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
        assert_eq!(line_text(&lines[2]), "Please open Chrome first.");
    }

    #[test]
    fn prompt_streaming_output_uses_one_blank_line() {
        let prompt = TranscriptNode {
            id: "prompt".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Prompt {
                text: "whats up".to_string(),
                followup: false,
            },
        };
        let answer = TranscriptNode {
            id: "answer".to_string(),
            seq: 2,
            revision: 2,
            kind: TranscriptKind::StreamingAssistant {
                markdown: "Not much. I'm ready to work.".to_string(),
            },
        };

        let lines = cells_to_lines([&prompt, &answer].into_iter(), 80, DisplayMode::Active);

        assert_eq!(line_text(&lines[0]), "> whats up");
        assert_eq!(line_text(&lines[1]), "");
        assert_eq!(line_text(&lines[2]), "Not much. I'm ready to work.");
    }

    #[test]
    fn streaming_with_pending_status_keeps_prompt_separator() {
        let active = TranscriptNode {
            id: "active-stack".to_string(),
            seq: 3,
            revision: 3,
            kind: TranscriptKind::Stack {
                nodes: vec![
                    TranscriptNode {
                        id: "stream".to_string(),
                        seq: 2,
                        revision: 2,
                        kind: TranscriptKind::StreamingAssistant {
                            markdown: "Not much. I'm ready to work.".to_string(),
                        },
                    },
                    TranscriptNode {
                        id: "status".to_string(),
                        seq: 3,
                        revision: 3,
                        kind: TranscriptKind::PendingStatus {
                            status: "Thinking...".to_string(),
                            detail: None,
                        },
                    },
                ],
            },
        };
        let model = TranscriptModel {
            session_id: "session".to_string(),
            committed: Vec::new(),
            terminal_committed: Vec::new(),
            active: Some(active),
            last_event_seq: 3,
            revision: 3,
            live_phase: 0,
        };

        let lines = active_viewport_lines(Some(&model), 80, 10);

        assert_eq!(line_text(&lines[0]), "");
        assert_eq!(line_text(&lines[1]), "Not much. I'm ready to work.");
        assert_eq!(line_text(&lines[2]), "• Thinking...");
    }

    #[test]
    fn active_streaming_moves_separator_with_emitted_prefix() {
        fn model_for(markdown: &str) -> TranscriptModel {
            TranscriptModel {
                session_id: "session".to_string(),
                committed: Vec::new(),
                terminal_committed: Vec::new(),
                active: Some(TranscriptNode {
                    id: "stream".to_string(),
                    seq: 1,
                    revision: 1,
                    kind: TranscriptKind::StreamingAssistant {
                        markdown: markdown.to_string(),
                    },
                }),
                last_event_seq: 1,
                revision: 1,
                live_phase: 0,
            }
        }

        let first = model_for("Not much. I'm ready to work.");
        let first_native_stream = active_streaming_lines(Some(&first), 80);
        assert_eq!(first_native_stream.len(), 1);

        let first_viewport = active_viewport_lines_with_stream_skip(Some(&first), 80, 100, 0);
        assert_eq!(line_text(&first_viewport[0]), "");
        assert_eq!(
            line_text(&first_viewport[1]),
            "Not much. I'm ready to work."
        );

        let second = model_for("Not much. I'm ready to work.\n\nSend me the command.");
        let second_native_stream = active_streaming_lines(Some(&second), 80);
        let emitted_lines = second_native_stream.len().saturating_sub(1);
        let second_viewport =
            active_viewport_lines_with_stream_skip(Some(&second), 80, 100, emitted_lines);

        assert_eq!(line_text(&second_viewport[0]), "Send me the command.");
    }

    #[test]
    fn prompt_tool_rows_use_one_blank_line() {
        let prompt = TranscriptNode {
            id: "prompt".to_string(),
            seq: 1,
            revision: 1,
            kind: TranscriptKind::Prompt {
                text: "whats this repo about".to_string(),
                followup: false,
            },
        };
        let run = TranscriptNode {
            id: "run".to_string(),
            seq: 2,
            revision: 2,
            kind: TranscriptKind::Timeline {
                group: "run".to_string(),
                lines: vec!["pwd && rg --files".to_string()],
                style: NodeStyle::Muted,
            },
        };

        let lines = cells_to_lines([&prompt, &run].into_iter(), 80, DisplayMode::Scrollback);

        assert_eq!(line_text(&lines[0]), "> whats this repo about");
        assert_eq!(line_text(&lines[1]), "");
        assert_eq!(line_text(&lines[2]), "• run");
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
    fn browser_script_failures_render_compact_diagnosis() {
        let event = EventRecord {
            seq: 7,
            id: "event-7".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.failed".to_string(),
            payload: serde_json::json!({
                "name": "browser_script",
                "error": "Traceback (most recent call last):\nRuntimeError: read CDP Runtime.evaluate: IO error",
                "diagnosis": {
                    "summary": "Browser is still connected; the same page should still be usable.",
                    "what_happened": "A CDP read timed out while waiting for Chrome.",
                    "next_step": "Continue on the same page with a smaller chunk.",
                    "browser_usable": true,
                    "page_usable": true,
                    "error_kind": "cdp-read-timeout"
                }
            }),
        };

        let lines = tool_failed_lines(&event);

        assert_eq!(lines[0], "browser_script failed");
        assert!(lines.iter().any(|line| line.contains("same page")));
        assert!(lines
            .iter()
            .any(|line| line.contains("Next: Continue on the same page")));
        assert!(lines
            .iter()
            .any(|line| line.contains("Details: RuntimeError")));
        assert!(!lines
            .iter()
            .any(|line| line.contains("Traceback (most recent call last)")));
    }

    #[test]
    fn browser_script_summary_hides_raw_page_info_text() {
        let event = EventRecord {
            seq: 8,
            id: "event-8".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.output".to_string(),
            payload: serde_json::json!({
                "name": "browser_script",
                "text": "{'url': 'https://login.gusto.com/realms/zenpayroll/protocol/openid-connect/auth?client_id=zenpayroll&device_uuid=secret', 'title': 'Gusto Login - Payroll, Benefits, HR | Gusto', 'readyState': 'complete', 'target': {'targetId': 'B6CDD9676BD0503360290CD36A12A4D1'}}",
                "summary": [{
                    "kind": "page",
                    "url": "https://login.gusto.com/realms/zenpayroll/protocol/openid-connect/auth?client_id=zenpayroll&device_uuid=secret",
                    "title": "Gusto Login - Payroll, Benefits, HR | Gusto"
                }],
                "images": [{"path": "/tmp/page.png"}],
                "artifacts": [{"path": "/tmp/result.json"}]
            }),
        };

        let node = tool_output_node(&event).expect("tool output node");
        let text = node
            .display_lines(120, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("• browser"), "{text}");
        assert!(
            text.contains(
                "page: login.gusto.com/realms/zenpayroll/protocol/openid-connect/auth?..."
            ),
            "{text}"
        );
        assert!(text.contains("Gusto Login - Payroll"), "{text}");
        assert!(text.contains("1 image artifact"), "{text}");
        assert!(text.contains("1 file artifact"), "{text}");
        assert!(!text.contains("targetId"), "{text}");
        assert!(!text.contains("client_id=zenpayroll"), "{text}");
        assert!(!text.contains("readyState"), "{text}");
    }

    #[test]
    fn browser_script_screenshot_image_label_names_capture_source() {
        let state = test_workbench_state();
        let event = EventRecord {
            seq: 10,
            id: "event-10".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.image".to_string(),
            payload: serde_json::json!({
                "name": "browser_script",
                "image": {
                    "path": "/tmp/hn_front_page.png",
                    "mime_type": "image/png",
                    "label": "hn_front_page",
                    "source": "screenshot"
                }
            }),
        };

        assert_eq!(
            tool_image_label(&event, &state),
            "screenshot: hn_front_page"
        );
    }

    #[test]
    fn browser_script_emit_image_label_names_attachment_source() {
        let state = test_workbench_state();
        let event = EventRecord {
            seq: 11,
            id: "event-11".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.image".to_string(),
            payload: serde_json::json!({
                "name": "browser_script",
                "image": {
                    "path": "/tmp/diagnostic.png",
                    "mime_type": "image/png",
                    "label": "diagnostic",
                    "source": "emit_image"
                }
            }),
        };

        assert_eq!(
            tool_image_label(&event, &state),
            "attached image: diagnostic"
        );
    }

    #[test]
    fn legacy_browser_script_image_label_stays_specific() {
        let state = test_workbench_state();
        let event = EventRecord {
            seq: 12,
            id: "event-12".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.image".to_string(),
            payload: serde_json::json!({
                "name": "browser_script",
                "image": {
                    "path": "/tmp/latest_screenshot.png",
                    "mime_type": "image/png",
                    "label": "latest_screenshot"
                }
            }),
        };

        assert_eq!(
            tool_image_label(&event, &state),
            "browser image: latest_screenshot"
        );
    }

    #[test]
    fn browser_script_summary_suppresses_running_transport_text() {
        let event = EventRecord {
            seq: 9,
            id: "event-9".to_string(),
            session_id: "session".to_string(),
            ts_ms: 0,
            event_type: "tool.output".to_string(),
            payload: serde_json::json!({
                "name": "browser_script",
                "text": "browser_script is still running.\nrun_id: bs-secret\nNext: observe this run again.",
                "summary": [{
                    "kind": "inspected",
                    "message": "Sampled 5 comments from current thread"
                }]
            }),
        };

        let node = tool_output_node(&event).expect("tool output node");
        let text = node
            .display_lines(120, DisplayMode::Scrollback)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            text.contains("Sampled 5 comments from current thread"),
            "{text}"
        );
        assert!(!text.contains("inspected Sampled"), "{text}");
        assert!(!text.contains("browser_script is still running"), "{text}");
        assert!(!text.contains("bs-secret"), "{text}");
    }

    #[test]
    fn browser_script_raw_text_fallback_is_bounded() {
        let line = format!(
            "{{'url': 'https://login.example.test/realms/acme/protocol/openid-connect/auth?client_id=zenpayroll&state={}', 'target': {{'targetId': '{}'}}}}",
            "x".repeat(240),
            "y".repeat(240)
        );

        let preview = browser_script_text_preview_lines(&line);

        assert_eq!(preview.len(), 1);
        assert!(preview[0].chars().count() <= 180, "{preview:?}");
        assert!(preview[0].ends_with("..."), "{preview:?}");
    }

    #[test]
    fn browser_script_running_text_fallback_hides_run_id() {
        let preview = browser_script_text_preview_lines(
            "browser_script is still running.\nNo new output in the last 50 ms.\nrun_id: bs-secret\nNext: observe this run again.",
        );

        assert!(preview.is_empty(), "{preview:?}");
        assert!(!preview.join("\n").contains("bs-secret"));
    }

    #[test]
    fn browser_script_partial_text_fallback_drops_runtime_instructions() {
        let preview = browser_script_text_preview_lines(
            "chunk one\n\nbrowser_script is still running.\nrun_id: bs-secret\nNext: observe this run again.",
        );

        assert_eq!(preview, vec!["chunk one"]);
        assert!(!preview.join("\n").contains("bs-secret"));
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
            live_phase: 0,
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
    fn result_file_lines_render_full_path_as_clickable_text() {
        let path = "/tmp/browser use/artifacts/session/result.json";
        let lines = result_file_lines(path, Some(2048), Some("application/octet-stream"), 120);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(text.contains("Saved result file"), "{text}");
        assert!(text.contains(path), "{text}");
        assert!(text.contains("2.0 KB"), "{text}");
        assert!(!text.contains("file://"), "{text}");
        assert!(!text.contains("application/octet-stream"), "{text}");
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.as_ref() == path && span.style == link())
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
