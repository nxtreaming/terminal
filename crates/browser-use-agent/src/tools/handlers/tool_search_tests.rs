//! Tests for the async `tool_search` tool (BM25 deferred-tool ranking).
//!
//! No network / no filesystem: the catalog is injected in-process and BM25 ranks
//! it deterministically. The last test drives a call end-to-end through the
//! [`ToolOrchestrator`] with the `NoneSandboxProvider` + `AutoApprover` stubs.

use crate::tools::approval::AskForApproval;
use crate::tools::handlers::tool_search::{
    compose_search_text, ToolSearchEntry, ToolSearchMatch, ToolSearchRequest, ToolSearchTool,
    TOOL_SEARCH_DEFAULT_LIMIT, TOOL_SEARCH_STDOUT_PREFIX,
};
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{AutoApprover, ToolCtx, ToolRuntime};
use crate::tools::sandbox::{FileSystemSandboxPolicy, NoneSandboxProvider};

/// A small representative catalog of deferred tools.
fn sample_catalog() -> Vec<ToolSearchEntry> {
    vec![
        ToolSearchEntry::new(
            "create_calendar_event",
            "Create a new event on the user's calendar with a title and time.",
            ["title", "start_time", "end_time"],
        ),
        ToolSearchEntry::new(
            "send_slack_message",
            "Send a message to a Slack channel or user.",
            ["channel", "text"],
        ),
        ToolSearchEntry::new(
            "list_linear_issues",
            "List issues from a Linear project, filtered by status.",
            ["project", "status"],
        ),
    ]
}

