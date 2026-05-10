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
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
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
    pub browser: BrowserSummary,
    pub history: Vec<HistoryRow>,
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

pub fn result_from_events(events: &[EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|event| {
        if event.event_type == "session.done" {
            event
                .payload
                .get("result")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        } else {
            None
        }
    })
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
            "browser.live_url" => activity.push("connected live browser".to_string()),
            "browser.page" | "browser.state" => {
                if let Some(url) = event.payload.get("url").and_then(Value::as_str) {
                    activity.push(format!("browsing {}", compact_url(url)));
                }
            }
            "tool.started" => {
                if event.payload.get("name").and_then(Value::as_str) == Some("python") {
                    activity.push("using browser".to_string());
                }
            }
            "agent.spawned" => activity.push(agent_started_text(&event.payload)),
            "agent.completed" => activity.push(agent_completed_text(&event.payload)),
            "agent.failed" => activity.push(agent_failed_text(&event.payload)),
            "agent.cancelled" => activity.push(agent_cancelled_text(&event.payload)),
            "model.delta" => activity.push("writing result".to_string()),
            _ => {}
        }
    }
    activity
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

fn agent_completed_text(payload: &Value) -> String {
    let result = payload
        .get("payload")
        .and_then(|payload| payload.get("result"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|result| !result.is_empty());
    match result {
        Some(result) => format!("helper finished: {result}"),
        None => "helper finished".to_string(),
    }
}

fn agent_failed_text(payload: &Value) -> String {
    let error = payload
        .get("payload")
        .and_then(|payload| payload.get("error"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|error| !error.is_empty());
    match error {
        Some(error) => format!("helper failed: {error}"),
        None => "helper failed".to_string(),
    }
}

fn agent_cancelled_text(payload: &Value) -> String {
    let reason = payload
        .get("payload")
        .and_then(|payload| payload.get("reason"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty());
    match reason {
        Some(reason) => format!("helper stopped: {reason}"),
        None => "helper stopped".to_string(),
    }
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
        browser: browser_summary_from_events(events_for_current, browser_backend),
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
        assert_eq!(activity_from_events(&events), vec!["browsing example.com"]);
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
            vec![
                "started checkout helper",
                "helper finished: checkout flow documented",
            ]
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
