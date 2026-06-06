//! Tests for the async `search` tool ([`SearchTool`]).
//!
//! No real network is touched: the pure parsing/formatting/URL helpers are
//! exercised against fixture HTML, and the `run` path is driven through a fake
//! [`SearchBackend`] (mirroring `update_plan_tests` / `tool_search_tests`).

use std::sync::Arc;

use super::search::{
    classify_response, extract_real_url, format_results, normalize_whitespace, parse_lite_results,
    SearchBackend, SearchError, SearchRequest, SearchResult, SearchTool, SEARCH_TOOL_NAME,
};
use crate::tools::approval::AskForApproval;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{
    Approvable, AutoApprover, SandboxAttempt, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions, SandboxType,
};

// ---- test scaffolding (mirrors update_plan_tests) -------------------------

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

fn ctx() -> ToolCtx {
    ToolCtx {
        call_id: "test-call".to_string(),
        tool_name: "search".to_string(),
        cwd: std::env::temp_dir(),
        artifact_root: std::env::temp_dir().join("artifacts"),
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

/// A fake backend returning a canned HTML body (no network).
struct HtmlBackend(String);

#[async_trait::async_trait]
impl SearchBackend for HtmlBackend {
    async fn fetch(&self, _query: &str) -> Result<String, SearchError> {
        Ok(self.0.clone())
    }
}

/// A fake backend returning a challenge error (no network).
struct ChallengeBackend;

#[async_trait::async_trait]
impl SearchBackend for ChallengeBackend {
    async fn fetch(&self, _query: &str) -> Result<String, SearchError> {
        Err(SearchError::Challenge)
    }
}

/// A small, realistic DuckDuckGo Lite results fixture exercising: a redirect
/// URL, an entity in the snippet, a "More info" link (skipped), a duplicate
/// (deduped), a `duckduckgo.com` target (skipped), a direct (non-redirect) link,
/// and a result without a snippet.
const FIXTURE: &str = r#"
<html><body>
<table>
  <tr><td valign="top">1.&nbsp;</td>
      <td><a rel="nofollow" class="result-link"
             href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=aaa">The Rust Programming Language</a></td></tr>
  <tr><td>&nbsp;</td>
      <td class="result-snippet">A language empowering everyone to build reliable &amp; efficient software &#x2014; fast.</td></tr>
  <tr><td><span class="link-text">www.rust-lang.org</span></td></tr>

  <tr><td valign="top">2.&nbsp;</td>
      <td><a class="result-link"
             href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=bbb">Rust (duplicate target)</a></td></tr>
  <tr><td class="result-snippet">duplicate should be dropped</td></tr>

  <tr><td><a class="result-link"
             href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fduckduckgo.com%2Fabout">DuckDuckGo About</a></td></tr>
  <tr><td class="result-snippet">a duckduckgo.com target, should be dropped</td></tr>

  <tr><td><a class="result-link"
             href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fmore">More info</a></td></tr>

  <tr><td><a class="result-link" href="https://direct.example.com/page">Direct Link No Redirect</a></td></tr>
  <tr><td class="result-snippet">direct link snippet</td></tr>

  <tr><td><a class="result-link"
             href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fno-snippet.example.com%2F">No Snippet Result</a></td></tr>
</table>
</body></html>
"#;

// ---- pure helpers: normalize_whitespace -----------------------------------

#[test]
fn normalize_whitespace_collapses_and_trims() {
    assert_eq!(normalize_whitespace("  a \n\t b   c \r\n"), "a b c");
    assert_eq!(normalize_whitespace("single"), "single");
    assert_eq!(normalize_whitespace("   "), "");
}

// ---- pure helpers: extract_real_url ---------------------------------------

#[test]
fn extract_real_url_unwraps_ddg_redirect() {
    let raw = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage%3Fx%3D1&rut=abc";
    assert_eq!(
        extract_real_url(raw),
        Some("https://example.com/page?x=1".to_string())
    );
}

#[test]
fn extract_real_url_decodes_plus_as_space() {
    // `parse_qs` semantics: `+` in a query value decodes to a space.
    let raw = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa+b";
    assert_eq!(
        extract_real_url(raw),
        Some("https://example.com/a b".to_string())
    );
}

#[test]
fn extract_real_url_adds_scheme_to_protocol_relative() {
    assert_eq!(
        extract_real_url("//example.com/x"),
        Some("https://example.com/x".to_string())
    );
}

#[test]
fn extract_real_url_passes_through_plain_http() {
    assert_eq!(
        extract_real_url("https://example.com/"),
        Some("https://example.com/".to_string())
    );
    assert_eq!(
        extract_real_url("http://example.com/"),
        Some("http://example.com/".to_string())
    );
}

#[test]
fn extract_real_url_drops_ads_and_unsafe_and_empty() {
    // Ad links.
    assert_eq!(
        extract_real_url("//duckduckgo.com/y.js?ad_provider=x"),
        None
    );
    // Non-http(s) schemes.
    assert_eq!(extract_real_url("javascript:alert(1)"), None);
    assert_eq!(extract_real_url("data:text/html,hi"), None);
    // Empty.
    assert_eq!(extract_real_url(""), None);
}

// ---- pure helpers: parse_lite_results -------------------------------------

#[test]
fn parse_lite_results_extracts_decodes_dedupes_and_filters() {
    let results = parse_lite_results(FIXTURE);

    // Kept, in order: rust-lang (redirect), direct link, no-snippet result.
    // Dropped: duplicate target, duckduckgo.com target, "More info" title.
    let titles: Vec<&str> = results.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(
        titles,
        vec![
            "The Rust Programming Language",
            "Direct Link No Redirect",
            "No Snippet Result",
        ]
    );

    // First result: redirect unwrapped + snippet entity-decoded + normalized.
    assert_eq!(results[0].url, "https://www.rust-lang.org/");
    assert_eq!(
        results[0].description,
        "A language empowering everyone to build reliable & efficient software — fast."
    );

    // Direct (non-redirect) link is passed through with its own snippet.
    assert_eq!(results[1].url, "https://direct.example.com/page");
    assert_eq!(results[1].description, "direct link snippet");

    // A result with no following snippet gets an empty description.
    assert_eq!(results[2].url, "https://no-snippet.example.com/");
    assert_eq!(results[2].description, "");
}

#[test]
fn parse_lite_results_handles_empty_and_resultless_html() {
    assert!(parse_lite_results("").is_empty());
    assert!(parse_lite_results("<html><body>no results here</body></html>").is_empty());
}

/// Inline markup inside a title/snippet, real whitespace runs, and a broadened
/// named entity: exercises `text_from_html` tag-stripping (both separators),
/// `normalize_whitespace` via the parse path, and the entity table.
#[test]
fn parse_lite_results_strips_inline_markup_and_collapses_whitespace() {
    let html = "<table>\
        <tr><td><a class=\"result-link\" href=\"https://book.example.com/\">The <b>Rust</b> Book</a></td></tr>\
        <tr><td class=\"result-snippet\"><b>Tokio</b>   is   an\n        async runtime for caf&eacute; &amp; more.</td></tr>\
        </table>";
    let results = parse_lite_results(html);
    assert_eq!(results.len(), 1);
    // Title: tags stripped (separator ""), single-spaced.
    assert_eq!(results[0].title, "The Rust Book");
    assert_eq!(results[0].url, "https://book.example.com/");
    // Snippet: tags -> space, &eacute;/&amp; decoded, whitespace runs collapsed.
    assert_eq!(
        results[0].description,
        "Tokio is an async runtime for café & more."
    );
}

#[test]
fn parse_lite_results_filters_duckduckgo_hosts_without_dropping_mentions_elsewhere() {
    let html = r#"
    <html><body><table>
      <tr><td><a class="result-link"
             href="https://example.com/articles/duckduckgo.com-review">Valid Mention</a></td></tr>
      <tr><td class="result-snippet">kept</td></tr>
      <tr><td><a class="result-link"
             href="https://duckduckgo.com/about">DuckDuckGo About</a></td></tr>
      <tr><td class="result-snippet">dropped</td></tr>
    </table></body></html>
    "#;

    let results = parse_lite_results(html);

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "Valid Mention");
    assert_eq!(
        results[0].url,
        "https://example.com/articles/duckduckgo.com-review"
    );
}

// ---- pure helpers: format_results -----------------------------------------

#[test]
fn format_results_renders_header_and_numbered_entries() {
    let results = vec![
        SearchResult {
            title: "First".to_string(),
            url: "https://a.example/".to_string(),
            description: "first snippet".to_string(),
        },
        SearchResult {
            title: "Second".to_string(),
            url: "https://b.example/".to_string(),
            description: String::new(),
        },
    ];
    let out = format_results("my query", &results);

    assert!(
        out.contains("Search results for \"my query\" (2 results):"),
        "got: {out}"
    );
    assert!(
        out.contains("do NOT navigate to a search engine"),
        "got: {out}"
    );
    assert!(out.contains("1. First"), "got: {out}");
    assert!(out.contains("   URL: https://a.example/"), "got: {out}");
    assert!(out.contains("   first snippet"), "got: {out}");
    assert!(out.contains("2. Second"), "got: {out}");
    assert!(out.contains("   URL: https://b.example/"), "got: {out}");
}

#[test]
fn format_results_truncates_long_title_and_description() {
    let results = vec![SearchResult {
        title: "ThisIsAVeryLongResultTitleThatExceedsThirtyCharacters".to_string(),
        url: "https://example.com/keep/this/whole/url".to_string(),
        description: "d".repeat(250),
    }];
    let out = format_results("q", &results);

    // Title capped at 30 characters including the ellipsis.
    let title = out
        .lines()
        .find_map(|l| l.strip_prefix("1. "))
        .expect("title line");
    assert_eq!(title.chars().count(), 30, "title capped at 30: {title:?}");
    assert!(title.ends_with('…'), "title ellipsized: {title:?}");
    assert!(
        title.starts_with("ThisIsAVeryLong"),
        "title prefix: {title:?}"
    );
    assert!(!out.contains("Characters"), "tail must be dropped: {out}");

    // URL is kept intact (not truncated).
    assert!(
        out.contains("https://example.com/keep/this/whole/url"),
        "url kept: {out}"
    );

    // Description capped at 125 characters including the ellipsis.
    let desc_line = out.lines().find(|l| l.starts_with("   d")).expect("desc");
    let desc = desc_line.strip_prefix("   ").unwrap();
    assert_eq!(desc.chars().count(), 125, "description capped at 125");
    assert!(desc.ends_with('…'), "description ellipsized: {desc:?}");
}

// ---- pure helpers: classify_response --------------------------------------

#[test]
fn classify_response_flags_challenge_status_and_anomaly_body() {
    assert!(matches!(
        classify_response(202, "anything"),
        Err(SearchError::Challenge)
    ));
    assert!(matches!(
        classify_response(200, "...Anomaly detected..."),
        Err(SearchError::Challenge)
    ));
}

#[test]
fn classify_response_flags_http_errors_with_snippet() {
    let body = "x".repeat(500);
    match classify_response(503, &body) {
        Err(SearchError::Http { status, snippet }) => {
            assert_eq!(status, 503);
            assert_eq!(
                snippet.chars().count(),
                200,
                "snippet truncated to 200 chars"
            );
        }
        other => panic!("expected Http error, got {other:?}"),
    }
}

#[test]
fn classify_response_flags_4xx_and_pins_the_400_boundary() {
    // 4xx is the case the port must handle (not just 5xx).
    match classify_response(404, "not found") {
        Err(SearchError::Http { status, snippet }) => {
            assert_eq!(status, 404);
            assert_eq!(snippet, "not found");
        }
        other => panic!("expected Http error, got {other:?}"),
    }
    // The 399-ok / 400-error boundary pins against an off-by-one in `>= 400`.
    assert!(classify_response(399, "ok").is_ok());
    assert!(matches!(
        classify_response(400, "bad"),
        Err(SearchError::Http { status: 400, .. })
    ));
}

#[test]
fn classify_response_accepts_ok() {
    assert!(classify_response(200, "<html>fine</html>").is_ok());
}

// ---- run() through the fake backend ---------------------------------------

#[tokio::test]
async fn run_formats_results_from_backend_html() {
    let tool = SearchTool::with_backend(Arc::new(HtmlBackend(FIXTURE.to_string())));
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let out = tool
        .run(&SearchRequest::new("rust"), &attempt, &ctx())
        .await
        .unwrap();

    assert_eq!(out.exit_code, 0);
    assert!(out.stderr.is_empty());
    assert!(
        out.stdout
            .contains("Search results for \"rust\" (3 results):"),
        "got: {}",
        out.stdout
    );
    // This title (29 chars) is within the 30-char cap, so it appears in full.
    assert!(
        out.stdout.contains("The Rust Programming Language"),
        "got: {}",
        out.stdout
    );
    // URLs are kept intact.
    assert!(
        out.stdout.contains("https://www.rust-lang.org/"),
        "got: {}",
        out.stdout
    );
}

#[tokio::test]
async fn run_reports_no_results() {
    let tool = SearchTool::with_backend(Arc::new(HtmlBackend(
        "<html><body>nothing</body></html>".to_string(),
    )));
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let out = tool
        .run(&SearchRequest::new("obscure"), &attempt, &ctx())
        .await
        .unwrap();

    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout, "No results found for \"obscure\".");
}

