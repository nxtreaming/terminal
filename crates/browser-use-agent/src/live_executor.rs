use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{anyhow, Context, Result};
use browser_use_protocol::EventRecord;
use browser_use_runtime::{
    AgentId, AttachChildAgentRequest, AttachRootAgentRequest, BrowserId as RuntimeBrowserId,
    MailboxDeliveryPhase as RuntimeMailboxDeliveryPhase, RunAgentRequest, RuntimeHandle,
    SessionId as RuntimeSessionId,
};
use browser_use_store::{Store, StoreNotifier};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::config_overrides::ProviderRunConfig;
use crate::entrypoint::RuntimeTurnDriver;
use crate::session::SharedStore;

#[derive(Clone)]
pub struct RuntimeAgentExecutor {
    inner: Arc<RuntimeAgentExecutorInner>,
}

struct RuntimeAgentExecutorInner {
    state_dir: PathBuf,
    runtime: RuntimeHandle,
    notifier: Option<StoreNotifier>,
    tokio: tokio::runtime::Runtime,
}

#[derive(Clone)]
pub struct RuntimeAgentExecutorConfig {
    pub state_dir: PathBuf,
    pub runtime: RuntimeHandle,
    pub notifier: Option<StoreNotifier>,
    pub worker_threads: usize,
}

impl RuntimeAgentExecutorConfig {
    pub fn new(state_dir: impl Into<PathBuf>, runtime: RuntimeHandle) -> Self {
        Self {
            state_dir: state_dir.into(),
            runtime,
            notifier: None,
            worker_threads: 2,
        }
    }

    pub fn with_notifier(mut self, notifier: Option<StoreNotifier>) -> Self {
        self.notifier = notifier;
        self
    }

    pub fn with_worker_threads(mut self, worker_threads: usize) -> Self {
        self.worker_threads = worker_threads.max(1);
        self
    }
}

#[derive(Clone)]
pub struct RuntimeAgentRunRequest {
    pub session_id: String,
    pub config: ProviderRunConfig,
    pub browser_id: Option<RuntimeBrowserId>,
    pub cancellation_token: Option<CancellationToken>,
}

impl RuntimeAgentRunRequest {
    pub fn new(session_id: impl Into<String>, config: ProviderRunConfig) -> Self {
        Self {
            session_id: session_id.into(),
            config,
            browser_id: None,
            cancellation_token: None,
        }
    }

    pub fn with_browser_id(mut self, browser_id: RuntimeBrowserId) -> Self {
        self.browser_id = Some(browser_id);
        self
    }

