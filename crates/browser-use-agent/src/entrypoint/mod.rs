//! Run-entrypoint facade: the binary-facing function tui/cli call to run a
//! session on the new async engine.
//!
//! This is the single callable that assembles config + session store + model
//! route + turn loop into one run, driving a session turn through the real
//! [`ModelSamplingDriver`](crate::turn::sampling::ModelSamplingDriver) path. It
//! closes the gap where
//! [`build_sampling_driver`](crate::turn::model_path::build_sampling_driver)
//! existed but nothing binary-facing called it (the call lands in
//! [`provider::resolve_provider`], reached from [`run_session_with_config`]).
//!
//! ## Assembly order (parity with the legacy `browser-use-core` run path)
//! The legacy `run_session` / `run_existing_session_from_config` assembled:
//! config → provider selection → workspace-context seed → loop → persistence.
//! This facade reproduces that order on the new primitives:
//!   1. resolve the provider backend → model route → sampling driver via
//!      [`provider::resolve_provider`] (**where `build_sampling_driver` is called**),
//!   2. seed the environment workspace-context durable event before the first
//!      turn ([`append_environment_context_event`]),
//!   3. drive the [`TurnLoop`] to quiescence over a store-backed [`TurnState`] +
//!      a [`TurnObserver`] that persists the run's result,
//!   4. return the [`SessionId`].
//!
//! The run's prompt history is whatever is already in the session's durable log
//! (e.g. a `session.input` the caller appended). [`ProviderRunConfig`] carries no
//! initial-user-text field of its own, so the facade does not synthesize one — it
//! drives over the existing log, exactly as the legacy resume path does for an
//! already-seeded session.
//!
//! ## What runs end-to-end vs. documented boundaries
//! - The **fake** backend runs end-to-end offline (the loop is driven to
//!   completion through an offline scripted [`SamplingDriver`]), proving the
//!   assembly with zero network.
//! - A **real** backend's driver is CONSTRUCTED via the same path
//!   ([`build_sampling_driver`]) and the loop is wired to it; the real
//!   `ModelClient::stream` only fires when a live key is configured, so tests
//!   never touch the network (they assert real-driver *construction* offline and
//!   drive the loop only via the fake/scripted path).
//!
//! ## Phase-E seams (NOT yet wired)
//! The agent crate has no production [`TurnState`]/[`TurnObserver`] for the live
//! turn loop yet (only `compact::CompactingTurnState`, which needs a
//! `ContextManager` + a `CompactionSampler`, and the test fakes). This facade
//! provides minimal store-backed impls so it can actually drive a run today; the
//! richer `ContextManager`-backed `TurnState` (token accounting, mid-turn
//! compaction, pending steer queue), tools/dispatch fusion, goals, hooks,
//! guardian, skills and personality context all remain Phase-E work. Each seam is
//! marked inline with `// Phase-E seam:`.

pub mod provider;

use std::sync::Arc;
use std::sync::Mutex;

use browser_use_llm::schema::Message;
use tokio_util::sync::CancellationToken;

use crate::config_overrides::ProviderRunConfig;
use crate::context::workspace_context::append_environment_context_event;
use crate::context::ContextManager;
use crate::decision::SamplingOutcome;
use crate::decision::TokenStatus;
use crate::events::EventSink;
use crate::events::PendingEvent;
use crate::events::TurnCtx;
use crate::session::provider_messages_from_events;
use crate::session::SessionId;
use crate::session::SharedStore;
use crate::task::TurnLifecycleEvent;
use crate::turn::SamplingDriver;
use crate::turn::TurnLoop;
use crate::turn::TurnObserver;
use crate::turn::TurnState;
use crate::AgentError;

use provider::ResolvedProvider;

/// codex `Provider::DEFAULT_STREAM_MAX_RETRIES`. Used when the run config does
/// not carry a `stream_max_retries` of its own.
///
/// Phase-E seam: the legacy run path threads `AgentConfig::stream_max_retries`
/// here; the full `AgentConfig` is not plumbed into [`ProviderRunConfig`] yet, so
/// the facade uses codex's default budget. Wave-E threads the real value through.
const DEFAULT_STREAM_MAX_RETRIES: u32 = 5;

