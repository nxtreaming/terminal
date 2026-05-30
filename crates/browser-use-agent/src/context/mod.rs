//! `context/` — `ContextManager` + REAL token accounting. Pure core, thin async wrapper.
//!
//! `Item` == browser-use-protocol canonical transcript item (`ResponseItem` equivalent).
//! Until protocol exposes it, the frozen surface aliases `serde_json::Value` (the legacy
//! provider-message currency, which is what session reconstruction returns today).

pub mod accounting;
pub mod assembly;
pub mod constants;
pub mod image_estimate;
pub mod inject;
pub mod normalize;

#[cfg(test)]
mod tests_accounting;

use browser_use_llm::schema::Message;

/// FROZEN ALIAS; swap to `protocol::ResponseItem` when available (open q).
pub type Item = serde_json::Value;
/// Modality probe; the real enum lives in route capabilities.
pub type InputModality = browser_use_llm::schema::ContentPart;

/// Async wrapper — the ONLY non-pure surface. browser-use-store = WRITE-SINK + notify.
pub struct ContextManager {
    // items, history_version, token_info, reference_context_item, sink
}

impl ContextManager {
    pub fn new() -> Self {
        unimplemented!()
    }

    pub fn record_items<I: IntoIterator<Item = Item>>(
        &mut self,
        _items: I,
        _p: assembly::TruncationPolicy,
    ) {
        unimplemented!()
    }

    pub fn snapshot_for_prompt(&self, _supports_image: bool) -> Vec<Item> {
        unimplemented!()
    }

    /// `Item` -> browser-use-llm `Message` for the chat request.
    pub fn lower_to_messages(&self, _items: &[Item]) -> Vec<Message> {
        unimplemented!()
    }

    pub fn update_token_info(&mut self, _u: &accounting::TokenUsage, _window: Option<i64>) {
        unimplemented!()
    }

    pub fn set_token_usage_full(&mut self, _window: i64) {
        unimplemented!()
    }

    pub fn total_token_usage(&self, _server_reasoning_included: bool) -> i64 {
        unimplemented!()
    }

    pub fn breakdown(&self) -> assembly::TotalTokenUsageBreakdown {
        unimplemented!()
    }

    pub fn history_version(&self) -> u64 {
        unimplemented!()
    }

    /// Write + notify, never read back.
    pub async fn persist_snapshot(&self) -> anyhow::Result<()> {
        unimplemented!()
    }
}

impl Default for ContextManager {
    fn default() -> Self {
        Self::new()
    }
}