    pub fn with_cancellation_token(mut self, cancellation_token: CancellationToken) -> Self {
        self.cancellation_token = Some(cancellation_token);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeAgentRunResult {
    pub session_id: String,
}

pub struct RuntimeAgentBackgroundCompletion {
    pub session_id: String,
    pub result: Result<RuntimeAgentRunResult>,
}

impl RuntimeAgentBackgroundCompletion {
    pub fn is_success(&self) -> bool {
        self.result.is_ok()
    }

    pub fn error_message(&self) -> Option<String> {
        self.result.as_ref().err().map(|error| format!("{error:#}"))
    }
}

impl RuntimeAgentExecutor {
    pub fn new(config: RuntimeAgentExecutorConfig) -> Result<Self> {
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("browser-use-live-agent-runtime")
            .worker_threads(config.worker_threads)
            .build()
            .context("build live agent tokio runtime")?;
        Ok(Self {
            inner: Arc::new(RuntimeAgentExecutorInner {
                state_dir: config.state_dir,
                runtime: config.runtime,
                notifier: config.notifier,
                tokio,
            }),
        })
    }

    pub fn state_dir(&self) -> &Path {
        &self.inner.state_dir
    }

    pub fn runtime_handle(&self) -> RuntimeHandle {
        self.inner.runtime.clone()
    }

    pub fn ensure_agent_attached(
        &self,
        session_id: &str,
        max_concurrent_threads_per_session: usize,
    ) -> Result<()> {
        let store = Store::open(&self.inner.state_dir)?;
        ensure_agent_attached(
            &self.inner.runtime,
            &store,
            session_id,
            max_concurrent_threads_per_session,
        )
    }

    pub fn run_blocking(&self, request: RuntimeAgentRunRequest) -> Result<RuntimeAgentRunResult> {
        let runtime_session_id = RuntimeSessionId::from_string(request.session_id.clone())?;
        let store =
            Store::open_with_optional_notifier(&self.inner.state_dir, self.inner.notifier.clone())?;
        ensure_agent_attached(
            &self.inner.runtime,
            &store,
            &request.session_id,
            request
                .config
                .options
                .multi_agent_v2
                .max_concurrent_threads_per_session,
        )?;
        let initial_input = latest_runtime_or_store_durable_prompt_input_event(
            &self.inner.runtime,
            &store,
            &runtime_session_id,
            &request.session_id,
        )?
        .map(|event| {
            json!({
                "source": "durable_prompt_input",
                "event_type": event.event_type,
                "source_event_seq": event.seq,
                "payload": event.payload,
            })
        });
        let session_meta = store
            .load_session(&request.session_id)?
            .with_context(|| format!("unknown session id: {}", request.session_id))?;
        let run_cwd = PathBuf::from(&session_meta.cwd);
        let shared_store: SharedStore = Arc::new(Mutex::new(store));
        let initial_cancel = request
            .cancellation_token
            .unwrap_or_else(CancellationToken::new);
        let provider_config = json!({
            "backend": format!("{:?}", request.config.backend),
            "model": request.config.model,
            "runtime_agent_executor": true,
        });
        let agent_id = AgentId::from_string(request.session_id.clone())?;
        let runtime = self.inner.runtime.clone();
        let request_session_id = request.session_id.clone();
        let config = request.config;
        let shared_store_for_loop = Arc::clone(&shared_store);
        let request_session_id_for_loop = request_session_id.clone();
        let result = self.inner.tokio.block_on(async move {
            let mut cancel = initial_cancel;
            let mut restart_count = 0usize;
            loop {
                let runtime_request = RunAgentRequest::new(runtime_session_id.clone())
                    .with_agent_id(agent_id.clone())
                    .with_provider_config(provider_config.clone())
                    .with_cwd(run_cwd.clone())
                    .with_input_source("runtime_agent_executor")
                    .with_cancellation_token(cancel.clone());
                let runtime_request = if let Some(initial_input) = initial_input.clone() {
                    runtime_request.with_initial_input(initial_input)
                } else {
                    runtime_request
                };
                let runtime_request = if let Some(browser_id) = request.browser_id.clone() {
                    runtime_request.with_browser_id(browser_id)
                } else {
                    runtime_request
                };
                let runner_runtime = runtime.clone();
                let shared_store_for_run = Arc::clone(&shared_store_for_loop);
                let request_session_id_for_run = request_session_id_for_loop.clone();
                let config_for_run = config.clone();
                let cancel_for_run = cancel.clone();
                let run_result = runtime
                    .run_agent(runtime_request, async move {
                        RuntimeTurnDriver::new(
                            shared_store_for_run,
                            request_session_id_for_run,
                            config_for_run,
                            cancel_for_run,
                        )
                        .with_runtime_handle(Some(runner_runtime))
                        .run()
                        .await
                    })
                    .await;

                match run_result {
                    Ok(response) => break Ok(response),
                    Err(_)
                        if cancel.is_cancelled()
                            && restart_count < 16
                            && runtime_has_trigger_turn_mail(&runtime, &runtime_session_id) =>
                    {
                        restart_count = restart_count.saturating_add(1);
                        cancel = CancellationToken::new();
                        continue;
                    }
                    Err(error) => break Err(error),
                }
            }
        });
        match result {
            Ok(response) => Ok(RuntimeAgentRunResult {
                session_id: response.output.0,
            }),
            Err(error) => {
                append_session_failed_if_missing(
                    &shared_store,
                    &request_session_id,
                    &format!("{error:#}"),
                );
                Err(error)
            }
        }
    }

    pub fn spawn_background(
        &self,
        thread_name: impl Into<String>,
        request: RuntimeAgentRunRequest,
        on_completion: impl FnOnce(RuntimeAgentBackgroundCompletion) + Send + 'static,
    ) -> Result<()> {
        let executor = self.clone();
        let session_id = request.session_id.clone();
        thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                let result = match catch_unwind(AssertUnwindSafe(|| executor.run_blocking(request)))
                {
                    Ok(result) => result,
                    Err(panic) => {
                        let message =
                            format!("agent executor panicked: {}", panic_payload_message(panic));
                        append_session_failed_to_state_dir(
                            executor.state_dir(),
                            &session_id,
                            &message,
                            executor.inner.notifier.clone(),
                        );
                        Err(anyhow!(message))
                    }
                };
                on_completion(RuntimeAgentBackgroundCompletion { session_id, result });
            })
            .context("spawn live agent executor thread")?;
        Ok(())
    }
}

