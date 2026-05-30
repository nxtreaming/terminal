//! Tests for the hosted `web_search` tool.
//!
//! These tests are NETWORK-FREE by construction: `web_search` is a hosted,
//! provider-executed tool, so there is nothing to fetch locally. They exercise
//! (1) `WebSearchConfig` serde round-tripping to codex's thin hosted wire shape,
//! (2) the tool declaration/spec (name + hosted marker), (3) the `run`
//! passthrough/hosted-marker behavior (NOT a real search), and (4) the
//! mode/enabled config gating (disabled vs enabled).

use std::path::PathBuf;

use crate::tools::handlers::web_search::{
    web_search_action_detail, web_search_detail, WebSearchAction, WebSearchConfig, WebSearchMode,
    WebSearchRequest, WebSearchTool, WEB_SEARCH_HOSTED_PREFIX, WEB_SEARCH_TOOL_NAME,
};
use crate::tools::runtime::{
    Approvable, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxLaunch, SandboxPermissions, SandboxPreference, SandboxType};

// ---- Test harness helpers (no network, no process) -------------------------

fn ctx() -> ToolCtx {
    ToolCtx {
        call_id: "web_search-test".to_string(),
        tool_name: WEB_SEARCH_TOOL_NAME.to_string(),
        cwd: PathBuf::from("/tmp"),
    }
}

fn none_launch() -> SandboxLaunch {
    SandboxLaunch {
        sandbox: SandboxType::None,
        cancel: None,
    }
}

fn attempt(launch: &SandboxLaunch) -> SandboxAttempt<'_> {
    SandboxAttempt {
        sandbox: SandboxType::None,
        permissions: SandboxPermissions::UseDefault,
        enforce_managed_network: false,
        launch,
        cancel: None,
    }
}

async fn run_tool(tool: &WebSearchTool, req: &WebSearchRequest) -> Result<String, ToolError> {
    let launch = none_launch();
    let out = tool.run(req, &attempt(&launch), &ctx()).await?;
    assert_eq!(out.exit_code, 0, "hosted passthrough should report success");
    assert!(out.stderr.is_empty(), "hosted passthrough writes no stderr");
    Ok(out.stdout)
}

// ---- (1) WebSearchConfig serde round-trips to codex's wire shape -----------

#[test]
fn config_disabled_serde_round_trips_to_wire_shape() {
    let cfg = WebSearchConfig::disabled();
    let json = serde_json::to_value(&cfg).expect("serialize");
    // Disabled config: mode is the snake_case "disabled" string; allowed_domains
    // is absent (skipped when None), matching codex's thin config.
    assert_eq!(json, serde_json::json!({ "mode": "disabled" }));

    let back: WebSearchConfig = serde_json::from_value(json).expect("deserialize");
    assert_eq!(back, cfg);
    assert!(!back.is_enabled());
}

#[test]
fn config_enabled_serde_round_trips_to_wire_shape() {
    let cfg = WebSearchConfig::enabled();
    let json = serde_json::to_value(&cfg).expect("serialize");
    assert_eq!(json, serde_json::json!({ "mode": "enabled" }));

    let back: WebSearchConfig = serde_json::from_value(json).expect("deserialize");
    assert_eq!(back, cfg);
    assert!(back.is_enabled());
}

#[test]
fn config_enabled_with_domains_serde_round_trips() {
    let cfg = WebSearchConfig::enabled_for(["example.com", "docs.rs"]);
    let json = serde_json::to_value(&cfg).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "mode": "enabled",
            "allowed_domains": ["example.com", "docs.rs"],
        })
    );

    let back: WebSearchConfig = serde_json::from_value(json).expect("deserialize");
    assert_eq!(back, cfg);
    assert_eq!(
        back.allowed_domains.as_deref(),
        Some(["example.com".to_string(), "docs.rs".to_string()].as_slice())
    );
}

#[test]
fn config_default_is_disabled() {
    // The default config must NOT offer the hosted tool (codex/legacy default:
    // web_search off unless explicitly enabled).
    let cfg = WebSearchConfig::default();
    assert_eq!(cfg.mode, WebSearchMode::Disabled);
    assert!(!cfg.is_enabled());
}

