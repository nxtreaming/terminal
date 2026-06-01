//! `turn/` — async turn loop + in-turn tool dispatch.

pub mod dispatch;
pub mod loop_driver;
pub mod model_path;
pub mod sampling;

#[cfg(test)]
mod dispatch_tests;
#[cfg(test)]
mod fusion_tests;
#[cfg(test)]
mod loop_tests;
#[cfg(test)]
mod sampling_tests;

use crate::decision;
use browser_use_llm::schema::Message;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionMode {
    PreTurn,
    MidTurn,
}

/// Reads/writes conversation state. Impl over `ContextManager` + `Session`; tests
/// use an `InMemoryTurnState`.
pub trait TurnState: Send + Sync + 'static {
    fn clone_history_for_prompt(&self) -> impl std::future::Future<Output = Vec<Message>> + Send;
    fn record_items(&self, items: &[Message]) -> impl std::future::Future<Output = ()> + Send;
    fn has_pending_input(&self) -> impl std::future::Future<Output = bool> + Send;
    fn take_pending_input(&self) -> impl std::future::Future<Output = Vec<Message>> + Send;
    fn token_status(&self) -> impl std::future::Future<Output = decision::TokenStatus> + Send;

    /// Mid-turn compaction hook, invoked by [`TurnLoop`] on a
    /// [`decision::LoopStep::CompactThenContinue`] step (codex `turn.rs:282`).
    ///
    /// The real model-based compaction work package is not built yet, so the
    /// default body is a no-op: the loop's CONTROL FLOW around compaction is
    /// codex-faithful (compact-then-continue, drain gate set per the decision)
    /// even while the compaction body is a stub. The production `TurnState`
    /// (over `ContextManager` + `Session`) overrides this to summarize history
    /// and reset token accounting; the loop tests override it to assert the hook
    /// fired exactly when `token_limit_reached && needs_follow_up`.
    fn compact(
        &self,
        _mode: CompactionMode,
    ) -> impl std::future::Future<Output = Result<(), crate::AgentError>> + Send {
        async { Ok(()) }
    }
}

/// One sampling round-trip + ordered tool dispatch (`turn.rs:892/1655/1873`).
/// Returns a `SamplingOutcome`.
pub trait SamplingDriver: Send + Sync + 'static {
    fn run_sampling_request(
        &self,
        input: Vec<Message>,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = Result<decision::SamplingOutcome, crate::AgentError>> + Send;
}

/// Side-effect sink for the loop (delegates to `events::EventSink` + lifecycle).
pub trait TurnObserver: Send + Sync + 'static {
    fn on_lifecycle(&self, ev: crate::task::TurnLifecycleEvent);
}

pub use dispatch::{
    CallRunner, OrchestratorRunner, RegistryRunner, ToolDispatchResult, ToolDispatcher,
};
pub use loop_driver::TurnLoop;
pub use model_path::{
    build_route, build_sampling_driver, build_transport, provider_choice_from_env, ModelPathError,
    ProviderChoice,
};
pub use sampling::FusionRecorder;