fn latest_durable_prompt_input_event(
    store: &Store,
    session_id: &str,
) -> Result<Option<EventRecord>> {
    Ok(latest_durable_prompt_input_from_events(
        store.events_for_session(session_id)?,
    ))
}

fn latest_runtime_or_store_durable_prompt_input_event(
    runtime: &RuntimeHandle,
    store: &Store,
    runtime_session_id: &RuntimeSessionId,
    store_session_id: &str,
) -> Result<Option<EventRecord>> {
    if let Ok(events) = runtime.events_for_session(runtime_session_id) {
        if let Some(event) = latest_durable_prompt_input_from_events(events) {
            return Ok(Some(event));
        }
    }
    latest_durable_prompt_input_event(store, store_session_id)
}

fn latest_durable_prompt_input_from_events(events: Vec<EventRecord>) -> Option<EventRecord> {
    events.into_iter().rev().find(|event| {
        matches!(
            event.event_type.as_str(),
            "session.input" | "session.followup" | "agent.mailbox_input"
        )
    })
}

fn runtime_has_trigger_turn_mail(runtime: &RuntimeHandle, session_id: &RuntimeSessionId) -> bool {
    runtime
        .has_pending_trigger_turn_agent_mail_for_session(
            session_id,
            RuntimeMailboxDeliveryPhase::CurrentTurn,
        )
        .unwrap_or(false)
        || runtime
            .has_pending_trigger_turn_agent_mail_for_session(
                session_id,
                RuntimeMailboxDeliveryPhase::NextTurn,
            )
            .unwrap_or(false)
}

#[deprecated(note = "use RuntimeAgentExecutor; LiveAgentExecutor is a compatibility alias")]
pub type LiveAgentExecutor = RuntimeAgentExecutor;

#[deprecated(note = "use RuntimeAgentExecutorConfig")]
pub type LiveAgentExecutorConfig = RuntimeAgentExecutorConfig;

#[deprecated(note = "use RuntimeAgentRunRequest")]
pub type LiveAgentRunRequest = RuntimeAgentRunRequest;

#[deprecated(note = "use RuntimeAgentRunResult")]
pub type LiveAgentRunResult = RuntimeAgentRunResult;

#[deprecated(note = "use RuntimeAgentBackgroundCompletion")]
pub type LiveAgentBackgroundCompletion = RuntimeAgentBackgroundCompletion;

