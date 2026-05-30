//! Tests for the async `request_user_input` tool ([`RequestUserInputTool`]).
//!
//! No network and no stdin: the tool implements the REQUEST side only (the host
//! response round-trip is deferred — see the module doc in
//! `request_user_input.rs`), so every test drives validation / serialization /
//! the request payload without ever blocking for a human. Structure mirrors
//! `update_plan_tests.rs` (the closest analog).

use std::collections::HashMap;

use super::request_user_input::{
    validate_request_user_input, RequestUserInputRequest, RequestUserInputResponse,
    RequestUserInputTool, UserInputAnswer, UserInputOption, UserInputQuestion,
    REQUEST_USER_INPUT_STDOUT_PREFIX,
};
use crate::tools::approval::AskForApproval;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{AutoApprover, SandboxAttempt, ToolCtx, ToolError, ToolRuntime};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions, SandboxType,
};

/// A `SandboxType::None` launch + attempt for direct `run` calls (mirrors
/// `update_plan_tests::none_launch` / `none_attempt`).
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
        tool_name: "request_user_input".to_string(),
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

fn sample_request() -> RequestUserInputRequest {
    RequestUserInputRequest::single(
        "deploy",
        "Deploy",
        "Ship it?",
        [
            UserInputOption {
                label: "Yes (Recommended)".to_string(),
                description: "Deploy now".to_string(),
            },
            UserInputOption {
                label: "No".to_string(),
                description: "Hold off".to_string(),
            },
        ],
    )
}

// ---- (1) a valid question (with and without choices) produces the request ----

// (1a) A valid question WITH choices -> tool produces the structured request
// payload (prefixed JSON) directly, without blocking.
#[tokio::test]
async fn valid_question_with_choices_produces_request_payload() {
    let tool = RequestUserInputTool::new();
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let ctx = ctx();

    let out = tool.run(&sample_request(), &attempt, &ctx).await.unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(out.stderr.is_empty());
    assert!(out.stdout.starts_with(REQUEST_USER_INPUT_STDOUT_PREFIX));

    // The payload is the NORMALIZED request (is_other forced true), not an answer.
    let json = out
        .stdout
        .strip_prefix(REQUEST_USER_INPUT_STDOUT_PREFIX)
        .unwrap();
    let parsed: RequestUserInputRequest = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.questions.len(), 1);
    assert_eq!(parsed.questions[0].id, "deploy");
    assert!(
        parsed.questions[0].is_other,
        "normalize forces is_other=true"
    );
    assert_eq!(parsed.questions[0].options.as_ref().unwrap().len(), 2);
}

// (1b) Validation accepts the "with choices" and "minimal but well-formed"
// shapes and normalizes is_other=true. (A question always needs options per
// codex, so the "without choices" valid case is the minimal one-question form.)
#[test]
fn validate_accepts_well_formed_and_forces_is_other() {
    let normalized = validate_request_user_input(sample_request()).unwrap();
    assert!(normalized.questions.iter().all(|q| q.is_other));

    let minimal = RequestUserInputRequest {
        questions: vec![UserInputQuestion {
            id: "q1".to_string(),
            header: "H".to_string(),
            question: "Pick?".to_string(),
            is_other: false,
            is_secret: false,
            options: Some(vec![UserInputOption {
                label: "A".to_string(),
                description: "first".to_string(),
            }]),
        }],
    };
    let normalized = validate_request_user_input(minimal).unwrap();
    assert!(normalized.questions[0].is_other);
}

// ---- (2) empty question -> rejected ----

#[test]
fn empty_question_text_is_rejected() {
    let req = RequestUserInputRequest {
        questions: vec![UserInputQuestion {
            id: "q1".to_string(),
            header: "H".to_string(),
            question: "   ".to_string(),
            is_other: false,
            is_secret: false,
            options: Some(vec![UserInputOption {
                label: "A".to_string(),
                description: "first".to_string(),
            }]),
        }],
    };
    let ToolError::Rejected(msg) = validate_request_user_input(req).unwrap_err() else {
        panic!("expected Rejected");
    };
    assert!(msg.contains("empty question text"), "got: {msg}");
}

#[test]
fn empty_questions_list_is_rejected() {
    let req = RequestUserInputRequest { questions: vec![] };
    let ToolError::Rejected(msg) = validate_request_user_input(req).unwrap_err() else {
        panic!("expected Rejected");
    };
    assert!(msg.contains("at least one question"), "got: {msg}");
}

// ---- (3) malformed choices handled per codex (normalize: non-empty options) --

#[test]
fn missing_options_is_rejected_per_codex() {
    let req = RequestUserInputRequest {
        questions: vec![UserInputQuestion {
            id: "q1".to_string(),
            header: "H".to_string(),
            question: "Pick one?".to_string(),
            is_other: false,
            is_secret: false,
            options: None,
        }],
    };
    let ToolError::Rejected(msg) = validate_request_user_input(req).unwrap_err() else {
        panic!("expected Rejected");
    };
    assert!(msg.contains("non-empty options"), "got: {msg}");
}

#[test]
fn empty_options_vec_is_rejected_per_codex() {
    let req = RequestUserInputRequest {
        questions: vec![UserInputQuestion {
            id: "q1".to_string(),
            header: "H".to_string(),
            question: "Pick one?".to_string(),
            is_other: false,
            is_secret: false,
            options: Some(vec![]),
        }],
    };
    let ToolError::Rejected(msg) = validate_request_user_input(req).unwrap_err() else {
        panic!("expected Rejected");
    };
    assert!(msg.contains("non-empty options"), "got: {msg}");
}

