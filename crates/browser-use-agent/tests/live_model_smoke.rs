//! Live, end-to-end smoke test for the PRODUCTION multi-provider model path.
//!
//! This is the only test that may touch the network, and it is double-gated so
//! plain `cargo test` stays fully offline:
//!
//! 1. behind `#[cfg(feature = "live")]` — not compiled unless `--features live`,
//! 2. `#[ignore]`d — not run even when compiled.
//!
//! ## Running it for real
//! ```text
//! cargo test -p browser-use-agent --features live -- --ignored live_model_smoke --nocapture
//! ```
//!
//! It resolves a provider from standard env keys via
//! [`browser_use_agent::turn::provider_choice_from_env`]
//! (`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `LLM_BROWSER_*` /
//! OpenAI-compatible), builds the real `ModelClient` route, sends a one-line
//! message, drives the live stream, and asserts a non-empty reply.
//!
//! ## Honest note (codex backend is cut)
//! This machine currently has only a codex OAuth login, which targets the cut
//! `chatgpt.com/backend-api` backend — that is deliberately NOT consulted here.
//! With no OpenAI/Anthropic/compatible key in env, the test fails fast with a
//! clear "no provider credentials" message rather than faking a result. Supply a
//! real non-codex key to exercise the live path.

#![cfg(feature = "live")]

use std::sync::Arc;

use browser_use_agent::turn::{build_route, provider_choice_from_env, ProviderChoice};
use browser_use_llm::route::ModelClient;
use browser_use_llm::schema::{ContentPart, LlmRequest, Message};

/// Model id to exercise. Override with `LIVE_SMOKE_MODEL`; defaults per provider.
fn smoke_model(choice: &ProviderChoice) -> String {
    if let Ok(m) = std::env::var("LIVE_SMOKE_MODEL") {
        if !m.trim().is_empty() {
            return m;
        }
    }
    match choice {
        ProviderChoice::OpenAiResponses { .. } => "gpt-5.1-codex".to_string(),
        ProviderChoice::Anthropic { .. } => "claude-sonnet-4-6".to_string(),
        ProviderChoice::OpenAiCompatibleProvider { .. }
        | ProviderChoice::OpenAiCompatibleCustom { .. } => "gpt-4o-mini".to_string(),
    }
}

/// Provider label for the request's `provider` field.
fn provider_label(choice: &ProviderChoice) -> &'static str {
    match choice {
        ProviderChoice::OpenAiResponses { .. } => "openai",
        ProviderChoice::Anthropic { .. } => "anthropic",
        ProviderChoice::OpenAiCompatibleProvider { .. }
        | ProviderChoice::OpenAiCompatibleCustom { .. } => "openai-compatible",
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live network + real provider key; run with --features live -- --ignored"]
async fn live_model_smoke_replies_non_empty() {
    // 1. Resolve a real provider from env (NOT codex). Honest failure if absent.
    let choice = provider_choice_from_env().expect(
        "no provider credentials in env: set OPENAI_API_KEY / ANTHROPIC_API_KEY / \
         LLM_BROWSER_* (the codex OAuth login is intentionally not used)",
    );
    let model = smoke_model(&choice);
    eprintln!("live_model_smoke: provider={choice:?} model={model}");

    // 2. Build the real route + client.
    let route = build_route(&choice, &model).expect("route builds");
    let client = ModelClient::new();

    // 3. One-line request through the production neutral request shape.
    let mut req = LlmRequest::new(model, provider_label(&choice));
    req.messages
        .push(Message::user_text("reply with the single word: ok"));

    // 4. Drive the live streaming path and aggregate.
    let resp = client
        .generate(&route, &req)
        .await
        .expect("live generate should succeed");

    // 5. Assert a non-empty streamed text reply.
    let text: String = resp
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    eprintln!("live_model_smoke reply: {text:?}");
    eprintln!("live_model_smoke usage: {:?}", resp.usage);
    assert!(
        !text.trim().is_empty(),
        "expected a non-empty streamed reply from the provider"
    );
}