pub fn ensure_agent_attached(
    runtime: &RuntimeHandle,
    store: &Store,
    session_id: &str,
    max_concurrent_threads_per_session: usize,
) -> Result<()> {
    let agent_id = AgentId::from_string(session_id.to_string())?;
    if runtime.agents().thread(&agent_id).is_ok() {
        return Ok(());
    }

    let session = store
        .load_session(session_id)?
        .with_context(|| format!("unknown session id: {session_id}"))?;
    if let Some(parent_session_id) = session.parent_id.as_deref() {
        ensure_agent_attached(
            runtime,
            store,
            parent_session_id,
            max_concurrent_threads_per_session,
        )?;
        let summary = store
            .agent_summary_for_child(session_id)?
            .with_context(|| format!("missing agent edge for child session id: {session_id}"))?;
        runtime.attach_child_agent(AttachChildAgentRequest {
            parent_agent_id: AgentId::from_string(parent_session_id.to_string())?,
            child_agent_id: agent_id,
            child_session_id: RuntimeSessionId::from_string(session_id.to_string())?,
            cwd: PathBuf::from(&session.cwd),
            agent_path: summary
                .agent_path
                .unwrap_or_else(|| format!("/root/{session_id}")),
            nickname: summary.agent_nickname,
            role: summary.agent_role,
        })?;
    } else {
        runtime.attach_root_agent(AttachRootAgentRequest {
            session_id: RuntimeSessionId::from_string(session_id.to_string())?,
            cwd: PathBuf::from(&session.cwd),
            task: session_id.to_string(),
            max_concurrent_threads_per_session,
        })?;
    }
    Ok(())
}

fn append_session_failed_if_missing(shared_store: &SharedStore, session_id: &str, error: &str) {
    if let Ok(store) = shared_store.lock() {
        append_session_failed(&store, session_id, error);
    }
}

fn append_session_failed_to_state_dir(
    state_dir: &Path,
    session_id: &str,
    error: &str,
    notifier: Option<StoreNotifier>,
) {
    if let Ok(store) = Store::open_with_optional_notifier(state_dir, notifier) {
        append_session_failed(&store, session_id, error);
    }
}

fn append_session_failed(store: &Store, session_id: &str, error: &str) {
    let already_failed = store
        .events_for_session(session_id)
        .map(|events| {
            events
                .iter()
                .any(|event| event.event_type == "session.failed")
        })
        .unwrap_or(false);
    if already_failed {
        return;
    }
    let _ = store.append_event(
        session_id,
        "session.failed",
        serde_json::json!({ "error": error }),
    );
}