// ---- (4) request/response serde round-trip to codex's EXACT wire shape ----

#[test]
fn request_serde_round_trips_to_codex_wire_shape() {
    // Codex `RequestUserInputQuestion`: is_other/is_secret use camelCase wire
    // names `isOther`/`isSecret` with `#[serde(default)]`; options is
    // skip_serializing_if Option::is_none.
    let req = RequestUserInputRequest {
        questions: vec![UserInputQuestion {
            id: "q1".to_string(),
            header: "Header".to_string(),
            question: "What now?".to_string(),
            is_other: true,
            is_secret: false,
            options: Some(vec![UserInputOption {
                label: "A".to_string(),
                description: "first".to_string(),
            }]),
        }],
    };
    let json = serde_json::to_value(&req).unwrap();
    let q = &json["questions"][0];
    assert_eq!(q["id"], "q1");
    assert_eq!(q["header"], "Header");
    assert_eq!(q["question"], "What now?");
    // camelCase wire names (codex parity).
    assert_eq!(q["isOther"], true);
    assert_eq!(q["isSecret"], false);
    assert!(q.get("is_other").is_none(), "must use camelCase isOther");
    assert_eq!(q["options"][0]["label"], "A");
    assert_eq!(q["options"][0]["description"], "first");

    let back: RequestUserInputRequest = serde_json::from_value(json).unwrap();
    assert_eq!(req, back);

    // Codex deserializes a minimal question (no isOther/isSecret/options),
    // defaulting the bools to false and options to None (#[serde(default)] /
    // Option default). This matches the codex handler test payload shape
    // (`request_user_input_tests.rs:37-53` omits isOther/isSecret).
    let minimal = serde_json::json!({
        "questions": [ { "id": "q1", "header": "H", "question": "Q?",
            "options": [ { "label": "A", "description": "a" } ] } ]
    });
    let parsed: RequestUserInputRequest = serde_json::from_value(minimal).unwrap();
    assert!(!parsed.questions[0].is_other);
    assert!(!parsed.questions[0].is_secret);
    assert_eq!(parsed.questions[0].options.as_ref().unwrap().len(), 1);
}

#[test]
fn response_serde_round_trips_to_codex_wire_shape() {
    // Codex `RequestUserInputResponse { answers: HashMap<String,
    // RequestUserInputAnswer> }`, `RequestUserInputAnswer { answers: Vec<String> }`
    // (`protocol/src/request_user_input.rs:36-44`).
    let mut answers = HashMap::new();
    answers.insert(
        "q1".to_string(),
        UserInputAnswer {
            answers: vec!["Yes".to_string()],
        },
    );
    let resp = RequestUserInputResponse { answers };

    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["answers"]["q1"]["answers"][0], "Yes");

    let back: RequestUserInputResponse = serde_json::from_value(json).unwrap();
    assert_eq!(resp, back);

    // Deserialize from the codex wire form.
    let wire = serde_json::json!({ "answers": { "q1": { "answers": ["A", "B"] } } });
    let parsed: RequestUserInputResponse = serde_json::from_value(wire).unwrap();
    assert_eq!(parsed.answers["q1"].answers, vec!["A", "B"]);
}

// ---- (5) drive one valid call through the orchestrator over the seam ----

#[tokio::test]
async fn orchestrated_request_completes_under_none_without_blocking() {
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = RequestUserInputTool::new();
    let ctx = ctx();

    // `Never` => no approval prompt; the call must return the request payload
    // without ever blocking for a human (request side only).
    let result = orch
        .run(
            &tool,
            &sample_request(),
            &ctx,
            &turn_env(),
            AskForApproval::Never,
        )
        .await
        .expect("orchestration ok");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert_eq!(result.output.exit_code, 0);
    assert!(
        result
            .output
            .stdout
            .starts_with(REQUEST_USER_INPUT_STDOUT_PREFIX),
        "got: {}",
        result.output.stdout
    );
    // It returned the REQUEST (questions), not a RequestUserInputResponse.
    let json = result
        .output
        .stdout
        .strip_prefix(REQUEST_USER_INPUT_STDOUT_PREFIX)
        .unwrap();
    assert!(serde_json::from_str::<RequestUserInputRequest>(json).is_ok());
    assert!(serde_json::from_str::<RequestUserInputResponse>(json).is_err());
}

#[tokio::test]
async fn orchestrated_missing_options_is_rejected() {
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = RequestUserInputTool::new();
    let ctx = ctx();
    let req = RequestUserInputRequest {
        questions: vec![UserInputQuestion {
            id: "q1".to_string(),
            header: "H".to_string(),
            question: "Pick?".to_string(),
            is_other: false,
            is_secret: false,
            options: None,
        }],
    };

    let err = orch
        .run(&tool, &req, &ctx, &turn_env(), AskForApproval::Never)
        .await
        .expect_err("missing options must not complete through the orchestrator");
    assert!(matches!(err, ToolError::Rejected(_)), "got: {err:?}");
}

// ---- parallel-safety (matches codex) ----

// parallel_safe MUST be false. Codex's request_user_input handler does not
// override supports_parallel_tool_calls, inheriting the trait default of false
// (codex-rs/tools/src/tool_executor.rs:51-53): it is a blocking human
// interaction and runs serially. DO NOT flip this to true.
#[test]
fn request_user_input_is_not_parallel_safe_matches_codex() {
    let tool = RequestUserInputTool::new();
    assert!(
        !tool.parallel_safe(&sample_request()),
        "request_user_input must match codex: NOT parallel-safe (blocking human interaction)"
    );
}
