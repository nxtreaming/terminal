//! Tests for the tool registry: dispatch-by-name, type-erased routing through
//! the orchestrator, model-visible definitions, and parallel-safe surfacing.
//!
//! All tests are offline. The original tests use the four originally-`Deserialize`
//! handlers (`update_plan`, `tool_search`, `web_search`, `request_user_input`),
//! each a pure / hosted / in-memory tool that touches no network, filesystem,
//! browser, or python interpreter. The registry-gap-closing tests at the bottom
//! exercise ALL TEN handlers: `shell` (dispatched against a real `echo`, which is
//! permitted), `apply_patch` / `view_image` (real filesystem under a tempdir),
//! and `browser` / `python` / `mcp` (injected FAKE backends so no Bun / Chrome /
//! Python / network is touched). They assert every tool registers + dispatches
//! to the right handler, that `model_visible_definitions()` returns 10, and that
//! each tool's wire args round-trip through deserialization.

use std::path::PathBuf;
use std::sync::Arc;

use browser_use_llm::schema::ToolDefinition;

use crate::tools::approval::AskForApproval;
use crate::tools::handlers::apply_patch::{ApplyPatchRequest, ApplyPatchTool};
use crate::tools::handlers::browser::{
    BrowserBackend, BrowserRequest, BrowserTool, BrowserWireArgs,
};
use crate::tools::handlers::done::DoneTool;
use crate::tools::handlers::mcp::{
    McpCallResult, McpClient, McpTool, McpToolCallRequest, McpWireArgs,
};
use crate::tools::handlers::python::{PythonBackend, PythonRequest, PythonTool};
use crate::tools::handlers::request_user_input::{RequestUserInputRequest, RequestUserInputTool};
use crate::tools::handlers::shell::{ShellRequest, ShellTool};
use crate::tools::handlers::tool_search::{ToolSearchEntry, ToolSearchRequest, ToolSearchTool};
use crate::tools::handlers::update_plan::{UpdatePlanRequest, UpdatePlanTool};
use crate::tools::handlers::view_image::{ViewImageRequest, ViewImageTool};
use crate::tools::handlers::web_search::{WebSearchConfig, WebSearchRequest, WebSearchTool};
use crate::tools::orchestrator::TurnEnv;
use crate::tools::registry::{default_registry, definitions, ToolRegistry};
use crate::tools::sandbox::FileSystemSandboxPolicy;
use crate::tools::{ExecOutput, ToolCtx, ToolError, ToolOrchestrator};

use browser_use_browser::{BrowserCommandOutput, BrowserScriptOutput};
use browser_use_python_worker::RunPythonResponse;

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
        artifact_root: std::path::PathBuf::from("/tmp/artifacts"),
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

// ===========================================================================
// Registry-gap-closing tests: all TEN tools register + dispatch.
// ===========================================================================

/// A fake browser backend: records the last call and returns canned output, so
/// no Bun / Chrome / CDP / network is touched (mirrors `browser_tests.rs`).
#[derive(Default)]
struct FakeBrowserBackend;

impl BrowserBackend for FakeBrowserBackend {
    fn command(
        &self,
        _session_id: &str,
        _cwd: &std::path::Path,
        _artifact_dir: &std::path::Path,
        command: &str,
    ) -> anyhow::Result<BrowserCommandOutput> {
        Ok(BrowserCommandOutput {
            content: serde_json::json!({ "echoed": command }),
            events: vec![],
        })
    }

    fn run_script(
        &self,
        _session_id: &str,
        _cwd: &std::path::Path,
        _artifact_dir: &std::path::Path,
        code: &str,
        _timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        Ok(BrowserScriptOutput {
            ok: true,
            text: format!("ran:{code}"),
            ..Default::default()
        })
    }

    fn start_script(
        &self,
        _session_id: &str,
        _cwd: &std::path::Path,
        _artifact_dir: &std::path::Path,
        code: &str,
        _timeout_secs: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        Ok(BrowserScriptOutput {
            ok: true,
            status: Some("running".to_string()),
            text: format!("started:{code}"),
            ..Default::default()
        })
    }

    fn observe_script(
        &self,
        _session_id: &str,
        run_id: &str,
        _observe_timeout_ms: u64,
    ) -> anyhow::Result<BrowserScriptOutput> {
        Ok(BrowserScriptOutput {
            ok: true,
            text: format!("observed:{run_id}"),
            ..Default::default()
        })
    }

    fn cancel_script(
        &self,
        _session_id: &str,
        run_id: &str,
    ) -> anyhow::Result<BrowserScriptOutput> {
        Ok(BrowserScriptOutput {
            ok: false,
            text: format!("cancelled:{run_id}"),
            ..Default::default()
        })
    }
}

