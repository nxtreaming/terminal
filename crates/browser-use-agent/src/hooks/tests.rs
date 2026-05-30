//! Network-free hook-runtime tests with a fake [`CommandRunner`].
//!
//! These mirror codex `hook_runtime_tests.rs`
//! (`/home/exedev/repos/codex/codex-rs/core/src/hook_runtime_tests.rs`) and
//! add coverage for the sanctioned additions (Prompt / Agent / PermissionRequest
//! kinds + the PermissionRequest event emission). No real process is spawned and
//! no network is touched: every hook runs through the in-memory [`FakeRunner`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use crate::events::EventSink;
use crate::events::PendingEvent;

use super::config::HookCommand;
use super::config::HookMatcherGroup;
use super::config::HooksConfig;
use super::event::HookDecision;
use super::event::HookEvent;
use super::event::HookInput;
use super::runtime::CommandOutput;
use super::runtime::CommandRunner;
use super::runtime::HookRuntime;
use super::runtime::PERMISSION_REQUEST_EVENT;

/// A fake runner returning canned outputs keyed by command string and recording
/// the commands actually run, in order. Mirrors codex test `FakeRunner`
/// (`hook_runtime_tests.rs:19-55`).
struct FakeRunner {
    responses: HashMap<String, CommandOutput>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl FakeRunner {
    fn new() -> Self {
        Self {
            responses: HashMap::new(),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_response(mut self, command: &str, output: CommandOutput) -> Self {
        self.responses.insert(command.to_string(), output);
        self
    }
}

#[async_trait]
impl CommandRunner for FakeRunner {
    async fn run(&self, command: &str, _stdin_json: &str, _timeout: Duration) -> CommandOutput {
        self.calls.lock().await.push(command.to_string());
        self.responses
            .get(command)
            .cloned()
            .unwrap_or(CommandOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            })
    }
}

/// An [`EventSink`] recording emitted events (Arc-shared for assertions).
#[derive(Clone, Default)]
struct SharedRecordingSink {
    events: Arc<std::sync::Mutex<Vec<PendingEvent>>>,
}

impl SharedRecordingSink {
    fn new() -> Self {
        Self::default()
    }

    fn events(&self) -> Vec<PendingEvent> {
        self.events.lock().expect("sink mutex poisoned").clone()
    }
}

impl EventSink for SharedRecordingSink {
    fn emit(&self, event: PendingEvent) {
        self.events.lock().expect("sink mutex poisoned").push(event);
    }
}

// --- output helpers (mirror codex `block_output` / `ctx_output`) ---

fn ok_output() -> CommandOutput {
    CommandOutput {
        exit_code: Some(0),
        stdout: String::new(),
        stderr: String::new(),
        timed_out: false,
    }
}

fn block_output(reason: &str) -> CommandOutput {
    CommandOutput {
        exit_code: Some(0),
        stdout: serde_json::to_string(&HookDecision {
            r#continue: Some(false),
            reason: Some(reason.to_string()),
            additional_context: None,
        })
        .unwrap(),
        stderr: String::new(),
        timed_out: false,
    }
}

fn ctx_output(ctx: &str) -> CommandOutput {
    CommandOutput {
        exit_code: Some(0),
        stdout: serde_json::to_string(&HookDecision {
            r#continue: None,
            reason: None,
            additional_context: Some(ctx.to_string()),
        })
        .unwrap(),
        stderr: String::new(),
        timed_out: false,
    }
}

fn group(matcher: Option<&str>, commands: &[&str]) -> HookMatcherGroup {
    HookMatcherGroup {
        matcher: matcher.map(|s| s.to_string()),
        hooks: commands
            .iter()
            .map(|c| HookCommand::Command {
                command: c.to_string(),
                timeout: None,
            })
            .collect(),
    }
}

fn config_with(event: HookEvent, groups: Vec<HookMatcherGroup>) -> HooksConfig {
    let mut map = HashMap::new();
    map.insert(event.as_str().to_string(), groups);
    HooksConfig { groups: map }
}

// ---------------------------------------------------------------------------
// Matching (codex: non_matching_hooks_do_not_run / matching_hook_runs)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_matching_hooks_do_not_run() {
    let cfg = config_with(
        HookEvent::PreToolUse,
        vec![group(Some("Bash"), &["echo-bash"])],
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runner = FakeRunner {
        responses: HashMap::new(),
        calls: calls.clone(),
    };
    let runtime = HookRuntime::with_runner(cfg, None, Arc::new(runner));
    let outcome = runtime
        .run(
            HookEvent::PreToolUse,
            Some("Edit"),
            HookInput::new(HookEvent::PreToolUse),
        )
        .await;
    assert!(calls.lock().await.is_empty());
    assert!(!outcome.is_blocked());
}

#[tokio::test]
async fn matching_hook_runs() {
    let cfg = config_with(
        HookEvent::PreToolUse,
        vec![group(Some("Bash"), &["echo-bash"])],
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runner = FakeRunner {
        responses: HashMap::new(),
        calls: calls.clone(),
    };
    let runtime = HookRuntime::with_runner(cfg, None, Arc::new(runner));
    let _ = runtime
        .run(
            HookEvent::PreToolUse,
            Some("Bash"),
            HookInput::new(HookEvent::PreToolUse),
        )
        .await;
    assert_eq!(calls.lock().await.as_slice(), &["echo-bash".to_string()]);
}

// ---------------------------------------------------------------------------
// Outcome folding (codex: block_short_circuits / additional_context_collected)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn block_short_circuits() {
    let cfg = config_with(
        HookEvent::PreToolUse,
        vec![group(Some("*"), &["blocker", "second"])],
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runner = FakeRunner {
        responses: HashMap::new(),
        calls: calls.clone(),
    }
    .with_response("blocker", block_output("nope"));
    let runtime = HookRuntime::with_runner(cfg, None, Arc::new(runner));
    let outcome = runtime
        .run(
            HookEvent::PreToolUse,
            Some("Bash"),
            HookInput::new(HookEvent::PreToolUse),
        )
        .await;
    assert!(outcome.is_blocked());
    assert_eq!(outcome.block_reason.as_deref(), Some("nope"));
    // The second hook must NOT run after the first blocks (short-circuit).
    assert_eq!(calls.lock().await.as_slice(), &["blocker".to_string()]);
}

#[tokio::test]
async fn allow_lets_action_proceed() {
    let cfg = config_with(HookEvent::PreToolUse, vec![group(Some("*"), &["noop"])]);
    let runner = FakeRunner::new().with_response("noop", ok_output());
    let runtime = HookRuntime::with_runner(cfg, None, Arc::new(runner));
    let outcome = runtime
        .run(
            HookEvent::PreToolUse,
            Some("Bash"),
            HookInput::new(HookEvent::PreToolUse),
        )
        .await;
    assert!(!outcome.is_blocked());
    assert_eq!(outcome.results.len(), 1);
}

#[tokio::test]
async fn additional_context_collected() {
    let cfg = config_with(
        HookEvent::PreToolUse,
        vec![group(Some("*"), &["ctx1", "ctx2"])],
    );
    let runner = FakeRunner::new()
        .with_response("ctx1", ctx_output("hello"))
        .with_response("ctx2", ctx_output("world"));
    let runtime = HookRuntime::with_runner(cfg, None, Arc::new(runner));
    let outcome = runtime
        .run(
            HookEvent::PreToolUse,
            Some("Bash"),
            HookInput::new(HookEvent::PreToolUse),
        )
        .await;
    assert_eq!(
        outcome.additional_context,
        vec!["hello".to_string(), "world".to_string()]
    );
}

// ---------------------------------------------------------------------------
// Timeout (codex: timeout_is_handled — recorded, does NOT block)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeout_is_handled() {
    let cfg = config_with(HookEvent::PreToolUse, vec![group(Some("*"), &["slow"])]);
    let runner = FakeRunner::new().with_response(
        "slow",
        CommandOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
        },
    );
    let runtime = HookRuntime::with_runner(cfg, None, Arc::new(runner));
    let outcome = runtime
        .run(
            HookEvent::PreToolUse,
            Some("Bash"),
            HookInput::new(HookEvent::PreToolUse),
        )
        .await;
    assert!(!outcome.is_blocked());
    assert_eq!(outcome.results.len(), 1);
    assert!(outcome.results[0].timed_out);
}

// ---------------------------------------------------------------------------
// exit code 2 => block (codex: exit_code_two_blocks)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exit_code_two_blocks() {
    let cfg = config_with(HookEvent::PreToolUse, vec![group(Some("*"), &["denier"])]);
    let runner = FakeRunner::new().with_response(
        "denier",
        CommandOutput {
            exit_code: Some(2),
            stdout: String::new(),
            stderr: "blocked by policy".to_string(),
            timed_out: false,
        },
    );
    let runtime = HookRuntime::with_runner(cfg, None, Arc::new(runner));
    let outcome = runtime
        .run(
            HookEvent::PreToolUse,
            Some("Bash"),
            HookInput::new(HookEvent::PreToolUse),
        )
        .await;
    assert!(outcome.is_blocked());
    assert_eq!(outcome.block_reason.as_deref(), Some("blocked by policy"));
}

