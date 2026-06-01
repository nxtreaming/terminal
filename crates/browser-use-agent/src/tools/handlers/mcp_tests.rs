//! Tests for the MCP tool-dispatch handler ([`McpTool`]).
//!
//! These NEVER touch a real MCP server or the network. A [`FakeMcpClient`]
//! records the (server, tool, args) it was routed and returns canned
//! [`McpCallResult`]s (or an error), so the handler's routing + result mapping
//! can be verified in isolation.
//!
//! Tests cover: (1) a tool call routes server/tool/args to the client and maps
//! content -> ExecOutput; (2) an is_error result -> nonzero exit + stderr; (3)
//! image/structured content items map sensibly; (4) a client error ->
//! ToolError; (5) an orchestrator-driven run with the fake client.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::mcp::{
    map_call_result_for_test, McpCallResult, McpClient, McpTool, McpToolCallRequest,
    MCP_EVENT_RESULT_MAX_CHARS,
};
use crate::tools::approval::AskForApproval;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{
    AutoApprover, ExecOutput, SandboxAttempt, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions, SandboxType,
};

/// What the client was asked to do (captured for routing assertions).
#[derive(Debug, Clone, PartialEq)]
struct Recorded {
    server: String,
    tool: String,
    args: Option<Value>,
}

/// A fake MCP client returning a canned result (or an error) and recording the
/// call.
struct FakeMcpClient {
    result: Result<McpCallResult, String>,
    seen: Mutex<Vec<Recorded>>,
}

impl FakeMcpClient {
    fn ok(result: McpCallResult) -> Arc<Self> {
        Arc::new(Self {
            result: Ok(result),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn err(message: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            result: Err(message.into()),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn recorded(&self) -> Vec<Recorded> {
        self.seen.lock().unwrap().clone()
    }
}

impl McpClient for FakeMcpClient {
    fn call_tool(
        &self,
        server: &str,
        tool: &str,
        args: Option<Value>,
    ) -> anyhow::Result<McpCallResult> {
        self.seen.lock().unwrap().push(Recorded {
            server: server.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
        });
        match &self.result {
            Ok(r) => Ok(r.clone()),
            Err(msg) => Err(anyhow::anyhow!("{msg}")),
        }
    }
}

// ---- Test harness helpers (no network, no MCP server) ----------------------

fn ctx() -> ToolCtx {
    ToolCtx {
        call_id: "call-mcp".to_string(),
        tool_name: "mcp".to_string(),
        cwd: std::env::temp_dir(),
        artifact_root: std::env::temp_dir().join("artifacts"),
    }
}

fn none_launch() -> SandboxLaunch {
    SandboxLaunch {
        sandbox: SandboxType::None,
        cancel: None,
    }
}

fn none_attempt(launch: &SandboxLaunch) -> SandboxAttempt<'_> {
    SandboxAttempt {
        sandbox: SandboxType::None,
        permissions: SandboxPermissions::UseDefault,
        enforce_managed_network: false,
        launch,
        cancel: None,
    }
}

/// Run a request directly through the runtime with a `SandboxType::None`
/// attempt (no orchestrator).
async fn run_direct(tool: &McpTool, req: &McpToolCallRequest) -> Result<ExecOutput, ToolError> {
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    tool.run(req, &attempt, &ctx()).await
}

// (1) A tool call routes server/tool/args to the client and maps text content
//     into stdout with a zero exit code.
#[tokio::test]
async fn routes_call_and_maps_text_content_to_stdout() {
    let client = FakeMcpClient::ok(McpCallResult::text("hello from tool"));
    let tool = McpTool::new(client.clone());
    let req = McpToolCallRequest::new("filesystem", "read_file", json!({ "path": "/tmp/x" }));

    let out = run_direct(&tool, &req).await.expect("run ok");

    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout, "hello from tool");
    assert_eq!(out.stderr, "");

    let seen = client.recorded();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].server, "filesystem");
    assert_eq!(seen[0].tool, "read_file");
    assert_eq!(seen[0].args, Some(json!({ "path": "/tmp/x" })));
}

