//! In-turn tool dispatch: `FuturesOrdered` + `RwLock` gate (codex `turn.rs:106` + `parallel.rs`).

use browser_use_llm::schema::{ContentPart, Message};
use tokio_util::sync::CancellationToken;

pub struct ToolDispatchResult {
    pub outputs_in_order: Vec<Message>,
    pub needs_follow_up: bool,
}

pub struct ToolDispatcher {
    // gate: Arc<tokio::sync::RwLock<()>>, toolset
}

impl ToolDispatcher {
    pub fn new() -> Self {
        unimplemented!()
    }

    /// Records in MODEL order; honors cancel + drains in-flight calls.
    pub async fn dispatch_ordered(
        &self,
        _calls: Vec<ContentPart>,
        _cancel: CancellationToken,
    ) -> ToolDispatchResult {
        unimplemented!()
    }
}

impl Default for ToolDispatcher {
    fn default() -> Self {
        Self::new()
    }
}