#[tokio::test]
async fn run_rejects_empty_query() {
    let tool = SearchTool::with_backend(Arc::new(HtmlBackend(String::new())));
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let err = tool
        .run(&SearchRequest::new("   "), &attempt, &ctx())
        .await
        .unwrap_err();
    let ToolError::Rejected(msg) = err else {
        panic!("expected Rejected, got {err:?}");
    };
    assert!(msg.contains("must not be empty"), "got: {msg}");
}

#[tokio::test]
async fn run_surfaces_backend_failure_as_soft_error() {
    let tool = SearchTool::with_backend(Arc::new(ChallengeBackend));
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let out = tool
        .run(&SearchRequest::new("rust"), &attempt, &ctx())
        .await
        .unwrap();

    // A fetch failure is a soft, model-visible error (nonzero exit + stderr),
    // not a hard tool error.
    assert_eq!(out.exit_code, 1);
    assert!(out.stdout.is_empty());
    assert!(
        out.stderr.contains("Search failed:") && out.stderr.contains("challenge"),
        "got: {}",
        out.stderr
    );
}

// ---- accessors + parallel-safety ------------------------------------------

#[test]
fn approval_accessors() {
    let tool = SearchTool::with_backend(Arc::new(HtmlBackend(String::new())));
    let req = SearchRequest::new("rust");
    assert_eq!(tool.approval_keys(&req).len(), 1, "one key per call");
    assert_eq!(
        tool.sandbox_permissions(&req),
        SandboxPermissions::UseDefault
    );
    assert!(tool.exec_approval_requirement(&req).is_none());
}