// A null `arguments` is forwarded as `None` (legacy dispatch takes Option<Value>).
#[tokio::test]
async fn null_arguments_forwarded_as_none() {
    let client = FakeMcpClient::ok(McpCallResult::text("ok"));
    let tool = McpTool::new(client.clone());
    let req = McpToolCallRequest::new("srv", "ping", Value::Null);

    let out = run_direct(&tool, &req).await.expect("run ok");
    assert_eq!(out.exit_code, 0);

    let seen = client.recorded();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].args, None);
}

// (2) An `is_error` result maps to a nonzero exit code with the content on
//     stderr (codex mcp_tool_call.rs:886, legacy lib.rs:13425-13426).
#[tokio::test]
async fn is_error_result_maps_to_nonzero_exit_and_stderr() {
    let client = FakeMcpClient::ok(McpCallResult::error_text("tool blew up"));
    let tool = McpTool::new(client);
    let req = McpToolCallRequest::new("srv", "boom", json!({}));

    let out = run_direct(&tool, &req)
        .await
        .expect("run returns Ok with nonzero exit");

    assert_ne!(out.exit_code, 0);
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr, "tool blew up");
}

// (3) Image / structured (non-text) content items map sensibly: an image item
//     becomes the `[image content]` marker; an unknown/structured item is
//     JSON-encoded; text items pass through. All joined with `\n` (legacy
//     mcp_result_tool_content, lib.rs:13817-13833).
#[tokio::test]
async fn image_and_structured_content_map_sensibly() {
    let result = McpCallResult {
        content: vec![
            json!({ "type": "text", "text": "before" }),
            json!({ "type": "image", "data": "BASE64", "mimeType": "image/png" }),
            json!({ "type": "resource", "resource": { "uri": "file:///x", "text": "R" } }),
        ],
        is_error: false,
        structured_content: Some(json!({ "rows": 3 })),
    };
    let client = FakeMcpClient::ok(result);
    let tool = McpTool::new(client);
    let req = McpToolCallRequest::new("srv", "mixed", json!({}));

    let out = run_direct(&tool, &req).await.expect("run ok");

    assert_eq!(out.exit_code, 0);
    let lines: Vec<&str> = out.stdout.split('\n').collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "before");
    // Image -> marker (the raw base64 is NOT surfaced to the text channel).
    assert_eq!(lines[1], "[image content]");
    assert!(!out.stdout.contains("BASE64"));
    // Unknown/structured content item -> JSON-encoded verbatim.
    assert!(lines[2].contains("\"type\":\"resource\""));
    assert!(lines[2].contains("file:///x"));
}

// (4) A client (transport) error becomes a `ToolError::Other` naming the call
//     (legacy lib.rs:13416-13419 / codex mcp_tool_call.rs:579).
#[tokio::test]
async fn client_error_becomes_tool_error() {
    let client = FakeMcpClient::err("connection reset");
    let tool = McpTool::new(client);
    let req = McpToolCallRequest::new("srv", "flaky", json!({}));

    let err = run_direct(&tool, &req)
        .await
        .expect_err("client error should surface as ToolError");

    match err {
        ToolError::Other(e) => {
            let msg = format!("{e}");
            assert!(msg.contains("MCP tool call failed"), "msg = {msg}");
            assert!(msg.contains("srv/flaky"), "msg = {msg}");
            assert!(msg.contains("connection reset"), "msg = {msg}");
        }
        other => panic!("expected ToolError::Other, got {other:?}"),
    }
}

// An empty server / tool is rejected before touching the client.
#[tokio::test]
async fn empty_server_or_tool_rejected() {
    let client = FakeMcpClient::ok(McpCallResult::text("unused"));
    let tool = McpTool::new(client);

    let req = McpToolCallRequest::new("", "t", json!({}));
    let err = run_direct(&tool, &req)
        .await
        .expect_err("empty server rejected");
    assert!(matches!(err, ToolError::Rejected(_)));

    let req = McpToolCallRequest::new("s", "", json!({}));
    let err = run_direct(&tool, &req)
        .await
        .expect_err("empty tool rejected");
    assert!(matches!(err, ToolError::Rejected(_)));
}