/// A fake python backend: returns a canned response echoing the code, so no
/// Python / Bun process is spawned (mirrors `python_tests.rs`).
struct FakePythonBackend;

impl PythonBackend for FakePythonBackend {
    fn run(
        &self,
        _session_id: &str,
        _cwd: &std::path::Path,
        _artifact_dir: &std::path::Path,
        code: &str,
        _timeout_secs: Option<f64>,
    ) -> anyhow::Result<RunPythonResponse> {
        // `RunPythonResponse` has no `Default`; construct it field-by-field
        // (mirrors `python_tests.rs::ok_response`).
        Ok(RunPythonResponse {
            id: "py-reg".to_string(),
            ok: true,
            text: format!("py:{code}"),
            error: None,
            data: serde_json::Value::Null,
            outputs: Vec::new(),
            artifacts: Vec::new(),
            images: Vec::new(),
            browser_events: Vec::new(),
            browser_harness_available: false,
            browser_harness_error: None,
        })
    }
}

/// A fake MCP client: echoes the server/tool, so no MCP server / network is
/// touched (mirrors `mcp_tests.rs`).
struct FakeMcpClient;

impl McpClient for FakeMcpClient {
    fn call_tool(
        &self,
        server: &str,
        tool: &str,
        _args: Option<serde_json::Value>,
    ) -> anyhow::Result<McpCallResult> {
        Ok(McpCallResult::text(format!("mcp:{server}/{tool}")))
    }
}

/// Build a registry holding all ten handlers via [`default_registry`], using
/// fake backends for browser/python/mcp so no OS resource is touched.
fn full_registry() -> ToolRegistry {
    default_registry(
        ShellTool::new(),
        ApplyPatchTool::new(),
        ViewImageTool::new(),
        BrowserTool::with_backend(Arc::new(FakeBrowserBackend)),
        PythonTool::with_backend(Arc::new(FakePythonBackend)),
        McpTool::new(Arc::new(FakeMcpClient)),
        UpdatePlanTool::new(),
        RequestUserInputTool::new(),
        ToolSearchTool::new(vec![ToolSearchEntry::new(
            "kubernetes",
            "manage k8s clusters",
            ["namespace"],
        )]),
        WebSearchTool::new(WebSearchConfig::enabled()),
        DoneTool::new(),
    )
}

/// A `ToolCtx` rooted at `cwd` (so filesystem tools resolve under a tempdir).
fn ctx_at(name: &str, cwd: PathBuf) -> ToolCtx {
    ToolCtx {
        call_id: "c1".to_string(),
        tool_name: name.to_string(),
        cwd: cwd.clone(),
        artifact_root: cwd.join("artifacts"),
    }
}

#[test]
fn default_registry_registers_all_eleven_tools() {
    let reg = full_registry();
    assert_eq!(reg.len(), 11, "all eleven tools must register");
    let defs = reg.model_visible_definitions();
    assert_eq!(
        defs.len(),
        11,
        "model_visible_definitions must list all eleven"
    );
    let mut names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "apply_patch",
            "browser",
            "done",
            "mcp",
            "python",
            "request_user_input",
            "shell",
            "tool_search",
            "update_plan",
            "view_image",
            "web_search",
        ]
    );
    // Every definition carries a non-empty description + object schema.
    for d in &defs {
        assert!(
            !d.description.is_empty(),
            "{} has empty description",
            d.name
        );
        assert_eq!(d.input_schema["type"], "object", "{} schema", d.name);
    }
}

#[test]
fn parallel_safe_flags_match_registration() {
    let reg = full_registry();
    // Pure / read-only tools are parallel-safe.
    assert_eq!(reg.parallel_safe("tool_search"), Some(true));
    assert_eq!(reg.parallel_safe("web_search"), Some(true));
    // Everything else is serial.
    for name in [
        "shell",
        "apply_patch",
        "view_image",
        "browser",
        "python",
        "mcp",
        "update_plan",
        "request_user_input",
        "done",
    ] {
        assert_eq!(
            reg.parallel_safe(name),
            Some(false),
            "{name} should be serial"
        );
    }
}