#[test]
fn search_is_parallel_safe() {
    let tool = SearchTool::with_backend(Arc::new(HtmlBackend(String::new())));
    assert!(tool.parallel_safe(&SearchRequest::new("rust")));
}

#[test]
fn tool_name_is_search() {
    assert_eq!(SEARCH_TOOL_NAME, "search");
    let tool = SearchTool::with_backend(Arc::new(HtmlBackend(String::new())));
    assert_eq!(tool.name(), "search");
}

#[test]
fn request_round_trips_wire_shape() {
    let json = r#"{"query":"hello world"}"#;
    let req: SearchRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.query, "hello world");
    let out = serde_json::to_string(&req).unwrap();
    assert_eq!(out, json);
}

// ---- drive a call through the orchestrator over the seam -------------------

#[tokio::test]
async fn orchestrated_search_completes_under_none() {
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = SearchTool::with_backend(Arc::new(HtmlBackend(FIXTURE.to_string())));

    let result = orch
        .run(
            &tool,
            &SearchRequest::new("rust"),
            &ctx(),
            &turn_env(),
            AskForApproval::Never,
        )
        .await
        .expect("orchestration ok");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert_eq!(result.output.exit_code, 0);
    // Within the 30-char title cap, so it appears in full.
    assert!(
        result
            .output
            .stdout
            .contains("The Rust Programming Language"),
        "got: {}",
        result.output.stdout
    );
}