#[test]
fn config_deserializes_from_empty_object_as_disabled() {
    // `mode` is `#[serde(default)]`, so an empty config object yields the disabled
    // default rather than failing — matching codex's "absent flag => off".
    let back: WebSearchConfig = serde_json::from_value(serde_json::json!({})).expect("deserialize");
    assert_eq!(back, WebSearchConfig::disabled());
    assert!(!back.is_enabled());
}

#[test]
fn mode_wire_strings_match_codex_snake_case() {
    assert_eq!(
        serde_json::to_value(WebSearchMode::Disabled).unwrap(),
        serde_json::json!("disabled")
    );
    assert_eq!(
        serde_json::to_value(WebSearchMode::Enabled).unwrap(),
        serde_json::json!("enabled")
    );
}

// ---- (2) Tool declaration / spec: name + hosted marker ---------------------

#[test]
fn tool_name_is_web_search() {
    assert_eq!(WEB_SEARCH_TOOL_NAME, "web_search");
    let tool = WebSearchTool::new(WebSearchConfig::enabled());
    assert_eq!(tool.name(), "web_search");
}

#[test]
fn tool_is_hosted_marker() {
    // The defining property: web_search is a HOSTED (provider-executed) tool.
    let enabled = WebSearchTool::new(WebSearchConfig::enabled());
    assert!(enabled.is_hosted(), "web_search must be marked hosted");
    let disabled = WebSearchTool::disabled();
    assert!(
        disabled.is_hosted(),
        "hosted-ness is intrinsic, independent of enabled state"
    );
}

#[test]
fn tool_carries_its_config() {
    let cfg = WebSearchConfig::enabled_for(["example.com"]);
    let tool = WebSearchTool::new(cfg.clone());
    assert_eq!(tool.config(), &cfg);
    assert!(tool.is_enabled());
}

// ---- (3) ToolRuntime run: passthrough / hosted marker (NOT a real search) --

#[tokio::test]
async fn run_passes_through_provider_supplied_result() {
    let tool = WebSearchTool::new(WebSearchConfig::enabled());
    let req = WebSearchRequest::with_provider_result(
        "rust async runtimes",
        "PROVIDER RESULT: tokio, async-std, smol",
    );
    let stdout = run_tool(&tool, &req).await.expect("hosted passthrough");
    // The provider-executed result is passed through verbatim — no local search,
    // no rewriting.
    assert_eq!(stdout, "PROVIDER RESULT: tokio, async-std, smol");
}

#[tokio::test]
async fn run_without_provider_result_emits_hosted_marker_not_a_fake_search() {
    let tool = WebSearchTool::new(WebSearchConfig::enabled());
    let req = WebSearchRequest::new("who won the 2026 world cup");
    let stdout = run_tool(&tool, &req).await.expect("hosted marker");
    // Absent a provider-supplied result, run emits a HOSTED MARKER making clear
    // the result is provider-side — it does NOT fabricate a search answer.
    assert!(
        stdout.starts_with(WEB_SEARCH_HOSTED_PREFIX),
        "expected hosted marker prefix, got: {stdout}"
    );
    assert!(
        stdout.contains("provider-executed"),
        "marker must state the tool is provider-executed"
    );
    // The query is surfaced in the marker detail but no answer is fabricated.
    assert!(stdout.contains("who won the 2026 world cup"));
}

#[tokio::test]
async fn run_uses_action_detail_in_marker() {
    let tool = WebSearchTool::new(WebSearchConfig::enabled());
    let req = WebSearchRequest {
        query: "fallback query".to_string(),
        action: Some(WebSearchAction::OpenPage {
            url: Some("https://example.com/page".to_string()),
        }),
        provider_result: None,
    };
    let stdout = run_tool(&tool, &req).await.expect("hosted marker");
    // The structured action detail (the opened URL) is preferred over the raw
    // query, mirroring codex's web_search_detail.
    assert!(stdout.contains("https://example.com/page"), "got: {stdout}");
}

#[tokio::test]
async fn run_rejects_when_disabled() {
    // A disabled hosted tool should never have been offered to the provider; a
    // dispatch is a configuration error, not a search to perform.
    let tool = WebSearchTool::disabled();
    let req = WebSearchRequest::with_provider_result("q", "should not matter");
    let err = run_tool(&tool, &req)
        .await
        .expect_err("disabled must reject");
    match err {
        ToolError::Rejected(msg) => assert!(msg.contains("disabled"), "got: {msg}"),
        other => panic!("expected Rejected, got {other:?}"),
    }
}