// ---------------------------------------------------------------------------
// Matcher semantics — full legacy parity. Exact names (+ `|` alternations) use
// literal equality; everything else compiles as an (unanchored) regex.
// Legacy `hook_matcher_matches` (browser-use-core/src/lib.rs:8354-8367) +
// `hook_matcher_is_exact` (:8369-8373).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn alternation_matcher_matches() {
    let g = HookMatcherGroup {
        matcher: Some("Edit|Write".to_string()),
        hooks: vec![],
    };
    assert!(g.matches("Edit"));
    assert!(g.matches("Write"));
    assert!(!g.matches("Bash"));
}

#[tokio::test]
async fn wildcard_matcher_matches_all() {
    let g = HookMatcherGroup {
        matcher: Some("*".to_string()),
        hooks: vec![],
    };
    assert!(g.matches("anything"));
    let g2 = HookMatcherGroup {
        matcher: None,
        hooks: vec![],
    };
    assert!(g2.matches("anything"));
    let g3 = HookMatcherGroup {
        matcher: Some(String::new()),
        hooks: vec![],
    };
    assert!(g3.matches("anything"));
}

#[tokio::test]
async fn exact_matcher_matches() {
    let g = HookMatcherGroup {
        matcher: Some("Bash".to_string()),
        hooks: vec![],
    };
    assert!(g.matches("Bash"));
    assert!(!g.matches("Edit"));
}