#[tokio::test]
async fn shell_dispatches_a_real_echo() {
    let reg = full_registry();
    let orch = ToolOrchestrator::stub();
    let input = serde_json::json!({ "command": ["echo", "hello-registry"] });
    let out = reg
        .dispatch(
            "shell",
            &input,
            &ctx("shell"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("shell should dispatch");
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("hello-registry"),
        "shell stdout: {:?}",
        out.stdout
    );
}

#[tokio::test]
async fn apply_patch_dispatches_and_writes_a_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg = full_registry();
    let orch = ToolOrchestrator::stub();
    let patch = "*** Begin Patch\n*** Add File: created.txt\n+made-by-registry\n*** End Patch\n";
    let input = serde_json::json!({ "patch": patch });
    let out = reg
        .dispatch(
            "apply_patch",
            &input,
            &ctx_at("apply_patch", dir.path().to_path_buf()),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("apply_patch should dispatch");
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    let written = std::fs::read_to_string(dir.path().join("created.txt")).expect("file written");
    assert_eq!(written, "made-by-registry");
}

#[tokio::test]
async fn view_image_dispatches_and_reads_an_image() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A 1x1 PNG (smallest valid-enough bytes for the read+encode path).
    let png_bytes: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xDE, 0xAD, 0xBE, 0xEF,
    ];
    std::fs::write(dir.path().join("pic.png"), png_bytes).expect("write png");
    let reg = full_registry();
    let orch = ToolOrchestrator::stub();
    let input = serde_json::json!({ "path": "pic.png" });
    let out = reg
        .dispatch(
            "view_image",
            &input,
            &ctx_at("view_image", dir.path().to_path_buf()),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("view_image should dispatch");
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("data:image/png;base64,"),
        "view_image stdout: {:?}",
        out.stdout
    );
}

