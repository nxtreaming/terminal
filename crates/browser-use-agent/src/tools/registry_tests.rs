//! Tests for the tool registry: dispatch-by-name, type-erased routing through
//! the orchestrator, model-visible definitions, and parallel-safe surfacing.
//!
//! All tests are offline: they use the four `Deserialize`-able handlers
//! (`update_plan`, `tool_search`, `web_search`, `request_user_input`), each a
//! pure / hosted / in-memory tool that touches no network, filesystem, browser,
//! or python interpreter. The six handlers whose `Req` is not yet `Deserialize`
//! (`shell`, `apply_patch`, `view_image`, `browser`, `python`, `mcp`) cannot be
//! registered yet — see the module-level follow-up note in `registry.rs`.

use browser_use_llm::schema::ToolDefinition;

use crate::tools::approval::AskForApproval;
use crate::tools::handlers::request_user_input::{RequestUserInputRequest, RequestUserInputTool};
use crate::tools::handlers::tool_search::{ToolSearchEntry, ToolSearchRequest, ToolSearchTool};
use crate::tools::handlers::update_plan::{UpdatePlanRequest, UpdatePlanTool};
use crate::tools::handlers::web_search::{WebSearchConfig, WebSearchRequest, WebSearchTool};
use crate::tools::orchestrator::TurnEnv;
use crate::tools::registry::ToolRegistry;
use crate::tools::sandbox::FileSystemSandboxPolicy;
use crate::tools::{ExecOutput, ToolCtx, ToolError, ToolOrchestrator};

/// A bare object-schema definition for a tool with the given `name`.
fn def(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: format!("the {name} tool"),
        input_schema: serde_json::json!({ "type": "object" }),
    }
}

fn env() -> TurnEnv {
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
        call_id: "c1".to_string(),
        tool_name: name.to_string(),
        cwd: std::path::PathBuf::from("/tmp"),
    }
}

/// Build a registry with a representative spread of the `Deserialize`-able
/// handlers, carrying each tool's static `parallel_safe`.
fn registry_with_basics() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    // update_plan: pure, serial.
    reg.register::<_, UpdatePlanRequest>(
        "update_plan",
        def("update_plan"),
        false,
        UpdatePlanTool::new(),
    );
    // request_user_input: pure (request side), serial.
    reg.register::<_, RequestUserInputRequest>(
        "request_user_input",
        def("request_user_input"),
        false,
        RequestUserInputTool::new(),
    );
    // tool_search: BM25 over an in-memory catalog, parallel-safe.
    reg.register::<_, ToolSearchRequest>(
        "tool_search",
        def("tool_search"),
        true,
        ToolSearchTool::new(vec![
            ToolSearchEntry::new("kubernetes", "manage k8s clusters", ["namespace"]),
            ToolSearchEntry::new("terraform", "provision infra", ["module"]),
        ]),
    );
    // web_search: hosted/passthrough, parallel-safe.
    reg.register::<_, WebSearchRequest>(
        "web_search",
        def("web_search"),
        true,
        WebSearchTool::new(WebSearchConfig::enabled()),
    );
    reg
}