#[tokio::test]
async fn regex_prefix_matcher_matches() {
    // `Bash.*` matches `Bash` and any `Bash`-prefixed name, not `Cat`.
    let g = HookMatcherGroup {
        matcher: Some("Bash.*".to_string()),
        hooks: vec![],
    };
    assert!(g.matches("Bash"));
    assert!(g.matches("BashOutput"));
    assert!(!g.matches("Cat"));
}

#[tokio::test]
async fn regex_mcp_namespace_matcher_matches() {
    // `mcp__.*` matches MCP-namespaced tool names like `mcp__foo`.
    let g = HookMatcherGroup {
        matcher: Some("mcp__.*".to_string()),
        hooks: vec![],
    };
    assert!(g.matches("mcp__foo"));
    assert!(g.matches("mcp__server__tool"));
    assert!(!g.matches("Bash"));
}

#[tokio::test]
async fn exact_name_does_not_match_superstring() {
    // `Edit` is an exact name (no metacharacters) so it takes the literal
    // equality path (legacy :8362) and must NOT match `Edited` / `PreEdit`.
    let g = HookMatcherGroup {
        matcher: Some("Edit".to_string()),
        hooks: vec![],
    };
    assert!(g.matches("Edit"));
    assert!(!g.matches("Edited"));
    assert!(!g.matches("PreEdit"));

    // `Edit|Write` is also all exact chars -> literal alternation, not regex.
    let g2 = HookMatcherGroup {
        matcher: Some("Edit|Write".to_string()),
        hooks: vec![],
    };
    assert!(g2.matches("Edit"));
    assert!(!g2.matches("Edited"));
}

#[tokio::test]
async fn anchored_regex_does_not_match_superstring() {
    // A pattern with metacharacters is compiled as a regex; explicit anchors
    // `^(?:Edit)$` ensure `Edit` does NOT match `Edited`.
    let g = HookMatcherGroup {
        matcher: Some("^(?:Edit)$".to_string()),
        hooks: vec![],
    };
    assert!(g.matches("Edit"));
    assert!(!g.matches("Edited"));
    assert!(!g.matches("PreEdit"));
}

#[tokio::test]
async fn invalid_regex_matches_nothing() {
    // Legacy parity (browser-use-core/src/lib.rs:8364-8366): an uncompilable
    // pattern returns `false` via `.unwrap_or(false)` -> matches NOTHING.
    // `(unclosed` has metacharacters so it is NOT the exact fast-path; it fails
    // to compile, so even its own raw text does not match.
    let g = HookMatcherGroup {
        matcher: Some("(unclosed".to_string()),
        hooks: vec![],
    };
    assert!(!g.matches("(unclosed"));
    assert!(!g.matches("unclosed"));
    assert!(!g.matches("anything"));
}

