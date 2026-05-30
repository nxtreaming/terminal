//! `testkit/` — deterministic, network-free fakes shared by every WP's tests.
//!
//! Mirrors `browser-use-llm`'s `tool_runtime` scripted-source pattern: every async
//! surface is driven by scripted fakes so no network/disk is touched, and every
//! behavior branch is proven on the PURE fn it delegates to. The shared recorder is
//! provided in WP-A0; each later WP fleshes out the fakes it needs.

use crate::events::{EventSink, PendingEvent};

/// Records every emitted [`PendingEvent`] for assertion in tests.
#[derive(Default)]
pub struct RecordingSink {
    pub events: std::sync::Mutex<Vec<PendingEvent>>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn drain(&self) -> Vec<PendingEvent> {
        std::mem::take(&mut self.events.lock().expect("recording sink poisoned"))
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, ev: PendingEvent) {
        self.events
            .lock()
            .expect("recording sink poisoned")
            .push(ev);
    }
}
