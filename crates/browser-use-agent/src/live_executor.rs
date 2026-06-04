use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{anyhow, Context, Result};
use browser_use_runtime::{
    AgentId, AttachChildAgentRequest, AttachRootAgentRequest,
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
    pub cancellation_token: Option<CancellationToken>,
}

impl RuntimeAgentRunRequest {
    pub fn new(session_id: impl Into<String>, config: ProviderRunConfig) -> Self {
        Self {
            session_id: session_id.into(),
            config,
            cancellation_token: None,
        }
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
