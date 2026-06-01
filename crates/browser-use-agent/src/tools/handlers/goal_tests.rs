//! Unit tests for the goal tool handlers.
//!
//! Network-free: drive each handler through the stub orchestrator (the same seam
//! the production dispatcher uses), asserting the shared `GoalStore` wiring and
//! the emitted durable `goal.*` events.

use std::sync::Arc;
use std::sync::Mutex;

use browser_use_protocol::EventRecord;
use serde_json::json;

use crate::events::{EventSink, PendingEvent};
use crate::tools::approval::AskForApproval;
use crate::tools::handlers::goal::*;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::ToolCtx;
use crate::tools::sandbox::FileSystemSandboxPolicy;

#[derive(Default)]
struct RecSink {
    events: Mutex<Vec<PendingEvent>>,
}
impl EventSink for RecSink {
    fn emit(&self, ev: PendingEvent) {
        self.events.lock().unwrap().push(ev);
    }
}
impl RecSink {
    fn types(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.event_type.clone())
            .collect()
    }
}

fn turn_env() -> TurnEnv {
    TurnEnv {
        file_system_sandbox_policy: FileSystemSandboxPolicy {
            restricted: false,
            denied_read: false,
        },
        managed_network_active: false,
        strict_auto_review: false,
        use_guardian: false,
    }
}

fn ctx(name: &str) -> ToolCtx {
    ToolCtx {
        call_id: format!("call-{name}"),
        tool_name: name.to_string(),
        cwd: std::env::temp_dir(),
        artifact_root: std::env::temp_dir().join("artifacts"),
    }
}

fn event_record(seq: i64, ts_ms: i64, ty: &str, payload: serde_json::Value) -> EventRecord {
    EventRecord {
        seq,
        id: format!("event-{seq}"),
        session_id: "sess-replay".to_string(),
        ts_ms,
        event_type: ty.to_string(),
        payload,
    }
}

#[tokio::test]
async fn create_then_get_shares_state_and_emits_created_event() {
    let sink = Arc::new(RecSink::default());
    let store = Arc::new(GoalStore::new("sess-1", sink.clone()));
    let orch = ToolOrchestrator::stub();

    // create_goal folds into the shared store and emits `goal.created`.
    let create = CreateGoalTool::new(store.clone());
    let req: CreateGoalRequest =
        serde_json::from_value(json!({"objective": "ship the goals row", "token_budget": 1000}))
            .unwrap();
    let out = orch
        .run(
            &create,
            &req,
            &ctx("create_goal"),
            &turn_env(),
            AskForApproval::Never,
        )
        .await
        .expect("create_goal runs");
    let created: serde_json::Value = serde_json::from_str(&out.output.stdout).unwrap();
    assert_eq!(created["goal"]["objective"], "ship the goals row");
    assert_eq!(created["goal"]["status"], "active");
    assert_eq!(created["goal"]["tokenBudget"], 1000);
    assert_eq!(created["remainingTokens"], 1000);
    assert!(
        sink.types()
            .contains(&crate::goals::GOAL_SET_EVENT.to_string()),
        "create_goal must emit goal.created, got {:?}",
        sink.types()
    );

    // get_goal observes the SAME shared state (proving the store is shared).
    let get = GetGoalTool::new(store.clone());
    let greq: GetGoalRequest = serde_json::from_value(json!({})).unwrap();
    let gout = orch
        .run(
            &get,
            &greq,
            &ctx("get_goal"),
            &turn_env(),
            AskForApproval::Never,
        )
        .await
        .expect("get_goal runs");
    let fetched: serde_json::Value = serde_json::from_str(&gout.output.stdout).unwrap();
    assert_eq!(fetched["goal"]["objective"], "ship the goals row");
    assert_eq!(fetched["goal"]["status"], "active");
}

#[tokio::test]
async fn update_goal_folds_and_emits_updated_event() {
    let sink = Arc::new(RecSink::default());
    let store = Arc::new(GoalStore::new("sess-2", sink.clone()));
    let orch = ToolOrchestrator::stub();

    let create = CreateGoalTool::new(store.clone());
    let creq: CreateGoalRequest = serde_json::from_value(json!({"objective": "do it"})).unwrap();
    orch.run(
        &create,
        &creq,
        &ctx("create_goal"),
        &turn_env(),
        AskForApproval::Never,
    )
    .await
    .expect("create_goal runs");

    let update = UpdateGoalTool::new(store.clone());
    let ureq: UpdateGoalRequest = serde_json::from_value(json!({"status": "complete"})).unwrap();
    let out = orch
        .run(
            &update,
            &ureq,
            &ctx("update_goal"),
            &turn_env(),
            AskForApproval::Never,
        )
        .await
        .expect("update_goal runs");
    let updated: serde_json::Value = serde_json::from_str(&out.output.stdout).unwrap();
    assert_eq!(updated["goal"]["status"], "complete");
    assert!(
        sink.types().contains(&GOAL_UPDATED_EVENT.to_string()),
        "update_goal must emit goal.updated, got {:?}",
        sink.types()
    );
}