fn tool_ctx() -> ToolCtx {
    ToolCtx {
        call_id: "call-tool-search".to_string(),
        tool_name: "tool_search".to_string(),
        cwd: std::env::temp_dir(),
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

/// (1) A query matching one entry returns it ranked first.
#[test]
fn query_matching_one_entry_ranks_it_first() {
    let tool = ToolSearchTool::new(sample_catalog());
    let matches = tool.search("calendar event", 5);
    assert!(!matches.is_empty(), "expected at least one match");
    assert_eq!(
        matches[0].name, "create_calendar_event",
        "the calendar tool should rank first for a calendar query; got {matches:?}"
    );
}

/// (2) `limit` caps the number of results.
#[test]
fn limit_caps_number_of_results() {
    let tool = ToolSearchTool::new(sample_catalog());
    // A broad query that all three entries plausibly share some terms with.
    let limited = tool.search("message event issues channel project", 1);
    assert!(
        limited.len() <= 1,
        "limit=1 must cap results to at most 1; got {} ({limited:?})",
        limited.len()
    );
}

/// (3a) An empty query is rejected gracefully (no panic).
#[tokio::test]
async fn empty_query_is_rejected() {
    let tool = ToolSearchTool::new(sample_catalog());
    let req = ToolSearchRequest::new("   ");
    let err = run_direct(&tool, &req)
        .await
        .expect_err("empty query rejects");
    match err {
        crate::tools::runtime::ToolError::Rejected(msg) => {
            assert!(
                msg.contains("query must not be empty"),
                "unexpected reject message: {msg}"
            );
        }
        other => panic!("expected Rejected for empty query, got {other:?}"),
    }
}

/// (3b) An empty catalog is handled gracefully: a valid query returns no matches
/// (not an error, not a panic).
#[tokio::test]
async fn empty_catalog_returns_no_matches() {
    let tool = ToolSearchTool::new(Vec::new());
    // Direct search yields nothing.
    assert!(tool.search("anything", 5).is_empty());

    // And a full run returns an empty JSON list in stdout.
    let req = ToolSearchRequest::new("anything");
    let out = run_direct(&tool, &req)
        .await
        .expect("empty catalog runs ok");
    assert_eq!(out.exit_code, 0);
    let json = out
        .stdout
        .strip_prefix(TOOL_SEARCH_STDOUT_PREFIX)
        .expect("stdout carries the tool_search prefix");
    let parsed: Vec<ToolSearchMatch> =
        serde_json::from_str(json).expect("payload is a JSON match list");
    assert!(parsed.is_empty(), "empty catalog should yield no matches");
}

/// (3c) A zero limit is rejected gracefully.
#[tokio::test]
async fn zero_limit_is_rejected() {
    let tool = ToolSearchTool::new(sample_catalog());
    let req = ToolSearchRequest::with_limit("calendar", 0);
    let err = run_direct(&tool, &req)
        .await
        .expect_err("zero limit rejects");
    match err {
        crate::tools::runtime::ToolError::Rejected(msg) => {
            assert!(
                msg.contains("limit must be greater than zero"),
                "unexpected reject message: {msg}"
            );
        }
        other => panic!("expected Rejected for zero limit, got {other:?}"),
    }
}

/// (4) Ranking is sensible: a query term present in a name/description ranks that
/// entry above unrelated ones.
#[test]
fn ranking_prefers_term_present_entry_over_unrelated() {
    let tool = ToolSearchTool::new(sample_catalog());

    // "slack" appears only in the Slack tool; it must out-rank the others.
    let slack = tool.search("slack", 5);
    assert_eq!(
        slack.first().map(|m| m.name.as_str()),
        Some("send_slack_message"),
        "a 'slack' query should surface the slack tool first; got {slack:?}"
    );

    // "linear issues" appears only in the Linear tool.
    let linear = tool.search("linear issues", 5);
    assert_eq!(
        linear.first().map(|m| m.name.as_str()),
        Some("list_linear_issues"),
        "a 'linear issues' query should surface the linear tool first; got {linear:?}"
    );
}

/// `compose_search_text` includes the name, description, and (sorted) schema
/// property names, so a property-only term is searchable.
#[test]
fn search_text_includes_name_description_and_properties() {
    let text = compose_search_text(
        "create_calendar_event",
        "Create a new event.",
        ["start_time", "title"],
    );
    assert!(text.contains("create_calendar_event"));
    assert!(text.contains("Create a new event."));
    assert!(text.contains("start_time"));
    assert!(text.contains("title"));

    // A query on a schema-property term ranks the entry carrying it.
    let tool = ToolSearchTool::new(sample_catalog());
    let by_prop = tool.search("end_time", 5);
    assert_eq!(
        by_prop.first().map(|m| m.name.as_str()),
        Some("create_calendar_event"),
        "a schema-property term should surface its owning tool; got {by_prop:?}"
    );
}

/// The default limit applies when the request omits `limit`.
#[test]
fn default_limit_is_used_when_unspecified() {
    let req = ToolSearchRequest::new("calendar");
    assert_eq!(req.limit, None);
    // Sanity on the constant the runtime falls back to.
    assert!(TOOL_SEARCH_DEFAULT_LIMIT > 0);
}

/// `tool_search` is parallel-safe (codex overrides `supports_parallel_tool_calls
/// -> true`).
#[test]
fn tool_search_is_parallel_safe() {
    let tool = ToolSearchTool::new(sample_catalog());
    let req = ToolSearchRequest::new("calendar");
    assert!(
        tool.parallel_safe(&req),
        "tool_search must be parallel-safe to match codex"
    );
}

/// (5) Drive one call end-to-end through the orchestrator with the stub sandbox
/// + auto-approver.
#[tokio::test]
async fn orchestrated_run_returns_ranked_matches() {
    let tool = ToolSearchTool::new(sample_catalog());
    let orchestrator = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let ctx = tool_ctx();
    let env = turn_env();

    let req = ToolSearchRequest::new("calendar event");
    let result = orchestrator
        .run(&tool, &req, &ctx, &env, AskForApproval::OnRequest)
        .await
        .expect("orchestrated tool_search should succeed");

    let out = result.output;
    assert_eq!(out.exit_code, 0);
    assert!(out.stderr.is_empty());

    let json = out
        .stdout
        .strip_prefix(TOOL_SEARCH_STDOUT_PREFIX)
        .expect("stdout carries the tool_search prefix");
    let matches: Vec<ToolSearchMatch> =
        serde_json::from_str(json).expect("payload is a JSON match list");
    assert!(!matches.is_empty(), "expected ranked matches");
    assert_eq!(
        matches[0].name, "create_calendar_event",
        "calendar query should rank the calendar tool first; got {matches:?}"
    );
}

/// The request round-trips through serde with the exact wire field shape
/// (`query` required, `limit` optional/omitted when `None`).
#[test]
fn request_serde_round_trip_and_wire_shape() {
    // limit omitted on the wire when None.
    let req = ToolSearchRequest::new("hello");
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json, serde_json::json!({ "query": "hello" }));

    // limit present when Some.
    let req2 = ToolSearchRequest::with_limit("hello", 3);
    let json2 = serde_json::to_value(&req2).unwrap();
    assert_eq!(json2, serde_json::json!({ "query": "hello", "limit": 3 }));

    // Decoding tolerates an omitted limit (serde default).
    let decoded: ToolSearchRequest =
        serde_json::from_value(serde_json::json!({ "query": "x" })).unwrap();
    assert_eq!(decoded, ToolSearchRequest::new("x"));
}

/// Helper: run the tool directly through a stub orchestrator attempt.
async fn run_direct(
    tool: &ToolSearchTool,
    req: &ToolSearchRequest,
) -> Result<crate::tools::runtime::ExecOutput, crate::tools::runtime::ToolError> {
    let orchestrator = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let ctx = tool_ctx();
    let env = turn_env();
    orchestrator
        .run(tool, req, &ctx, &env, AskForApproval::Never)
        .await
        .map(|r| r.output)
}