fn panic_payload_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_use_runtime::{
        BrowserUseRuntime, CreateRootAgentRequest, Durability, LiveThreadPersistence,
        SqliteJournal, StateIndex,
    };
    use serde_json::json;
    use tempfile::TempDir;

    fn sqlite_runtime(state_dir: &Path) -> Result<RuntimeHandle> {
        let journal = Arc::new(SqliteJournal::open(state_dir)?);
        let persistence: Arc<dyn LiveThreadPersistence> = journal.clone();
        let state_index: Arc<dyn StateIndex> = journal;
        Ok(BrowserUseRuntime::new(persistence, state_index).handle())
    }

    fn create_runtime_root(runtime: &RuntimeHandle, cwd: &Path) -> Result<RuntimeSessionId> {
        let root = runtime.create_root_agent(CreateRootAgentRequest {
            cwd: cwd.to_path_buf(),
            task: "runtime root".to_string(),
            max_concurrent_threads_per_session: 3,
        })?;
        Ok(root.session_id().clone())
    }

    #[test]
    fn durable_prompt_input_prefers_runtime_journal_over_store() -> Result<()> {
        let store_dir = TempDir::new()?;
        let store = Store::open(store_dir.path())?;
        let session = store.create_session(None, Path::new("/work"))?;
        store.append_event(
            &session.id,
            "session.input",
            json!({ "text": "stale store input" }),
        )?;

        let (runtime, _journal) = BrowserUseRuntime::memory();
        let runtime = runtime.handle();
        let runtime_session_id = create_runtime_root(&runtime, Path::new("/work"))?;
        let runtime_input = runtime.append_observed_session_event(
            runtime_session_id.clone(),
            "session.input",
            json!({ "text": "runtime input" }),
            Durability::Barrier,
        )?;

        let selected = latest_runtime_or_store_durable_prompt_input_event(
            &runtime,
            &store,
            &runtime_session_id,
            &session.id,
        )?
        .context("missing selected durable input")?;

        assert_eq!(selected.seq, runtime_input.seq.unwrap());
        assert_eq!(selected.payload["text"], "runtime input");
        Ok(())
    }

    #[test]
    fn durable_prompt_input_falls_back_to_store_when_runtime_has_none() -> Result<()> {
        let store_dir = TempDir::new()?;
        let store = Store::open(store_dir.path())?;
        let session = store.create_session(None, Path::new("/work"))?;
        let store_input = store.append_event(
            &session.id,
            "session.input",
            json!({ "text": "store replay input" }),
        )?;

        let (runtime, _journal) = BrowserUseRuntime::memory();
        let runtime = runtime.handle();
        let runtime_session_id = create_runtime_root(&runtime, Path::new("/work"))?;

        let selected = latest_runtime_or_store_durable_prompt_input_event(
            &runtime,
            &store,
            &runtime_session_id,
            &session.id,
        )?
        .context("missing selected durable input")?;

        assert_eq!(selected.seq, store_input.seq);
        assert_eq!(selected.payload["text"], "store replay input");
        Ok(())
    }

    #[tokio::test]
    async fn durable_prompt_input_is_accepted_once_without_mailbox() -> Result<()> {
        let dir = TempDir::new()?;
        let store = Store::open(dir.path())?;
        let session = store.create_session(None, Path::new("/work"))?;
        let input = store.append_event(
            &session.id,
            "session.input",
            json!({ "text": "inspect the repo" }),
        )?;
        let runtime = sqlite_runtime(dir.path())?;
        ensure_agent_attached(&runtime, &store, &session.id, 3)?;

        let runtime_session_id = RuntimeSessionId::from_string(session.id.clone())?;
        let agent_id = AgentId::from_string(session.id.clone())?;
        let initial_input = json!({
            "source": "durable_prompt_input",
            "event_type": input.event_type,
            "source_event_seq": input.seq,
            "payload": input.payload,
        });
        let runtime_for_run = runtime.clone();
        let runtime_session_id_for_run = runtime_session_id.clone();
        runtime
            .run_agent(
                RunAgentRequest::new(runtime_session_id.clone())
                    .with_agent_id(agent_id.clone())
                    .with_initial_input(initial_input.clone()),
                async move {
                    let thread = runtime_for_run.agents().thread(&agent_id)?;
                    assert!(thread.mailbox().pending_items().is_empty());
                    let live = thread.live_state_snapshot();
                    assert_eq!(live.accepted_input_count, 1);
                    assert_eq!(live.pending_prompt_input_count, 1);
                    assert_eq!(live.last_accepted_prompt_input_seq, input.seq);
                    let consumed = runtime_for_run
                        .consume_prompt_input_for_session(&runtime_session_id_for_run)?;
                    assert!(consumed.consumed);
                    Ok::<_, anyhow::Error>(())
                },
            )
            .await?;

        let agent_id = AgentId::from_string(session.id.clone())?;
        let thread = runtime.agents().thread(&agent_id)?;
        assert!(thread.mailbox().pending_items().is_empty());

        let runtime_for_rerun = runtime.clone();
        let runtime_session_id_for_rerun = runtime_session_id.clone();
        runtime
            .run_agent(
                RunAgentRequest::new(runtime_session_id.clone())
                    .with_agent_id(agent_id.clone())
                    .with_initial_input(initial_input),
                async move {
                    let consumed = runtime_for_rerun
                        .consume_prompt_input_for_session(&runtime_session_id_for_rerun)?;
                    assert!(
                        !consumed.consumed,
                        "same durable input seq must not be re-accepted by run_agent"
                    );
                    Ok::<_, anyhow::Error>(())
                },
            )
            .await?;
        let live = thread.live_state_snapshot();
        assert_eq!(live.accepted_input_count, 1);
        assert_eq!(live.pending_prompt_input_count, 0);
        assert_eq!(live.last_consumed_prompt_input_seq, input.seq);

        let event_types = store
            .events_for_session(&session.id)?
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert_eq!(
            event_types
                .iter()
                .filter(|event_type| event_type.as_str() == "agent.input.accepted")
                .count(),
            1
        );
        assert_eq!(
            event_types
                .iter()
                .filter(|event_type| event_type.as_str() == "agent.input.consumed")
                .count(),
            1
        );
        assert!(event_types
            .iter()
            .all(|event_type| event_type != "mailbox.enqueued"));
        Ok(())
    }
}
