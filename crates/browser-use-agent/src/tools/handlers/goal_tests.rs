//! Unit tests for the goal tool handlers.
//!
//! Network-free: drive each handler through the stub orchestrator (the same seam
//! the production dispatcher uses), asserting the shared `GoalStore` wiring and
//! the emitted durable `goal.*` events.

use std::sync::Arc;
use std::sync::Mutex;

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

#[tokio::test]
async fn create_then_get_shares_state_and_emits_created_event() {
    let sink = Arc::new(RecSink::default());
    let store = Arc::new(GoalStore::new("sess-1", sink.clone()));
    let orch = ToolOrchestrator::stub();

    // create_goal folds into the shared store and emits `goal.created`.
    let create = CreateGoalTool::new(store.clone());
    let req: CreateGoalRequest =
        serde_json::from_value(json!({"text": "ship the goals row", "token_budget": 1000}))
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
    assert_eq!(created["active"], true);
    assert_eq!(created["text"], "ship the goals row");
    assert_eq!(created["token_budget"], 1000);
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
    assert_eq!(fetched["text"], "ship the goals row");
    assert_eq!(fetched["active"], true);
}

#[tokio::test]
async fn update_goal_folds_and_emits_updated_event() {
    let sink = Arc::new(RecSink::default());
    let store = Arc::new(GoalStore::new("sess-2", sink.clone()));
    let orch = ToolOrchestrator::stub();

    let create = CreateGoalTool::new(store.clone());
    let creq: CreateGoalRequest = serde_json::from_value(json!({"text": "do it"})).unwrap();
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
    assert_eq!(updated["status"], "complete");
    assert!(
        sink.types().contains(&GOAL_UPDATED_EVENT.to_string()),
        "update_goal must emit goal.updated, got {:?}",
        sink.types()
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