#[tokio::test]
async fn browser_dispatches_through_the_wire_args() {
    let reg = full_registry();
    let orch = ToolOrchestrator::stub();
    // The `execute` action with the flat wire-args shape -> BrowserRequest.
    let input = serde_json::json!({
        "action": "execute",
        "session_id": "s1",
        "script": "click()",
        "background": false
    });
    let out = reg
        .dispatch(
            "browser",
            &input,
            &ctx("browser"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("browser should dispatch");
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("ran:click()"),
        "browser stdout: {:?}",
        out.stdout
    );
}

#[tokio::test]
async fn python_dispatches_to_the_fake_backend() {
    let reg = full_registry();
    let orch = ToolOrchestrator::stub();
    let input = serde_json::json!({ "code": "print(1)" });
    let out = reg
        .dispatch(
            "python",
            &input,
            &ctx("python"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("python should dispatch");
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("py:print(1)"),
        "python stdout: {:?}",
        out.stdout
    );
}

#[tokio::test]
async fn mcp_dispatches_through_the_wire_args() {
    let reg = full_registry();
    let orch = ToolOrchestrator::stub();
    let input = serde_json::json!({
        "server": "memory",
        "tool": "create_entities",
        "arguments": { "x": 1 }
    });
    let out = reg
        .dispatch(
            "mcp",
            &input,
            &ctx("mcp"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("mcp should dispatch");
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("mcp:memory/create_entities"),
        "mcp stdout: {:?}",
        out.stdout
    );
}

#[tokio::test]
async fn update_plan_and_request_user_input_dispatch() {
    let reg = full_registry();
    let orch = ToolOrchestrator::stub();
    // update_plan
    let plan = serde_json::json!({
        "plan": [{ "step": "do it", "status": "pending" }]
    });
    let out = reg
        .dispatch(
            "update_plan",
            &plan,
            &ctx("update_plan"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("update_plan should dispatch");
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("do it"),
        "update_plan: {:?}",
        out.stdout
    );

    // request_user_input: a full valid question (non-empty id/question +
    // non-empty options, per the handler's validation rules).
    let q = serde_json::json!({
        "questions": [{
            "id": "name",
            "header": "Name",
            "question": "What is your name?",
            "options": [
                { "label": "Alice", "description": "the first option" },
                { "label": "Bob", "description": "the second option" }
            ]
        }]
    });
    let out = reg
        .dispatch(
            "request_user_input",
            &q,
            &ctx("request_user_input"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("request_user_input should dispatch");
    assert_eq!(out.exit_code, 0);
}

#[tokio::test]
async fn tool_search_and_web_search_dispatch() {
    let reg = full_registry();
    let orch = ToolOrchestrator::stub();
    let ts = reg
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
        ts.stdout.contains("kubernetes"),
        "tool_search: {:?}",
        ts.stdout
    );

    let ws = reg
        .dispatch(
            "web_search",
            &serde_json::json!({ "query": "rust async" }),
            &ctx("web_search"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("web_search should dispatch");
    assert!(
        ws.stdout.contains("rust async"),
        "web_search: {:?}",
        ws.stdout
    );
}

#[tokio::test]
async fn browser_bad_action_value_surfaces_an_error_naming_the_tool() {
    let reg = full_registry();
    let orch = ToolOrchestrator::stub();
    // `action` is not one of the enum variants -> wire-args deserialize fails.
    let bad = serde_json::json!({ "action": "teleport", "session_id": "s1" });
    let err = reg
        .dispatch(
            "browser",
            &bad,
            &ctx("browser"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect_err("bad browser action must error");
    match err {
        ToolError::Other(e) => assert!(
            e.to_string().contains("tool `browser`") && e.to_string().contains("invalid arguments"),
            "unexpected error: {e}"
        ),
        other => panic!("expected Other(invalid arguments), got {other:?}"),
    }
}

// ---- Wire-arg deserialize round-trips (one per tool's model arg object) ----

#[test]
fn shell_wire_args_round_trip() {
    let req: ShellRequest = serde_json::from_value(serde_json::json!({
        "command": ["ls", "-la"],
        "cwd": "/tmp",
        "timeout_ms": 5000,
        "env": { "KEY": "VAL" }
    }))
    .expect("shell deserialize");
    assert_eq!(req.command, vec!["ls".to_string(), "-la".to_string()]);
    assert_eq!(req.cwd, Some(PathBuf::from("/tmp")));
    assert_eq!(req.timeout_ms, Some(5000));
    assert_eq!(req.env.get("KEY").map(String::as_str), Some("VAL"));
    // Minimal form: only `command` required; the rest default.
    let min: ShellRequest =
        serde_json::from_value(serde_json::json!({ "command": ["pwd"] })).expect("min shell");
    assert_eq!(min.cwd, None);
    assert_eq!(min.timeout_ms, None);
    assert!(min.env.is_empty());
}

#[test]
fn apply_patch_wire_args_round_trip() {
    let req: ApplyPatchRequest =
        serde_json::from_value(serde_json::json!({ "patch": "*** Begin Patch\n*** End Patch\n" }))
            .expect("apply_patch deserialize");
    assert!(req.patch.contains("Begin Patch"));
    assert_eq!(
        req.cwd, None,
        "cwd not a model wire field; defaults to None"
    );
}

#[test]
fn view_image_wire_args_round_trip() {
    let req: ViewImageRequest =
        serde_json::from_value(serde_json::json!({ "path": "a/b.png" })).expect("view_image");
    assert_eq!(req.path, PathBuf::from("a/b.png"));
    assert_eq!(req.cwd, None);
}

#[test]
fn python_wire_args_round_trip() {
    let req: PythonRequest = serde_json::from_value(serde_json::json!({
        "code": "x=1",
        "session_id": "s",
        "timeout_secs": 12.5
    }))
    .expect("python deserialize");
    assert_eq!(req.code, "x=1");
    assert_eq!(req.session_id.as_deref(), Some("s"));
    assert_eq!(req.timeout_secs, Some(12.5));
    // Minimal: only `code`.
    let min: PythonRequest =
        serde_json::from_value(serde_json::json!({ "code": "y=2" })).expect("min python");
    assert_eq!(min.session_id, None);
    assert_eq!(min.timeout_secs, None);
}

#[test]
fn browser_wire_args_round_trip_and_convert() {
    // command action
    let w: BrowserWireArgs = serde_json::from_value(serde_json::json!({
        "action": "command",
        "session_id": "s1",
        "command": "go https://example.com"
    }))
    .expect("browser wire deserialize");
    let req: BrowserRequest = w.into();
    assert_eq!(req.session_id, "s1");
    assert_eq!(
        req.action,
        crate::tools::handlers::browser::BrowserAction::Command {
            command: "go https://example.com".to_string()
        }
    );
    // observe action
    let w2: BrowserWireArgs = serde_json::from_value(serde_json::json!({
        "action": "observe",
        "session_id": "s1",
        "run_id": "r9"
    }))
    .expect("browser observe wire");
    let req2: BrowserRequest = w2.into();
    assert_eq!(
        req2.action,
        crate::tools::handlers::browser::BrowserAction::Observe {
            run_id: "r9".to_string()
        }
    );
}

#[test]
fn mcp_wire_args_round_trip_and_convert() {
    let w: McpWireArgs = serde_json::from_value(serde_json::json!({
        "server": "memory",
        "tool": "create_entities",
        "arguments": { "k": "v" },
        "read_only": true
    }))
    .expect("mcp wire deserialize");
    let req: McpToolCallRequest = w.into();
    assert_eq!(req.server, "memory");
    assert_eq!(req.tool, "create_entities");
    assert_eq!(req.arguments, serde_json::json!({ "k": "v" }));
    assert!(req.read_only);
    // Minimal: only server + tool; arguments default to Null, read_only false.
    let min: McpWireArgs =
        serde_json::from_value(serde_json::json!({ "server": "s", "tool": "t" }))
            .expect("min mcp wire");
    let min_req: McpToolCallRequest = min.into();
    assert!(min_req.arguments.is_null());
    assert!(!min_req.read_only);
}

#[test]
fn definitions_carry_required_fields_and_names() {
    // Each builder's name matches its registered key + marks its required args.
    assert_eq!(definitions::shell().name, "shell");
    assert_eq!(definitions::shell().input_schema["required"][0], "command");
    assert_eq!(
        definitions::apply_patch().input_schema["required"][0],
        "patch"
    );
    assert_eq!(
        definitions::view_image().input_schema["required"][0],
        "path"
    );
    assert_eq!(definitions::python().input_schema["required"][0], "code");
    assert_eq!(definitions::browser().input_schema["required"][0], "action");
    assert_eq!(definitions::mcp().input_schema["required"][0], "server");
}

/// REGRESSION (fix 1): the `request_user_input` schema must advertise the shape
/// the handler ACTUALLY accepts — `{ "questions": [...] }` — NOT the old flat
/// `{ "prompt": ... }`, which a model would follow and then hit a deserialize
/// error. The advertised schema's `required`/`properties` and a model-shaped
/// payload must both line up with `RequestUserInputRequest`.
#[test]
fn request_user_input_schema_matches_the_handler_questions_shape() {
    let schema = definitions::request_user_input().input_schema;

    // The advertised schema requires `questions` (an array), NOT `prompt`.
    assert_eq!(
        schema["required"][0], "questions",
        "schema must require `questions`"
    );
    assert!(
        schema["properties"].get("questions").is_some(),
        "schema must advertise a `questions` property"
    );
    assert_eq!(schema["properties"]["questions"]["type"], "array");
    // The OLD buggy `prompt` field must be gone (it was never accepted).
    assert!(
        schema["properties"].get("prompt").is_none(),
        "schema must NOT advertise the old `prompt` field"
    );
    assert_ne!(
        schema["required"][0], "prompt",
        "schema must not require the old `prompt`"
    );
    // Each question item advertises id/header/question/options.
    let item_required = &schema["properties"]["questions"]["items"]["required"];
    let req_set: Vec<&str> = item_required
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for field in ["id", "header", "question", "options"] {
        assert!(
            req_set.contains(&field),
            "question item must require `{field}`"
        );
    }

    // A MODEL-shaped payload that follows the advertised schema deserializes into
    // the handler's real `RequestUserInputRequest`.
    let model_payload = serde_json::json!({
        "questions": [{
            "id": "deploy",
            "header": "Deploy",
            "question": "Ship it?",
            "options": [
                { "label": "Yes", "description": "deploy now" },
                { "label": "No", "description": "hold off" }
            ]
        }]
    });
    let parsed: RequestUserInputRequest =
        serde_json::from_value(model_payload).expect("model `questions` payload must deserialize");
    assert_eq!(parsed.questions.len(), 1);
    assert_eq!(parsed.questions[0].id, "deploy");
    assert_eq!(parsed.questions[0].options.as_ref().unwrap().len(), 2);

    // The OLD advertised payload `{ "prompt": ... }` is NOT what the handler
    // accepts (proving the old schema was a real correctness bug).
    let old_payload = serde_json::json!({ "prompt": "Ship it?" });
    assert!(
        serde_json::from_value::<RequestUserInputRequest>(old_payload).is_err(),
        "the old `{{prompt}}` shape must NOT deserialize into the handler's request"
    );
}

/// The `done` (completion) tool is registered + dispatches through the registry,
/// returning its prefixed acknowledgement (fix 3).
#[tokio::test]
async fn done_dispatches_through_the_registry() {
    let reg = full_registry();
    assert!(reg.contains("done"), "done must be registered");
    let orch = ToolOrchestrator::stub();
    let out = reg
        .dispatch(
            "done",
            &serde_json::json!({ "text": "task finished" }),
            &ctx("done"),
            &env(),
            AskForApproval::Never,
            &orch,
        )
        .await
        .expect("done should dispatch");
    assert_eq!(out.exit_code, 0);
    assert_eq!(
        out.stdout,
        format!(
            "{}task finished",
            crate::tools::handlers::done::DONE_STDOUT_PREFIX
        )
    );
    // done is serial (terminal).
    assert_eq!(reg.parallel_safe("done"), Some(false));
}