// ---- (4) mode / enabled config behaves (disabled vs enabled) ---------------

#[test]
fn enabled_vs_disabled_gating() {
    assert!(WebSearchTool::new(WebSearchConfig::enabled()).is_enabled());
    assert!(WebSearchTool::new(WebSearchConfig::enabled_for(["x.com"])).is_enabled());
    assert!(!WebSearchTool::disabled().is_enabled());
    assert!(!WebSearchTool::new(WebSearchConfig::disabled()).is_enabled());
}

#[test]
fn mode_is_enabled_helper() {
    assert!(WebSearchMode::Enabled.is_enabled());
    assert!(!WebSearchMode::Disabled.is_enabled());
}

// ---- Codex display-helper parity (core/src/web_search.rs:3-39) -------------

#[test]
fn action_detail_prefers_single_query() {
    let action = WebSearchAction::Search {
        query: Some("rust ownership".to_string()),
        queries: Some(vec!["ignored".to_string(), "also ignored".to_string()]),
    };
    assert_eq!(web_search_action_detail(&action), "rust ownership");
}

#[test]
fn action_detail_falls_back_to_first_of_many_queries_with_ellipsis() {
    let action = WebSearchAction::Search {
        query: None,
        queries: Some(vec!["first".to_string(), "second".to_string()]),
    };
    // Codex: more than one query and a non-empty first => "first ...".
    assert_eq!(web_search_action_detail(&action), "first ...");
}

#[test]
fn action_detail_single_of_one_query_no_ellipsis() {
    let action = WebSearchAction::Search {
        query: None,
        queries: Some(vec!["only".to_string()]),
    };
    assert_eq!(web_search_action_detail(&action), "only");
}

#[test]
fn action_detail_find_in_page_variants() {
    assert_eq!(
        web_search_action_detail(&WebSearchAction::FindInPage {
            url: Some("https://e.com".to_string()),
            pattern: Some("foo".to_string()),
        }),
        "'foo' in https://e.com"
    );
    assert_eq!(
        web_search_action_detail(&WebSearchAction::FindInPage {
            url: None,
            pattern: Some("foo".to_string()),
        }),
        "'foo'"
    );
    assert_eq!(
        web_search_action_detail(&WebSearchAction::FindInPage {
            url: Some("https://e.com".to_string()),
            pattern: None,
        }),
        "https://e.com"
    );
    assert_eq!(
        web_search_action_detail(&WebSearchAction::FindInPage {
            url: None,
            pattern: None,
        }),
        ""
    );
}

#[test]
fn action_detail_other_is_empty() {
    assert_eq!(web_search_action_detail(&WebSearchAction::Other), "");
}

#[test]
fn detail_falls_back_to_query_when_action_detail_empty() {
    // No action => fall back to the raw query.
    assert_eq!(web_search_detail(None, "raw query"), "raw query");
    // Action that renders empty (Other) => fall back to the raw query.
    assert_eq!(
        web_search_detail(Some(&WebSearchAction::Other), "raw query"),
        "raw query"
    );
    // Action that renders non-empty => use the action detail.
    assert_eq!(
        web_search_detail(
            Some(&WebSearchAction::OpenPage {
                url: Some("https://x".to_string()),
            }),
            "raw query"
        ),
        "https://x"
    );
}

// ---- Approval / sandbox seam parity (non-FS, benign, parallel-safe) --------

#[test]
fn approval_and_sandbox_seam_is_benign() {
    let tool = WebSearchTool::new(WebSearchConfig::enabled());
    let req = WebSearchRequest::new("q");

    // One approval key, derived from the query.
    let keys = tool.approval_keys(&req);
    assert_eq!(keys.len(), 1);

    // No FS escalation, no intrinsic approval requirement (provider-executed).
    assert_eq!(
        tool.sandbox_permissions(&req),
        SandboxPermissions::UseDefault
    );
    assert!(tool.exec_approval_requirement(&req).is_none());

    // Non-FS sandbox seam, uniform with the other benign tools.
    assert_eq!(tool.sandbox_preference(), SandboxPreference::Auto);
    assert!(tool.escalate_on_failure());

    // Hosted passthrough is parallel-safe.
    assert!(tool.parallel_safe(&req));
}