#[tokio::test]
async fn dispatch_routes_to_named_tool_and_returns_its_output() {
    let reg = registry_with_basics();
    let orch = ToolOrchestrator::stub();

    let input = serde_json::json!({
        "plan": [
            {"step": "first", "status": "pending"},
            {"step": "second", "status": "completed"}
        ]
    });
    let out = reg
        .dispatch(
            "update_plan",
            &input,
            &ctx("update_plan"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("update_plan should dispatch");
    // update_plan renders a "Plan updated:" summary with one line per step.
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("Plan updated:"),
        "got: {:?}",
        out.stdout
    );
    assert!(out.stdout.contains("[ ] first"), "got: {:?}", out.stdout);
    assert!(out.stdout.contains("[x] second"), "got: {:?}", out.stdout);
}

#[tokio::test]
async fn dispatch_routes_distinct_tools_to_distinct_handlers() {
    let reg = registry_with_basics();
    let orch = ToolOrchestrator::stub();

    // tool_search ranks the in-memory catalog -> the matching entry name.
    let ts_input = serde_json::json!({ "query": "kubernetes" });
    let ts_out: ExecOutput = reg
        .dispatch(
            "tool_search",
            &ts_input,
            &ctx("tool_search"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("tool_search should dispatch");
    assert!(
        ts_out.stdout.contains("kubernetes"),
        "tool_search output should rank the match, got: {:?}",
        ts_out.stdout
    );

    // web_search (hosted/passthrough) -> marker mentioning the query.
    let ws_input = serde_json::json!({ "query": "rust async" });
    let ws_out = reg
        .dispatch(
            "web_search",
            &ws_input,
            &ctx("web_search"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("web_search should dispatch");
    assert!(
        ws_out.stdout.contains("rust async"),
        "web_search output should reflect the query, got: {:?}",
        ws_out.stdout
    );
}

#[tokio::test]
async fn dispatch_unknown_tool_is_an_error() {
    let reg = registry_with_basics();
    let orch = ToolOrchestrator::stub();

    let err = reg
        .dispatch(
            "does_not_exist",
            &serde_json::json!({}),
            &ctx("does_not_exist"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect_err("unknown tool must error");
    match err {
        ToolError::Other(e) => assert!(
            e.to_string().contains("unknown tool `does_not_exist`"),
            "unexpected error: {e}"
        ),
        other => panic!("expected Other(unknown tool), got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_with_bad_arguments_surfaces_an_error_naming_the_tool() {
    let reg = registry_with_basics();
    let orch = ToolOrchestrator::stub();

    // update_plan requires `plan: Vec<PlanItem>`; pass a wrong shape.
    let bad = serde_json::json!({ "plan": "not-an-array" });
    let err = reg
        .dispatch(
            "update_plan",
            &bad,
            &ctx("update_plan"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect_err("bad args must error");
    match err {
        ToolError::Other(e) => assert!(
            e.to_string().contains("tool `update_plan`")
                && e.to_string().contains("invalid arguments"),
            "unexpected error: {e}"
        ),
        other => panic!("expected Other(invalid arguments), got {other:?}"),
    }
}

#[tokio::test]
async fn input_value_deserializes_into_the_tools_req() {
    let reg = registry_with_basics();
    let orch = ToolOrchestrator::stub();

    // tool_search takes `{ query, limit? }`; confirm a Value with an explicit
    // limit deserializes into the tool's `ToolSearchRequest` and runs.
    let input = serde_json::json!({ "query": "terraform", "limit": 1 });
    let out = reg
        .dispatch(
            "tool_search",
            &input,
            &ctx("tool_search"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("tool_search should dispatch");
    assert!(
        out.stdout.contains("terraform"),
        "tool_search should reflect the deserialized query, got: {:?}",
        out.stdout
    );
}

#[test]
fn model_visible_definitions_lists_all_registered_tools() {
    let reg = registry_with_basics();
    let defs = reg.model_visible_definitions();
    let mut names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "request_user_input",
            "tool_search",
            "update_plan",
            "web_search"
        ]
    );
    assert_eq!(defs.len(), reg.len());
    // Definitions carry the handler's description + schema.
    let plan = defs
        .iter()
        .find(|d| d.name == "update_plan")
        .expect("update_plan definition present");
    assert!(!plan.description.is_empty());
    assert_eq!(plan.input_schema["type"], "object");
}

#[test]
fn parallel_safe_is_surfaced_per_tool() {
    let reg = registry_with_basics();
    // update_plan / request_user_input are serial; tool_search / web_search are
    // parallel-safe.
    assert_eq!(reg.parallel_safe("update_plan"), Some(false));
    assert_eq!(reg.parallel_safe("request_user_input"), Some(false));
    assert_eq!(reg.parallel_safe("tool_search"), Some(true));
    assert_eq!(reg.parallel_safe("web_search"), Some(true));
    assert_eq!(reg.parallel_safe("nope"), None);
}

#[test]
fn deferred_search_entries_round_trip() {
    // The default `(S, A)` seams are filled in by the type alias defaults.
    let mut reg: ToolRegistry = ToolRegistry::new();
    let entries = vec![
        ToolSearchEntry::new("rare_tool", "rarely used", ["arg"]),
        ToolSearchEntry::new("big_tool", "large schema", ["x", "y"]),
    ];
    reg.set_deferred_search_entries(entries.clone());
    assert_eq!(reg.deferred_search_entries(), entries.as_slice());
}

#[tokio::test]
async fn tool_search_handler_dispatches_over_a_catalog() {
    // tool_search is itself a registered tool whose catalog mirrors the
    // registry's deferred entries.
    let catalog = vec![
        ToolSearchEntry::new("kubernetes", "manage k8s clusters", ["namespace"]),
        ToolSearchEntry::new("terraform", "provision infra", ["module"]),
    ];
    let mut reg: ToolRegistry = ToolRegistry::new();
    reg.register::<_, ToolSearchRequest>(
        "tool_search",
        def("tool_search"),
        true,
        ToolSearchTool::new(catalog.clone()),
    );
    reg.set_deferred_search_entries(catalog);

    let orch = ToolOrchestrator::stub();
    let out = reg
        .dispatch(
            "tool_search",
            &serde_json::json!({ "query": "kubernetes" }),
            &ctx("tool_search"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("tool_search should dispatch");
    assert!(
        out.stdout.contains("kubernetes"),
        "tool_search should rank the matching entry, got: {:?}",
        out.stdout
    );
    // tool_search is parallel-safe.
    assert_eq!(reg.parallel_safe("tool_search"), Some(true));
    // and its catalog is mirrored as the deferred search entries.
    assert_eq!(reg.deferred_search_entries().len(), 2);
}

#[test]
fn last_registration_for_a_name_wins() {
    let mut reg: ToolRegistry = ToolRegistry::new();
    reg.register::<_, UpdatePlanRequest>(
        "update_plan",
        def("update_plan"),
        false,
        UpdatePlanTool::new(),
    );
    reg.register::<_, UpdatePlanRequest>(
        "update_plan",
        def("update_plan"),
        false,
        UpdatePlanTool::new(),
    );
    assert_eq!(reg.len(), 1);
    assert!(reg.contains("update_plan"));
}