// ---- live smoke (ignored: hits the real DuckDuckGo endpoint) --------------

/// End-to-end check against the REAL DuckDuckGo Lite endpoint via the default
/// [`HttpSearchBackend`]. Ignored by default (network + non-deterministic, and
/// DuckDuckGo may rate-limit/serve a challenge). Run it manually with:
///
/// ```text
/// cargo test -p browser-use-agent --lib -- --ignored --nocapture search_live_smoke
/// ```
#[tokio::test]
#[ignore = "hits the live DuckDuckGo Lite endpoint"]
async fn search_live_smoke() {
    let tool = SearchTool::new();
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let out = tool
        .run(
            &SearchRequest::new("rust programming language"),
            &attempt,
            &ctx(),
        )
        .await
        .expect("run ok");

    eprintln!(
        "exit_code={}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.exit_code, out.stdout, out.stderr
    );
    // A challenge/CAPTCHA is a legitimate live outcome (exit 1 + message); only
    // assert hard on the success shape so the test documents both paths.
    if out.exit_code == 0 {
        assert!(
            out.stdout.contains("Search results for") || out.stdout.contains("No results found"),
            "unexpected stdout: {}",
            out.stdout
        );
    } else {
        assert!(
            out.stderr.contains("Search failed:"),
            "unexpected stderr: {}",
            out.stderr
        );
    }
}