/// A minimal [`TurnState`] backed by the session's durable event log.
///
/// It lowers the durable history (reduced to provider messages, then lowered to
/// typed [`Message`]s through the pure [`ContextManager`]) into each turn's prompt,
/// and records assistant turns back into an in-memory buffer for the rest of the
/// run.
///
/// Phase-E seam: the production `TurnState` is `ContextManager`-backed with token
/// accounting, mid-turn compaction (`compact`), and a pending steer/input queue.
/// This impl has no pending-input queue (`has_pending_input` is always false and
/// `take_pending_input` is empty) and a zeroed [`TokenStatus`] (so the loop's
/// compaction gate never trips). Wave-E replaces it with the real state.
struct StoreTurnState {
    store: SharedStore,
    session_id: SessionId,
    /// Assistant turns recorded this run, so a follow-up prompt sees them.
    recorded: Mutex<Vec<Message>>,
}

impl StoreTurnState {
    fn new(store: SharedStore, session_id: SessionId) -> Self {
        Self {
            store,
            session_id,
            recorded: Mutex::new(Vec::new()),
        }
    }
}

/// Lower a session's durable event log into typed prompt messages: blocking store
/// read + pure reduce ([`provider_messages_from_events`]) + pure lower
/// ([`ContextManager::lower_to_messages`]). Runs synchronously; the caller wraps
/// it in `spawn_blocking` to keep it off the async runtime.
fn history_from_store(store: &SharedStore, session_id: &str) -> Vec<Message> {
    let events = {
        let store = store.lock().expect("store mutex poisoned");
        store.events_for_session(session_id).unwrap_or_default()
    };
    // Pure reduce: durable events -> provider messages (the legacy currency).
    let items = provider_messages_from_events(&events);
    // Pure lower: provider-message Values -> typed Messages for the request.
    ContextManager::new().lower_to_messages(&items)
}

impl TurnState for StoreTurnState {
    async fn clone_history_for_prompt(&self) -> Vec<Message> {
        let store = Arc::clone(&self.store);
        let session_id = self.session_id.as_str().to_string();
        // The durable read is synchronous (rusqlite); run it off the async runtime.
        let mut msgs = tokio::task::spawn_blocking(move || history_from_store(&store, &session_id))
            .await
            .unwrap_or_default();
        msgs.extend(self.recorded.lock().unwrap().iter().cloned());
        msgs
    }

    async fn record_items(&self, items: &[Message]) {
        self.recorded.lock().unwrap().extend_from_slice(items);
    }

    async fn has_pending_input(&self) -> bool {
        // Phase-E seam: no pending steer/input queue is wired yet.
        false
    }

    async fn take_pending_input(&self) -> Vec<Message> {
        // Phase-E seam: no pending steer/input queue is wired yet.
        Vec::new()
    }

    async fn token_status(&self) -> TokenStatus {
        // Phase-E seam: real token accounting (and thus compaction triggering) is
        // not wired yet — a zeroed status never trips the loop's compaction gate
        // (`token_limit_reached == false`, scope below limit).
        TokenStatus {
            auto_compact_scope_tokens: 0,
            auto_compact_scope_limit: i64::MAX,
            full_context_window_limit_reached: false,
            token_limit_reached: false,
        }
    }

    // Phase-E seam: `compact` keeps the trait default (no-op); the real
    // model-based compaction body lands with the ContextManager-backed TurnState.
}

/// A [`TurnObserver`] that maps loop lifecycle into the durable UI event log.
///
/// On turn completion it emits the final agent message as an `agent.message`
/// event through the durable UI sink, so the run's result is persisted (parity:
/// the legacy run path persisted the final assistant message). The streaming text
/// deltas are already emitted by the sampling driver through its own sink.
struct StoreObserver {
    sink: Arc<dyn EventSink>,
    session_id: String,
}

impl StoreObserver {
    fn new(sink: Arc<dyn EventSink>, session_id: String) -> Self {
        Self { sink, session_id }
    }
}

impl TurnObserver for StoreObserver {
    fn on_lifecycle(&self, ev: TurnLifecycleEvent) {
        // Phase-E seam: started/aborted lifecycle markers are not surfaced as
        // store events yet (the legacy stack had richer turn-lifecycle telemetry).
        // We persist the terminal agent message, which is what readers need today.
        if let TurnLifecycleEvent::TurnComplete {
            last_agent_message: Some(text),
            ..
        } = ev
        {
            self.sink.emit(PendingEvent::new(
                self.session_id.clone(),
                "agent.message",
                serde_json::json!({ "content": text }),
            ));
        }
    }
}

/// A network-free scripted driver for the `Fake` backend.
///
/// The `Fake` [`ProviderBackend`] has no real provider
/// ([`provider::ResolvedProvider::Fake`]); this driver lets the facade still drive
/// a run to completion offline. It returns one terminal [`SamplingOutcome`] (no
/// follow-up) carrying a fixed assistant message, so the loop completes in a
/// single iteration. Used for the `Fake` product backend and the facade tests.
struct FakeSamplingDriver {
    message: String,
}

