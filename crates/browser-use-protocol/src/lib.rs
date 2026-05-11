use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SessionMeta {
    pub id: String,
    pub parent_id: Option<String>,
    pub cwd: String,
    pub artifact_root: String,
    pub status: SessionStatus,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Created,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Created | Self::Running)
    }
}

impl std::str::FromStr for SessionStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "created" => Ok(Self::Created),
            "running" => Ok(Self::Running),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(format!("unknown session status: {other}")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct EventRecord {
    pub seq: i64,
    pub id: String,
    pub session_id: String,
    pub ts_ms: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ArtifactMeta {
    pub id: String,
    pub session_id: String,
    pub event_seq: Option<i64>,
    pub kind: String,
    pub path: String,
    pub mime: Option<String>,
    pub bytes: Option<i64>,
    pub created_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolImage {
    pub label: Option<String>,
    pub path: String,
    pub mime_type: String,
    pub detail: String,
    pub order: i64,
    pub ts_ms: i64,
    pub url: Option<String>,
    pub title: Option<String>,
    pub viewport: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub ok: bool,
    pub text: String,
    pub data: Value,
    pub images: Vec<ToolImage>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ModelUsage {
    pub input_tokens: Option<i64>,
    pub input_cached_tokens: Option<i64>,
    pub input_cache_creation_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub input_cost_usd: Option<f64>,
    pub input_cached_cost_usd: Option<f64>,
    pub input_cache_creation_cost_usd: Option<f64>,
    pub output_cost_usd: Option<f64>,
    pub cost_usd: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    TextDelta { text: String },
    ToolCall { call: ToolCall },
    Usage { usage: ModelUsage },
    Done,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowserSummary {
    pub backend: String,
    pub status: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub live_url: Option<String>,
    pub tabs: Option<i64>,
    pub viewport: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TelemetrySummary {
    pub trace_id: Option<String>,
    pub backend: Option<String>,
    pub endpoint: Option<String>,
    pub failure: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRow {
    pub session_id: String,
    pub task: String,
    pub status: SessionStatus,
    pub updated_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkbenchState {
    pub setup_complete: bool,
    pub current_session: Option<SessionMeta>,
    pub task: Option<String>,
    pub result: Option<String>,
    pub failure: Option<String>,
    pub activity: Vec<String>,
    pub transcript: Vec<TranscriptTurn>,
    pub browser: BrowserSummary,
    pub telemetry: TelemetrySummary,
    pub history: Vec<HistoryRow>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptTurn {
    pub prompt: String,
    pub is_followup: bool,
    pub activity: Vec<String>,
    pub result: Option<String>,
    pub failure: Option<String>,
}

pub fn task_from_events(events: &[EventRecord]) -> Option<String> {
    events.iter().find_map(|event| {
        if event.event_type == "session.input" {
            event
                .payload
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(ToOwned::to_owned)
        } else {
            None
        }
    })
}

pub fn transcript_from_events(events: &[EventRecord]) -> Vec<TranscriptTurn> {
    let mut starts = Vec::new();
    for (idx, event) in events.iter().enumerate() {
        let is_followup = match event.event_type.as_str() {
            "session.input" => false,
            "session.followup" => true,
            _ => continue,
        };
        let Some(prompt) = event
            .payload
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        starts.push((idx, prompt.to_string(), is_followup));
    }

    starts
        .iter()
        .enumerate()
        .map(|(turn_idx, (event_idx, prompt, is_followup))| {
            let next_idx = starts
                .get(turn_idx + 1)
                .map(|(idx, _, _)| *idx)
                .unwrap_or(events.len());
            let segment = events
                .get(event_idx.saturating_add(1)..next_idx)
                .unwrap_or_default();
            TranscriptTurn {
                prompt: prompt.clone(),
                is_followup: *is_followup,
                activity: activity_from_events(segment),
                result: turn_result_from_events(segment),
                failure: turn_failure_from_events(segment),
            }
        })
        .collect()
}

pub fn result_from_events(events: &[EventRecord]) -> Option<String> {
    events
        .iter()
        .rev()
        .find_map(|event| match event.event_type.as_str() {
            "session.done" => event
                .payload
                .get("result")
                .and_then(Value::as_str)
                .map(clean_result_text),
            "agent.completed" => event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("result"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|result| !result.is_empty())
                .map(clean_result_text),
            _ => None,
        })
}

fn clean_result_text(text: &str) -> String {
    let mut cleaned = text.trim_end().to_string();
    loop {
        let chars = cleaned.chars().collect::<Vec<_>>();
        let len = chars.len();
        let Some(repeated_len) = (24..=len / 2).rev().find(|candidate| {
            let start = len.saturating_sub(candidate * 2);
            chars[start..start + candidate] == chars[start + candidate..]
        }) else {
            break;
        };
        let keep = len.saturating_sub(repeated_len);
        cleaned = chars[..keep]
            .iter()
            .collect::<String>()
            .trim_end()
            .to_string();
    }
    cleaned
}

pub fn helper_failure_from_events(events: &[EventRecord]) -> Option<String> {
    events
        .iter()
        .rev()
        .find_map(|event| match event.event_type.as_str() {
            "agent.failed" => event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("error"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|error| !error.is_empty())
                .map(ToOwned::to_owned),
            "agent.cancelled" => event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .map(ToOwned::to_owned),
            _ => None,
        })
}

pub fn turn_failure_from_events(events: &[EventRecord]) -> Option<String> {
    failure_from_events(events).or_else(|| helper_failure_from_events(events))
}

pub fn turn_result_from_events(events: &[EventRecord]) -> Option<String> {
    result_from_events(events)
}

pub fn failure_from_events(events: &[EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|event| {
        if event.event_type == "session.failed" {
            event
                .payload
                .get("error")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        } else {
            None
        }
    })
}

pub fn browser_summary_from_events(
    events: &[EventRecord],
    backend: impl Into<String>,
) -> BrowserSummary {
    let mut summary = BrowserSummary {
        backend: backend.into(),
        status: "not connected".to_string(),
        title: None,
        url: None,
        live_url: None,
        tabs: None,
        viewport: None,
    };
    for event in events {
        match event.event_type.as_str() {
            "browser.connected" | "browser.reconnected" | "browser.target_changed" => {
                summary.status = "connected".to_string();
                if let Some(url) = event.payload.get("url").and_then(Value::as_str) {
                    summary.url = Some(url.to_string());
                }
                if let Some(title) = event.payload.get("title").and_then(Value::as_str) {
                    summary.title = Some(title.to_string());
                }
            }
            "browser.disconnected" => {
                summary.status = "disconnected".to_string();
            }
            "browser.live_url" => {
                summary.status = "connected".to_string();
                summary.live_url = event
                    .payload
                    .get("live_url")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
            }
            "browser.page" | "browser.state" => {
                if let Some(status) = event.payload.get("status").and_then(Value::as_str) {
                    summary.status = status.to_string();
                }
                if let Some(url) = event.payload.get("url").and_then(Value::as_str) {
                    summary.url = Some(url.to_string());
                    summary.status = "connected".to_string();
                }
                if let Some(title) = event.payload.get("title").and_then(Value::as_str) {
                    summary.title = Some(title.to_string());
                }
                if let Some(tabs) = event
                    .payload
                    .get("tabs")
                    .or_else(|| event.payload.get("tab_count"))
                    .and_then(Value::as_i64)
                {
                    summary.tabs = Some(tabs);
                }
                if let Some(viewport) = viewport_label_from_payload(&event.payload) {
                    summary.viewport = Some(viewport);
                }
            }
            _ => {}
        }
    }
    summary
}

pub fn telemetry_summary_from_events(events: &[EventRecord]) -> TelemetrySummary {
    let mut summary = TelemetrySummary::default();
    for event in events {
        match event.event_type.as_str() {
            "telemetry.trace" => {
                summary.trace_id = event
                    .payload
                    .get("trace_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                summary.backend = event
                    .payload
                    .get("backend")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                summary.endpoint = event
                    .payload
                    .get("endpoint")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                summary.failure = None;
            }
            "telemetry.failed" => {
                summary.failure = event
                    .payload
                    .get("error")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| Some("Laminar exporter setup failed".to_string()));
                summary.trace_id = None;
                summary.backend = None;
                summary.endpoint = None;
            }
            _ => {}
        }
    }
    summary
}

fn viewport_label_from_payload(payload: &Value) -> Option<String> {
    if let Some(label) = payload.get("viewport").and_then(Value::as_str) {
        return (!label.trim().is_empty()).then(|| label.trim().to_string());
    }
    let viewport = payload.get("viewport").unwrap_or(payload);
    let width = viewport
        .get("w")
        .or_else(|| viewport.get("width"))
        .and_then(Value::as_i64)?;
    let height = viewport
        .get("h")
        .or_else(|| viewport.get("height"))
        .and_then(Value::as_i64)?;
    Some(format!("{width} x {height}"))
}

pub fn activity_from_events(events: &[EventRecord]) -> Vec<String> {
    let mut activity = Vec::new();
    for event in events {
        match event.event_type.as_str() {
            "browser.connected" => push_activity(&mut activity, "browser connected"),
            "browser.reconnected" => push_activity(&mut activity, "browser reconnected"),
            "browser.target_changed" => push_activity(&mut activity, "browser target changed"),
            "browser.disconnected" => push_activity(&mut activity, "browser disconnected"),
            "browser.live_url" => push_activity(&mut activity, "connected live browser"),
            "browser.page" | "browser.state" => {
                if let Some(url) = event.payload.get("url").and_then(Value::as_str) {
                    push_activity(&mut activity, format!("browsing {}", compact_url(url)));
                }
            }
            "plan.updated" => push_activity(&mut activity, "updated plan"),
            "command.started" => {
                if let Some(cmd) = event.payload.get("cmd").and_then(Value::as_str) {
                    push_activity(&mut activity, format!("ran {}", truncate_activity(cmd, 72)));
                } else {
                    push_activity(&mut activity, "ran command");
                }
            }
            "command.finished" => {
                if event
                    .payload
                    .get("success")
                    .and_then(Value::as_bool)
                    .is_some_and(|success| !success)
                {
                    let code = event
                        .payload
                        .get("exit_code")
                        .and_then(Value::as_i64)
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    push_activity(&mut activity, format!("command failed with exit {code}"));
                }
            }
            "patch.file_changed" => {
                let kind = event
                    .payload
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("changed");
                let path = event
                    .payload
                    .get("path")
                    .and_then(Value::as_str)
                    .map(compact_path)
                    .unwrap_or_else(|| "file".to_string());
                push_activity(&mut activity, format!("{kind} {path}"));
            }
            "file.read" => {
                if let Some(path) = event.payload.get("path").and_then(Value::as_str) {
                    push_activity(&mut activity, format!("read {}", compact_path(path)));
                }
            }
            "file.search" => {
                let query = event
                    .payload
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("files");
                let matches = event
                    .payload
                    .get("matches")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                push_activity(
                    &mut activity,
                    format!("searched {query:?} ({matches} matches)"),
                );
            }
            "file.list" => push_activity(&mut activity, "listed files"),
            "agent.spawned" => push_activity(&mut activity, agent_started_text(&event.payload)),
            "agent.completed" => push_activity(&mut activity, "helper finished"),
            "agent.failed" => push_activity(&mut activity, "helper failed"),
            "agent.cancelled" => push_activity(&mut activity, "helper stopped"),
            _ => {}
        }
    }
    activity
}

fn push_activity(activity: &mut Vec<String>, item: impl Into<String>) {
    let item = item.into();
    if activity.last().is_some_and(|last| last == &item) {
        return;
    }
    activity.push(item);
}

fn compact_path(path: &str) -> String {
    let trimmed = path.trim();
    trimmed
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .filter(|tail| !tail.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

fn truncate_activity(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out = value
        .chars()
        .take(max.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

pub fn sanitized_agent_context_from_events(events: &[EventRecord]) -> Value {
    let browser = browser_summary_from_events(events, "parent browser");
    let mut activity = activity_from_events(events);
    if activity.len() > 8 {
        activity = activity.split_off(activity.len() - 8);
    }
    serde_json::json!({
        "task": task_from_events(events),
        "result": result_from_events(events),
        "failure": failure_from_events(events),
        "browser": {
            "status": browser.status,
            "title": browser.title,
            "url": browser.url,
            "live_url": browser.live_url,
        },
        "activity": activity,
    })
}

fn compact_url(url: &str) -> String {
    url.trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))
        .unwrap_or_else(|| url.trim())
        .trim_end_matches('/')
        .to_string()
}

fn agent_started_text(payload: &Value) -> String {
    let label = payload
        .get("nickname")
        .and_then(Value::as_str)
        .or_else(|| payload.get("role").and_then(Value::as_str))
        .unwrap_or("helper");
    format!("started {label} helper")
}

pub fn project_workbench(
    sessions: &[SessionMeta],
    events_for_current: &[EventRecord],
    all_events: &[(String, Vec<EventRecord>)],
    selected_session_id: Option<&str>,
    browser_backend: impl Into<String>,
) -> WorkbenchState {
    let current_session = selected_session_id
        .and_then(|id| sessions.iter().find(|session| session.id == id))
        .cloned();

    let mut history = sessions
        .iter()
        .map(|session| {
            let events = all_events
                .iter()
                .find(|(id, _)| id == &session.id)
                .map(|(_, events)| events.as_slice())
                .unwrap_or_default();
            HistoryRow {
                session_id: session.id.clone(),
                task: task_from_events(events).unwrap_or_else(|| "untitled task".to_string()),
                status: session.status.clone(),
                updated_ms: session.updated_ms,
            }
        })
        .collect::<Vec<_>>();
    history.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));

    let result = if current_session
        .as_ref()
        .is_some_and(|session| session.status == SessionStatus::Done)
    {
        result_from_events(events_for_current)
    } else {
        None
    };
    let failure = if current_session
        .as_ref()
        .is_some_and(|session| session.status == SessionStatus::Failed)
    {
        failure_from_events(events_for_current)
    } else {
        None
    };

    WorkbenchState {
        setup_complete: false,
        current_session,
        task: task_from_events(events_for_current),
        result,
        failure,
        activity: activity_from_events(events_for_current),
        transcript: transcript_from_events(events_for_current),
        browser: browser_summary_from_events(events_for_current, browser_backend),
        telemetry: telemetry_summary_from_events(events_for_current),
        history,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_task_result_browser_and_activity_from_events() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "session.input".to_string(),
                payload: json!({"text": "Find flights"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "browser.page".to_string(),
                payload: json!({
                    "url": "https://example.com/",
                    "title": "Example",
                    "tabs": 2,
                    "viewport": {"w": 1440, "h": 900},
                }),
            },
            EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 3,
                event_type: "command.started".to_string(),
                payload: json!({"cmd": "cargo test -p browser-use-core"}),
            },
            EventRecord {
                seq: 4,
                id: "e4".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 4,
                event_type: "patch.file_changed".to_string(),
                payload: json!({"kind": "modified", "path": "/repo/src/main.rs"}),
            },
            EventRecord {
                seq: 5,
                id: "e5".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 5,
                event_type: "session.done".to_string(),
                payload: json!({"result": "Done"}),
            },
        ];
        assert_eq!(task_from_events(&events).as_deref(), Some("Find flights"));
        assert_eq!(result_from_events(&events).as_deref(), Some("Done"));
        let browser = browser_summary_from_events(&events, "local chrome");
        assert_eq!(browser.status, "connected");
        assert_eq!(browser.title.as_deref(), Some("Example"));
        assert_eq!(browser.tabs, Some(2));
        assert_eq!(browser.viewport.as_deref(), Some("1440 x 900"));
        assert_eq!(
            activity_from_events(&events),
            vec![
                "browsing example.com",
                "ran cargo test -p browser-use-core",
                "modified main.rs",
            ]
        );
    }

    #[test]
    fn result_projection_dedupes_repeated_full_text_delta_artifacts() {
        let answer = "Your callback has been scheduled.\n\nThey will call you tomorrow.";
        let events = vec![EventRecord {
            seq: 1,
            id: "e1".to_string(),
            session_id: "s1".to_string(),
            ts_ms: 1,
            event_type: "session.done".to_string(),
            payload: json!({"result": format!("{answer}{answer}")}),
        }];
        assert_eq!(result_from_events(&events).as_deref(), Some(answer));
    }

    #[test]
    fn projects_browser_identity_events() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "browser.connected".to_string(),
                payload: json!({
                    "target_id": "target-1",
                    "session_id": "session-1",
                    "url": "https://example.com/one",
                    "title": "One",
                }),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "browser.reconnected".to_string(),
                payload: json!({
                    "target_id": "target-1",
                    "session_id": "session-2",
                    "previous_session_id": "session-1",
                    "url": "https://example.com/two",
                    "title": "Two",
                    "stale_object_ids": true,
                }),
            },
            EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 3,
                event_type: "browser.target_changed".to_string(),
                payload: json!({
                    "target_id": "target-2",
                    "previous_target_id": "target-1",
                    "session_id": "session-3",
                    "url": "https://example.com/three",
                    "title": "Three",
                    "stale_object_ids": true,
                }),
            },
        ];
        let browser = browser_summary_from_events(&events, "browser use cloud");
        assert_eq!(browser.status, "connected");
        assert_eq!(browser.url.as_deref(), Some("https://example.com/three"));
        assert_eq!(browser.title.as_deref(), Some("Three"));
        assert_eq!(
            activity_from_events(&events),
            vec![
                "browser connected",
                "browser reconnected",
                "browser target changed",
            ]
        );
    }

    #[test]
    fn ignores_internal_python_tool_start_activity() {
        let events = vec![EventRecord {
            seq: 1,
            id: "e1".to_string(),
            session_id: "s1".to_string(),
            ts_ms: 1,
            event_type: "tool.started".to_string(),
            payload: json!({"name": "python", "call_id": "call-1"}),
        }];
        assert!(activity_from_events(&events).is_empty());
    }

    #[test]
    fn collapses_consecutive_duplicate_activity_pings() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "browser.state".to_string(),
                payload: json!({"url": "https://example.com/dashboard"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "browser.state".to_string(),
                payload: json!({"url": "https://example.com/dashboard"}),
            },
            EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 3,
                event_type: "browser.state".to_string(),
                payload: json!({"url": "https://example.com/settings"}),
            },
            EventRecord {
                seq: 4,
                id: "e4".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 4,
                event_type: "browser.state".to_string(),
                payload: json!({"url": "https://example.com/dashboard"}),
            },
        ];
        assert_eq!(
            activity_from_events(&events),
            vec![
                "browsing example.com/dashboard",
                "browsing example.com/settings",
                "browsing example.com/dashboard",
            ]
        );
    }

    #[test]
    fn projects_latest_telemetry_state_from_events() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "telemetry.failed".to_string(),
                payload: json!({"error": "bad endpoint"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "telemetry.trace".to_string(),
                payload: json!({
                    "trace_id": "abc123",
                    "backend": "laminar",
                    "endpoint": "https://api.lmnr.ai/v1/traces",
                }),
            },
        ];
        let telemetry = telemetry_summary_from_events(&events);
        assert_eq!(telemetry.trace_id.as_deref(), Some("abc123"));
        assert_eq!(telemetry.backend.as_deref(), Some("laminar"));
        assert_eq!(
            telemetry.endpoint.as_deref(),
            Some("https://api.lmnr.ai/v1/traces")
        );
        assert!(telemetry.failure.is_none());
    }

    #[test]
    fn does_not_select_history_without_an_explicit_current_session() {
        let sessions = vec![SessionMeta {
            id: "s1".to_string(),
            parent_id: None,
            cwd: "/tmp".to_string(),
            artifact_root: "/tmp/artifacts/s1".to_string(),
            status: SessionStatus::Done,
            created_ms: 1,
            updated_ms: 2,
        }];
        let state = project_workbench(&sessions, &[], &[], None, "local chrome");
        assert!(state.current_session.is_none());
        assert_eq!(state.history.len(), 1);
    }

    #[test]
    fn projects_agent_events_as_compact_activity() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "agent.spawned".to_string(),
                payload: json!({"child_session_id": "c1", "nickname": "checkout"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "agent.completed".to_string(),
                payload: json!({
                    "child_session_id": "c1",
                    "status": "done",
                    "payload": {"result": "checkout flow documented"},
                }),
            },
        ];
        assert_eq!(
            activity_from_events(&events),
            vec!["started checkout helper", "helper finished"]
        );
        assert_eq!(
            result_from_events(&events).as_deref(),
            Some("checkout flow documented"),
        );
    }

    #[test]
    fn sanitized_agent_context_omits_raw_tool_output() {
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "session.input".to_string(),
                payload: json!({"text": "Book a hotel"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "tool.output".to_string(),
                payload: json!({"text": "raw page dump with token SECRET"}),
            },
            EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 3,
                event_type: "browser.state".to_string(),
                payload: json!({"url": "https://example.com", "title": "Example"}),
            },
        ];
        let context = sanitized_agent_context_from_events(&events);
        assert_eq!(context["task"], "Book a hotel");
        assert_eq!(context["browser"]["url"], "https://example.com");
        assert!(!context.to_string().contains("SECRET"));
    }

    #[test]
    fn workbench_uses_latest_projected_status_for_result_or_failure() {
        let session = SessionMeta {
            id: "s1".to_string(),
            parent_id: None,
            cwd: "/tmp".to_string(),
            artifact_root: "/tmp/artifacts/s1".to_string(),
            status: SessionStatus::Done,
            created_ms: 1,
            updated_ms: 4,
        };
        let events = vec![
            EventRecord {
                seq: 1,
                id: "e1".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 1,
                event_type: "session.input".to_string(),
                payload: json!({"text": "retry task"}),
            },
            EventRecord {
                seq: 2,
                id: "e2".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 2,
                event_type: "session.failed".to_string(),
                payload: json!({"error": "first failure"}),
            },
            EventRecord {
                seq: 3,
                id: "e3".to_string(),
                session_id: "s1".to_string(),
                ts_ms: 3,
                event_type: "session.done".to_string(),
                payload: json!({"result": "retry succeeded"}),
            },
        ];
        let state = project_workbench(
            &[session],
            &events,
            &[("s1".to_string(), events.clone())],
            Some("s1"),
            "local chrome",
        );
        assert_eq!(state.result.as_deref(), Some("retry succeeded"));
        assert!(state.failure.is_none());
    }

    #[test]
    fn model_event_json_round_trips() {
        let event = ModelEvent::ToolCall {
            call: ToolCall {
                id: "call_1".to_string(),
                name: "python".to_string(),
                arguments: json!({"code": "print(1)"}),
            },
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "tool_call");
        let decoded: ModelEvent = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, event);
    }
}