#[tokio::test]
async fn update_goal_accounts_elapsed_before_terminal_status() {
    let sink = Arc::new(RecSink::default());
    let records = vec![event_record(
        1,
        1_000,
        crate::goals::GOAL_SET_EVENT,
        json!({
            "objective": "do it",
            "status": "active",
            "createdAt": 1,
            "updatedAt": 1,
        }),
    )];
    let store = Arc::new(GoalStore::from_event_records(
        "sess-replay",
        sink.clone(),
        &records,
    ));
    let orch = ToolOrchestrator::stub();

    let update = UpdateGoalTool::new(store);
    let ureq: UpdateGoalRequest = serde_json::from_value(json!({"status": "complete"})).unwrap();
    let out = orch
        .run(
            &update,
            &ureq,
            &ctx("update_goal"),
            &turn_env(),
            AskForApproval::Never,
        )
        .await
        .expect("update_goal runs");
    let updated: serde_json::Value = serde_json::from_str(&out.output.stdout).unwrap();
    assert_eq!(updated["goal"]["status"], "complete");

    let events = sink.events.lock().unwrap();
    let accounted_idx = events
        .iter()
        .position(|event| event.event_type == GOAL_ACCOUNTED_EVENT)
        .expect("update_goal should emit final accounting first");
    let updated_idx = events
        .iter()
        .position(|event| event.event_type == GOAL_UPDATED_EVENT)
        .expect("update_goal should emit goal.updated");
    assert!(
        accounted_idx < updated_idx,
        "final accounting must happen before terminal status mutation"
    );
    assert!(
        events[accounted_idx].payload["timeUsedSeconds"]
            .as_i64()
            .is_some_and(|seconds| seconds > 0),
        "final accounting should include elapsed active time"
    );
}

#[tokio::test]
async fn update_goal_without_active_goal_errors() {
    let sink = Arc::new(RecSink::default());
    let store = Arc::new(GoalStore::new("sess-3", sink));
    let orch = ToolOrchestrator::stub();

    let update = UpdateGoalTool::new(store);
    let ureq: UpdateGoalRequest = serde_json::from_value(json!({"status": "complete"})).unwrap();
    let res = orch
        .run(
            &update,
            &ureq,
            &ctx("update_goal"),
            &turn_env(),
            AskForApproval::Never,
        )
        .await;
    assert!(res.is_err(), "update with no active goal must error");
}

#[test]
fn budget_limited_context_is_reported_once_per_store() {
    let sink = Arc::new(RecSink::default());
    let store = GoalStore::new("sess-budget", sink);
    store
        .create_goal_response("stay inside budget", Some(10))
        .expect("goal created");
    store.account_usage(
        &browser_use_llm::schema::Usage {
            input_tokens: 11,
            total_tokens: 11,
            ..Default::default()
        },
        1,
    );

    let first = store.goal_context_text().expect("first budget prompt");
    assert!(first.contains("has reached its token budget"));
    assert!(
        store.goal_context_text().is_none(),
        "budget-limit steering should not repeat in the same store"
    );
}

#[test]
fn replayed_goal_created_replaces_prior_goal_snapshot() {
    let records = vec![
        event_record(
            1,
            10_000,
            crate::goals::GOAL_SET_EVENT,
            json!({
                "goal_id": "goal-1",
                "objective": "first objective",
                "createdAt": 10,
                "updatedAt": 10,
            }),
        ),
        event_record(
            2,
            11_000,
            GOAL_ACCOUNTED_EVENT,
            json!({"tokens_used": 50, "time_used_seconds": 2}),
        ),
        event_record(
            3,
            30_000,
            crate::goals::GOAL_SET_EVENT,
            json!({
                "goal_id": "goal-2",
                "objective": "replacement objective",
                "createdAt": 30,
                "updatedAt": 31,
            }),
        ),
    ];

    let response = goal_response_from_event_records("sess-replay", &records);
    assert_eq!(response["goal"]["objective"], "replacement objective");
    assert_eq!(response["goal"]["createdAt"], 30);
    assert_eq!(response["goal"]["updatedAt"], 31);
    assert_eq!(response["goal"]["tokensUsed"], 0);
}

#[test]
fn shared_store_goal_refreshes_from_external_events() {
    let temp = tempfile::tempdir().unwrap();
    let shared = Arc::new(Mutex::new(
        browser_use_store::Store::open(temp.path()).unwrap(),
    ));
    let session = shared
        .lock()
        .unwrap()
        .create_session(None, temp.path())
        .unwrap();
    let sink = Arc::new(RecSink::default());
    let store = GoalStore::from_shared_store(session.id.clone(), sink, shared.clone());

    shared
        .lock()
        .unwrap()
        .append_event(
            &session.id,
            crate::goals::GOAL_SET_EVENT,
            json!({"goal_id": "goal-1", "objective": "external objective"}),
        )
        .unwrap();
    let created = store.response(CompletionBudgetReport::Omit);
    assert_eq!(created["goal"]["objective"], "external objective");
    assert_eq!(created["goal"]["status"], "active");

    shared
        .lock()
        .unwrap()
        .append_event(&session.id, GOAL_UPDATED_EVENT, json!({"status": "paused"}))
        .unwrap();
    let updated = store.response(CompletionBudgetReport::Omit);
    assert_eq!(updated["goal"]["status"], "paused");
}