impl FakeSamplingDriver {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl SamplingDriver for FakeSamplingDriver {
    async fn run_sampling_request(
        &self,
        _input: Vec<Message>,
        _cancel: CancellationToken,
    ) -> Result<SamplingOutcome, AgentError> {
        Ok(SamplingOutcome {
            model_needs_follow_up: false,
            last_agent_message: Some(self.message.clone()),
            finish_reason: None,
        })
    }
}

/// Build the per-turn [`TurnCtx`] identity from a config + session id.
fn turn_ctx(session_id: &SessionId, config: &ProviderRunConfig) -> TurnCtx {
    TurnCtx {
        session_id: session_id.as_str().to_string(),
        model: config.model.clone(),
        // Best-effort wire-provider label from the backend; the request's real
        // provider identity is fixed inside `build_route`.
        provider: format!("{:?}", config.backend).to_ascii_lowercase(),
        turn_idx: 0,
        attempt: 0,
    }
}

/// A no-op UI [`EventSink`] for the sampling driver's emitted events.
///
/// Phase-E seam: production routes the driver's UI events to a real
/// [`StoreSink`]/TUI sink. The facade discards them for now — the run's durable
/// result is still persisted by [`StoreObserver`] — so a run is exercisable
/// without a UI wired up. Wave-E threads the real UI sink here.
struct DiscardSink;

impl EventSink for DiscardSink {
    fn emit(&self, _ev: PendingEvent) {}
}

/// Drive a loop run to quiescence with `driver`, over a store-backed state +
/// observer. Returns the final assistant message (`None` if no text was produced).
async fn drive_run<Sd: SamplingDriver>(
    store: SharedStore,
    session_id: SessionId,
    ctx: TurnCtx,
    driver: Sd,
    turn_has_fresh_input: bool,
) -> Result<Option<String>, AgentError> {
    let state = StoreTurnState::new(Arc::clone(&store), session_id.clone());

    // The observer persists the terminal agent message through a synchronous
    // durable sink over the SharedStore. (The async `events::StoreSink` writer
    // needs sole ownership of the Store, which the facade does not have — it keeps
    // a SharedStore clone — so a small shared-lock adapter is used instead.)
    let sink: Arc<dyn EventSink> = make_ui_sink(Arc::clone(&store));
    let observer = StoreObserver::new(sink, session_id.as_str().to_string());

    let turn_loop = TurnLoop::new(state, driver, observer);
    turn_loop
        .run(ctx, turn_has_fresh_input, CancellationToken::new())
        .await
}

/// Build the durable UI sink for loop lifecycle events.
///
/// The async `events::StoreSink::spawn` requires sole ownership of the `Store`,
/// which the facade does not have (the caller keeps the `SharedStore`). So the
/// lifecycle observer persists through a small synchronous adapter over the
/// `SharedStore` instead.
fn make_ui_sink(store: SharedStore) -> Arc<dyn EventSink> {
    Arc::new(SharedStoreSink { store })
}

/// A synchronous [`EventSink`] over a [`SharedStore`] for lifecycle persistence.
///
/// The async durable sink needs sole ownership of the `Store`; the facade holds a
/// shared handle, so this adapter appends events directly under the shared lock.
/// Best-effort: append errors are swallowed (the loop's return value also carries
/// the result), matching the infallible-fan-out contract of [`EventSink::emit`].
struct SharedStoreSink {
    store: SharedStore,
}

impl EventSink for SharedStoreSink {
    fn emit(&self, ev: PendingEvent) {
        if let Ok(store) = self.store.lock() {
            let _ = store.append_event(&ev.session_id, &ev.event_type, ev.payload);
        }
    }
}

/// Run a session on the new async engine using a resolved provider config.
///
/// This is the binary-facing facade. It seeds the environment workspace-context
/// and the optional initial user message into the durable log under `session_id`,
/// builds the real sampling driver via [`provider::resolve_provider`] (which calls
/// [`build_sampling_driver`](crate::turn::model_path::build_sampling_driver)), and
/// drives the turn loop to quiescence, persisting the run's result. Returns the
/// [`SessionId`].
///
/// The `Fake` backend is driven offline via a scripted driver (so the assembly is
/// testable network-free); every other backend is driven with the real
/// [`ModelSamplingDriver`](crate::turn::sampling::ModelSamplingDriver) (which
/// performs network I/O only when its `run_sampling_request` awaits the model
/// stream).
pub async fn run_session_with_config(
    store: SharedStore,
    session_id: &str,
    config: ProviderRunConfig,
) -> anyhow::Result<SessionId> {
    let session_id = SessionId(session_id.to_string());
    let ctx = turn_ctx(&session_id, &config);

    // (1) resolve provider → driver. This reaches `build_sampling_driver` for
    //     every real backend; `Fake` yields the offline-driver signal.
    let driver_sink: Arc<dyn EventSink> = Arc::new(DiscardSink);
    let resolved =
        provider::resolve_provider(&config, driver_sink, ctx.clone(), max_retries(&config))
            .map_err(|e| anyhow::anyhow!("{e}"))?;

    // (2) seed the environment workspace-context durable event (de-duped per kind).
    let env_content = environment_context_content(&config);
    append_environment_context_event(Arc::clone(&store), session_id.as_str(), env_content).await?;

    // The run drives over the session's existing durable history (the prompt the
    // caller already seeded). `turn_has_fresh_input` is true iff that log already
    // carries a real user turn (a `session.input` event), so the loop's initial
    // drain gate matches codex (`initial_can_drain`): fresh input is sampled
    // before any queued steer is drained.
    let turn_has_fresh_input = log_has_user_input(&store, session_id.as_str());

    // (3) drive the loop to quiescence with the resolved driver.
    match resolved {
        ResolvedProvider::Real(driver) => {
            drive_run(
                Arc::clone(&store),
                session_id.clone(),
                ctx,
                *driver,
                turn_has_fresh_input,
            )
            .await?;
        }
        ResolvedProvider::Fake => {
            // The fake backend has no real driver; drive offline so the facade is
            // exercisable end-to-end without a network.
            let driver = FakeSamplingDriver::new(fake_response_text(&config));
            drive_run(
                Arc::clone(&store),
                session_id.clone(),
                ctx,
                driver,
                turn_has_fresh_input,
            )
            .await?;
        }
    }

    // (4) return the session id of the completed run.
    Ok(session_id)
}

/// True iff the session's durable log already contains a real user turn
/// (`session.input`), used to seed the loop's initial drain gate.
fn log_has_user_input(store: &SharedStore, session_id: &str) -> bool {
    let store = store.lock().expect("store mutex poisoned");
    store
        .events_for_session(session_id)
        .map(|events| events.iter().any(|e| e.event_type == "session.input"))
        .unwrap_or(false)
}

/// The retry budget for the sampling driver (see [`DEFAULT_STREAM_MAX_RETRIES`]).
fn max_retries(_config: &ProviderRunConfig) -> u32 {
    DEFAULT_STREAM_MAX_RETRIES
}

/// The environment workspace-context content string.
///
/// Phase-E seam: the legacy stack assembles a rich `<environment_context>` block
/// (cwd + shell + network + AGENTS.md). That assembly depends on AGENTS.md loading
/// / environment snapshotting that are not ported into the agent crate yet, so the
/// facade seeds a minimal-but-real environment block carrying the configured cwd
/// (from the first environment, when present). Wave-E swaps in the full assembly.
fn environment_context_content(config: &ProviderRunConfig) -> String {
    let cwd = config
        .options
        .environment_context_environments
        .first()
        .map(|e| e.cwd.as_str())
        .unwrap_or(".");
    format!("<environment_context>\n<cwd>{cwd}</cwd>\n</environment_context>")
}

/// The fixed assistant reply the `Fake` backend emits.
///
/// Honors a configured [`ProviderRunConfig::with_fake_result`] when present
/// (parity: the legacy fake provider replays the scripted result, carried on the
/// run config's `fake_result` field), else a stable placeholder.
fn fake_response_text(config: &ProviderRunConfig) -> String {
    config
        .fake_result
        .clone()
        .unwrap_or_else(|| "(fake response)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_overrides::ProviderBackend;
    use crate::config_overrides::ProviderRunConfig;
    use browser_use_store::Store;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// A tempdir-backed `SharedStore` with a fresh session row (the `events` table
    /// has a FK on `sessions(id)`, so the session must exist before we append).
    /// Returns the `TempDir` so the caller keeps the on-disk sqlite db alive.
    fn store_with_session() -> (TempDir, SharedStore, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("open store");
        let session_id = store
            .create_session(None, std::path::Path::new("/work"))
            .expect("create session")
            .id;
        (dir, Arc::new(Mutex::new(store)), session_id)
    }

    fn events(store: &SharedStore, session_id: &str) -> Vec<browser_use_protocol::EventRecord> {
        store
            .lock()
            .unwrap()
            .events_for_session(session_id)
            .unwrap()
    }

    fn fake_config() -> ProviderRunConfig {
        ProviderRunConfig::new(ProviderBackend::Fake, "fake-model").with_fake_result("hi from fake")
    }

    /// Seed a real user turn into the durable log before driving.
    ///
    /// Appends straight through the store lock (the sync `Store::append_event`)
    /// rather than the `EventSink` trait, to avoid the two-`EventSink`-traits
    /// ambiguity in test scope.
    async fn seed_user_input(store: &SharedStore, session_id: &str, text: &str) {
        let store = store.lock().expect("store mutex poisoned");
        store
            .append_event(
                session_id,
                "session.input",
                serde_json::json!({ "text": text }),
            )
            .expect("seed user input");
    }

    /// The full config-driven facade drives the Fake backend end-to-end over a
    /// seeded session, persisting the env-context + the agent reply to quiescence.
    #[tokio::test]
    async fn config_facade_drives_fake_backend_to_quiescence() {
        let (_dir, store, session_id) = store_with_session();
        seed_user_input(&store, &session_id, "do a thing").await;

        let id = run_session_with_config(Arc::clone(&store), &session_id, fake_config())
            .await
            .expect("config facade must run the fake backend");
        assert_eq!(id.as_str(), session_id);

        let log = events(&store, &session_id);
        // env workspace-context was seeded.
        assert!(
            log.iter().any(|e| e.event_type == "workspace.context"),
            "expected a seeded workspace.context event"
        );
        // the terminal agent message was persisted by the observer.
        assert!(
            log.iter().any(|e| e.event_type == "agent.message"
                && e.payload.get("content").and_then(|v| v.as_str()) == Some("hi from fake")),
            "expected the fake assistant reply persisted; log={log:?}"
        );
    }

    /// With no user turn in the log the facade still drives (env-context only) and
    /// completes — proving the seed/loop wiring is independent of fresh input.
    #[tokio::test]
    async fn config_facade_drives_without_initial_input() {
        let (_dir, store, session_id) = store_with_session();
        run_session_with_config(Arc::clone(&store), &session_id, fake_config())
            .await
            .expect("facade must run with no user input");
        let log = events(&store, &session_id);
        assert!(log.iter().any(|e| e.event_type == "workspace.context"));
        assert!(
            !log.iter().any(|e| e.event_type == "session.input"),
            "no user input should be present"
        );
        // The agent still produced (and persisted) a reply.
        assert!(log.iter().any(|e| e.event_type == "agent.message"));
    }

    /// The cut codex backend surfaces a clear error through the facade rather than
    /// wiring chatgpt.com.
    #[tokio::test]
    async fn config_facade_rejects_codex_backend() {
        let (_dir, store, session_id) = store_with_session();
        let cfg = ProviderRunConfig::new(ProviderBackend::Codex, "codex-model");
        let err = run_session_with_config(store, &session_id, cfg)
            .await
            .expect_err("codex backend must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("codex"),
            "error should explain codex is cut: {msg}"
        );
    }

    /// The store-backed [`TurnState`] lowers the durable log into the prompt and
    /// records assistant turns back — proving the loop's conversation seam.
    #[tokio::test]
    async fn store_turn_state_lowers_history_and_records() {
        let (_dir, store, session_id) = store_with_session();
        // Seed a real user input event into the durable log (straight through the
        // store lock; see `seed_user_input` for why we avoid the EventSink trait).
        let sid = SessionId(session_id.clone());
        {
            let store = store.lock().expect("store mutex poisoned");
            store
                .append_event(
                    &session_id,
                    "session.input",
                    serde_json::json!({ "text": "hello" }),
                )
                .expect("append");
        }

        let state = StoreTurnState::new(Arc::clone(&store), sid);
        let before = state.clone_history_for_prompt().await;
        assert!(
            !before.is_empty(),
            "the seeded user message must lower into the prompt"
        );

        let assistant = Message::new(
            browser_use_llm::schema::MessageRole::Assistant,
            vec![browser_use_llm::schema::ContentPart::text("hi")],
        );
        state.record_items(std::slice::from_ref(&assistant)).await;
        let after = state.clone_history_for_prompt().await;
        assert_eq!(
            after.len(),
            before.len() + 1,
            "recorded assistant turn should be visible on the next prompt"
        );
        assert!(!state.has_pending_input().await);
        assert!(!state.token_status().await.token_limit_reached);
    }
}
