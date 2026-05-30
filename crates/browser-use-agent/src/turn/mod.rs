//! `turn/` — async turn loop + in-turn tool dispatch.

pub mod dispatch;
pub mod loop_driver;
pub mod sampling;

#[cfg(test)]
mod dispatch_tests;
#[cfg(test)]
mod sampling_tests;

use crate::decision;
use browser_use_llm::schema::Message;
use tokio_util::sync::CancellationToken;

/// Reads/writes conversation state. Impl over `ContextManager` + `Session`; tests
/// use an `InMemoryTurnState`.
pub trait TurnState: Send + Sync + 'static {
    fn clone_history_for_prompt(&self) -> impl std::future::Future<Output = Vec<Message>> + Send;
    fn record_items(&self, items: &[Message]) -> impl std::future::Future<Output = ()> + Send;
    fn has_pending_input(&self) -> impl std::future::Future<Output = bool> + Send;
    fn take_pending_input(&self) -> impl std::future::Future<Output = Vec<Message>> + Send;
    fn token_status(&self) -> impl std::future::Future<Output = decision::TokenStatus> + Send;
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

pub use dispatch::{ToolDispatchResult, ToolDispatcher};
pub use loop_driver::TurnLoop;