// (5) Orchestrator-driven test: the fake client routes through the full
//     orchestrator flow under a permissive policy and yields the mapped output.
#[tokio::test]
async fn orchestrator_drives_mcp_tool() {
    let client = FakeMcpClient::ok(McpCallResult::text("orchestrated"));
    let tool = McpTool::new(client.clone());
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let env = TurnEnv {
        file_system_sandbox_policy: FileSystemSandboxPolicy {
            restricted: false,
            denied_read: false,
        },
        managed_network_active: false,
        strict_auto_review: false,
        use_guardian: false,
    };

    let req = McpToolCallRequest::new("srv", "do_it", json!({ "k": "v" }));
    let result = orch
        .run(&tool, &req, &ctx(), &env, AskForApproval::Never)
        .await
        .expect("orchestration ok");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert_eq!(result.output.exit_code, 0);
    assert_eq!(result.output.stdout, "orchestrated");

    let seen = client.recorded();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].server, "srv");
    assert_eq!(seen[0].tool, "do_it");
}

// Namespaced-name parsing: `mcp__<server>__<tool>` splits once after the server,
// so a tool name containing `__` is preserved. Malformed names return `None`.
#[test]
fn parse_namespaced_splits_once_after_server() {
    let req = McpToolCallRequest::parse_namespaced("mcp__github__list__issues", json!({ "a": 1 }))
        .expect("well-formed namespaced name parses");
    assert_eq!(req.server, "github");
    assert_eq!(req.tool, "list__issues");
    assert_eq!(req.arguments, json!({ "a": 1 }));

    assert!(McpToolCallRequest::parse_namespaced("not_namespaced", json!({})).is_none());
    assert!(McpToolCallRequest::parse_namespaced("mcp__only", json!({})).is_none());
    assert!(McpToolCallRequest::parse_namespaced("mcp____tool", json!({})).is_none());
    assert!(McpToolCallRequest::parse_namespaced("mcp__srv__", json!({})).is_none());
}

// parallel_safe follows the server's read-only hint (codex handlers/mcp.rs:46-57).
#[test]
fn parallel_safe_follows_read_only_hint() {
    let client = FakeMcpClient::ok(McpCallResult::text("x"));
    let tool = McpTool::new(client);

    let serial = McpToolCallRequest::new("s", "write", json!({}));
    assert!(!tool.parallel_safe(&serial));

    let mut read_only = McpToolCallRequest::new("s", "read", json!({}));
    read_only.read_only = true;
    assert!(tool.parallel_safe(&read_only));
}

// The model-facing mapping does NOT apply the 20k event-log cap: a large result
// passes through uncapped (MCP_EVENT_RESULT_MAX_CHARS is event-log-only).
#[tokio::test]
async fn model_facing_output_is_not_event_log_capped() {
    let big = "z".repeat(MCP_EVENT_RESULT_MAX_CHARS + 5_000);
    let client = FakeMcpClient::ok(McpCallResult::text(big.clone()));
    let tool = McpTool::new(client);
    let req = McpToolCallRequest::new("srv", "big", json!({}));

    let out = run_direct(&tool, &req).await.expect("run ok");

    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout.len(), big.len());
    assert!(out.stdout.len() > MCP_EVENT_RESULT_MAX_CHARS);
}

// Direct unit check of the result mapping helper (success + error branches).
#[test]
fn map_call_result_branches() {
    let ok = map_call_result_for_test(McpCallResult::text("good"));
    assert_eq!(ok.exit_code, 0);
    assert_eq!(ok.stdout, "good");
    assert_eq!(ok.stderr, "");

    let bad = map_call_result_for_test(McpCallResult::error_text("bad"));
    assert_ne!(bad.exit_code, 0);
    assert_eq!(bad.stdout, "");
    assert_eq!(bad.stderr, "bad");
}