#[tokio::test]
async fn matcher_is_exact_detects_metacharacters() {
    assert!(super::config::matcher_is_exact("Bash"));
    assert!(super::config::matcher_is_exact("mcp__foo"));
    assert!(super::config::matcher_is_exact("Edit|Write"));
    assert!(!super::config::matcher_is_exact("Bash.*"));
    assert!(!super::config::matcher_is_exact("^Edit$"));
}

// ---------------------------------------------------------------------------
// Ordering (codex: ordering_preserved)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ordering_preserved() {
    let cfg = config_with(
        HookEvent::PreToolUse,
        vec![group(Some("*"), &["a"]), group(Some("*"), &["b", "c"])],
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runner = FakeRunner {
        responses: HashMap::new(),
        calls: calls.clone(),
    };
    let runtime = HookRuntime::with_runner(cfg, None, Arc::new(runner));
    let outcome = runtime
        .run(
            HookEvent::PreToolUse,
            Some("Bash"),
            HookInput::new(HookEvent::PreToolUse),
        )
        .await;
    assert_eq!(outcome.results.len(), 3);
    assert_eq!(
        calls.lock().await.as_slice(),
        &["a".to_string(), "b".to_string(), "c".to_string()]
    );
}

// ---------------------------------------------------------------------------
// Event-kind selection: each kind selects only its own hooks.
// Covers codex PreToolUse/PostToolUse + the sanctioned Prompt / Agent /
// PermissionRequest / SubagentStart / SubagentStop additions.
// ---------------------------------------------------------------------------

async fn assert_event_runs_only_its_hooks(event: HookEvent, other: HookEvent) {
    let mut map = HashMap::new();
    map.insert(event.as_str().to_string(), vec![group(None, &["mine"])]);
    map.insert(other.as_str().to_string(), vec![group(None, &["theirs"])]);
    let cfg = HooksConfig { groups: map };
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runner = FakeRunner {
        responses: HashMap::new(),
        calls: calls.clone(),
    };
    let runtime = HookRuntime::with_runner(cfg, None, Arc::new(runner));
    let _ = runtime.run(event, None, HookInput::new(event)).await;
    assert_eq!(
        calls.lock().await.as_slice(),
        &["mine".to_string()],
        "event {:?} selected the wrong hooks",
        event
    );
}

#[tokio::test]
async fn each_event_kind_selects_its_own_hooks() {
    assert_event_runs_only_its_hooks(HookEvent::PreToolUse, HookEvent::PostToolUse).await;
    assert_event_runs_only_its_hooks(HookEvent::PostToolUse, HookEvent::PreToolUse).await;
    assert_event_runs_only_its_hooks(HookEvent::Prompt, HookEvent::Agent).await;
    assert_event_runs_only_its_hooks(HookEvent::Agent, HookEvent::Prompt).await;
    assert_event_runs_only_its_hooks(HookEvent::PermissionRequest, HookEvent::PreToolUse).await;
    assert_event_runs_only_its_hooks(HookEvent::SubagentStart, HookEvent::SubagentStop).await;
    assert_event_runs_only_its_hooks(HookEvent::SubagentStop, HookEvent::SubagentStart).await;
}

#[tokio::test]
async fn event_as_str_round_trips_added_kinds() {
    assert_eq!(HookEvent::PermissionRequest.as_str(), "PermissionRequest");
    assert_eq!(HookEvent::Prompt.as_str(), "Prompt");
    assert_eq!(HookEvent::Agent.as_str(), "Agent");
}

// ---------------------------------------------------------------------------
// PermissionRequest event emission (SANCTIONED ADDITION).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn permission_request_emits_pending_event() {
    // No hooks configured for the event; we only assert the PendingEvent.
    let cfg = HooksConfig::default();
    let sink = SharedRecordingSink::new();
    let runtime =
        HookRuntime::with_runner(cfg, Some("sess-1".to_string()), Arc::new(FakeRunner::new()))
            .with_event_sink(Arc::new(sink.clone()));
    let input = HookInput::new(HookEvent::PermissionRequest)
        .with_tool_input(json!({"cmd": "rm -rf /"}))
        .with_extra("reason", json!("destructive command"));
    let _ = runtime
        .run(HookEvent::PermissionRequest, Some("Bash"), input)
        .await;

    let events = sink.events();
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.session_id, "sess-1");
    assert_eq!(ev.event_type, PERMISSION_REQUEST_EVENT);
    assert_eq!(ev.payload["hookEventName"], json!("PermissionRequest"));
    assert_eq!(ev.payload["toolName"], json!("Bash"));
    assert_eq!(ev.payload["reason"], json!("destructive command"));
    assert_eq!(ev.payload["toolInput"], json!({"cmd": "rm -rf /"}));
}

#[tokio::test]
async fn permission_request_runs_matching_hooks_and_can_block() {
    let cfg = config_with(
        HookEvent::PermissionRequest,
        vec![group(Some("Bash"), &["gate"])],
    );
    let sink = SharedRecordingSink::new();
    let runner = FakeRunner::new().with_response("gate", block_output("denied"));
    let runtime = HookRuntime::with_runner(cfg, Some("sess-2".to_string()), Arc::new(runner))
        .with_event_sink(Arc::new(sink.clone()));
    let outcome = runtime
        .run(
            HookEvent::PermissionRequest,
            Some("Bash"),
            HookInput::new(HookEvent::PermissionRequest),
        )
        .await;
    // Hook can block the permission, and the PendingEvent is still emitted.
    assert!(outcome.is_blocked());
    assert_eq!(outcome.block_reason.as_deref(), Some("denied"));
    assert_eq!(sink.events().len(), 1);
}

#[tokio::test]
async fn permission_request_without_sink_is_noop_emission() {
    // No sink configured: hooks still run, no panic, no event.
    let cfg = config_with(HookEvent::PermissionRequest, vec![group(None, &["gate"])]);
    let runtime =
        HookRuntime::with_runner(cfg, Some("sess-3".to_string()), Arc::new(FakeRunner::new()));
    let outcome = runtime
        .run(
            HookEvent::PermissionRequest,
            Some("Bash"),
            HookInput::new(HookEvent::PermissionRequest),
        )
        .await;
    assert!(!outcome.is_blocked());
    assert_eq!(outcome.results.len(), 1);
}

// ---------------------------------------------------------------------------
// HookInput / config serde round-trips.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hook_input_serializes_event_name_and_extra() {
    let input = HookInput::new(HookEvent::Prompt)
        .with_prompt("hello")
        .with_extra("custom", json!(42));
    let v = serde_json::to_value(&input).unwrap();
    assert_eq!(v["hookEventName"], json!("Prompt"));
    assert_eq!(v["prompt"], json!("hello"));
    // `extra` is flattened to the top level.
    assert_eq!(v["custom"], json!(42));
    // Absent optional fields are omitted.
    assert!(v.get("toolName").is_none());
}

#[tokio::test]
async fn config_deserializes_codex_shape() {
    let raw = json!({
        "PreToolUse": [
            { "matcher": "Bash", "hooks": [ { "type": "command", "command": "echo hi", "timeout": 5 } ] }
        ]
    });
    let cfg: HooksConfig = serde_json::from_value(raw).unwrap();
    let groups = cfg.groups_for(HookEvent::PreToolUse);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].matcher.as_deref(), Some("Bash"));
    assert_eq!(groups[0].hooks.len(), 1);
    assert_eq!(groups[0].hooks[0].command_line(), "echo hi");
    assert_eq!(groups[0].hooks[0].timeout_secs(), Some(5));
}

#[tokio::test]
async fn matcher_matches_free_fn_parity() {
    assert!(super::config::matcher_matches(None, "x"));
    assert!(super::config::matcher_matches(Some("*"), "x"));
    assert!(super::config::matcher_matches(Some(""), "x"));
    assert!(super::config::matcher_matches(Some("a|b"), "b"));
    assert!(!super::config::matcher_matches(Some("a|b"), "c"));
    assert!(super::config::matcher_matches(Some("exact"), "exact"));
    assert!(!super::config::matcher_matches(Some("exact"), "other"));
    // Full regex now works through the free fn (`Bash.*` is a real regex);
    // a plain exact name uses literal equality.
    assert!(super::config::matcher_matches(Some("Bash.*"), "BashOutput"));
    assert!(!super::config::matcher_matches(Some("Bash"), "Bashx"));
}
