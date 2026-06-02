//! `Lifecycle` — normalizes a provider's stream into a well-formed
//! `step_start → (text|reasoning start/delta/end)* → step_finish → finish`
//! sequence, regardless of whether the provider emits explicit block
//! start/end events. Pure, synchronous, deterministic.

use std::collections::BTreeSet;

use crate::schema::{FinishReason, LlmEvent, TextPhase, Usage};

#[derive(Debug, Default)]
pub struct Lifecycle {
    step_started: bool,
    finished: bool,
    open_text: BTreeSet<String>,
    open_reasoning: BTreeSet<String>,
}

impl Lifecycle {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_step(&mut self, out: &mut Vec<LlmEvent>) {
        if !self.step_started {
            self.step_started = true;
            out.push(LlmEvent::StepStart);
        }
    }

    /// Record a text delta. Emits `StepStart` (once) and `TextStart` (the first
    /// time this id is seen) before the `TextDelta`.
    pub fn text_delta(&mut self, id: impl Into<String>, delta: impl Into<String>) -> Vec<LlmEvent> {
        let id = id.into();
        let mut out = Vec::new();
        self.ensure_step(&mut out);
        if self.open_text.insert(id.clone()) {
            out.push(LlmEvent::TextStart { id: id.clone() });
        }
        out.push(LlmEvent::TextDelta {
            id,
            delta: delta.into(),
        });
        out
    }

    /// Close a text block (no-op if it was never opened / already closed).
    pub fn text_end(&mut self, id: impl Into<String>) -> Vec<LlmEvent> {
        self.text_end_with_phase(id, None)
    }

    pub fn text_end_with_phase(
        &mut self,
        id: impl Into<String>,
        phase: Option<TextPhase>,
    ) -> Vec<LlmEvent> {
        let id = id.into();
        if self.open_text.remove(&id) {
            vec![LlmEvent::TextEnd { id, phase }]
        } else {
            Vec::new()
        }
    }

    /// Record a reasoning delta (same contract as `text_delta`).
    pub fn reasoning_delta(
        &mut self,
        id: impl Into<String>,
        delta: impl Into<String>,
    ) -> Vec<LlmEvent> {
        let id = id.into();
        let mut out = Vec::new();
        self.ensure_step(&mut out);
        if self.open_reasoning.insert(id.clone()) {
            out.push(LlmEvent::ReasoningStart { id: id.clone() });
        }
        out.push(LlmEvent::ReasoningDelta {
            id,
            delta: delta.into(),
        });
        out
    }

    /// Close a reasoning block (no-op if never opened / already closed).
    pub fn reasoning_end(&mut self, id: impl Into<String>) -> Vec<LlmEvent> {
        let id = id.into();
        if self.open_reasoning.remove(&id) {
            vec![LlmEvent::ReasoningEnd { id }]
        } else {
            Vec::new()
        }
    }

    /// Finish the turn: auto-close any dangling blocks, then emit `StepFinish`
    /// and `Finish`. Idempotent — a second call returns no events.
    pub fn finish(&mut self, usage: Usage, finish_reason: Option<FinishReason>) -> Vec<LlmEvent> {
        if self.finished {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.ensure_step(&mut out);

        let open_text: Vec<String> = std::mem::take(&mut self.open_text).into_iter().collect();
        for id in open_text {
            out.push(LlmEvent::TextEnd { id, phase: None });
        }
        let open_reasoning: Vec<String> = std::mem::take(&mut self.open_reasoning)
            .into_iter()
            .collect();
        for id in open_reasoning {
            out.push(LlmEvent::ReasoningEnd { id });
        }

        self.finished = true;
        out.push(LlmEvent::StepFinish {
            usage,
            finish_reason,
        });
        out.push(LlmEvent::Finish {
            usage,
            finish_reason,
        });
        out
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_emit_step_and_block_start_once() {
        let mut lc = Lifecycle::new();
        assert_eq!(
            lc.text_delta("t0", "he"),
            vec![
                LlmEvent::StepStart,
                LlmEvent::TextStart { id: "t0".into() },
                LlmEvent::TextDelta {
                    id: "t0".into(),
                    delta: "he".into()
                },
            ]
        );
        // second delta: no StepStart, no TextStart
        assert_eq!(
            lc.text_delta("t0", "llo"),
            vec![LlmEvent::TextDelta {
                id: "t0".into(),
                delta: "llo".into()
            }]
        );
    }

    #[test]
    fn finish_auto_closes_dangling_blocks() {
        let mut lc = Lifecycle::new();
        lc.text_delta("t0", "hi");
        lc.reasoning_delta("r0", "think");
        let evts = lc.finish(Usage::default(), Some(FinishReason::Stop));
        // dangling text + reasoning get end events before step_finish/finish
        assert_eq!(
            evts,
            vec![
                LlmEvent::TextEnd {
                    id: "t0".into(),
                    phase: None,
                },
                LlmEvent::ReasoningEnd { id: "r0".into() },
                LlmEvent::StepFinish {
                    usage: Usage::default(),
                    finish_reason: Some(FinishReason::Stop)
                },
                LlmEvent::Finish {
                    usage: Usage::default(),
                    finish_reason: Some(FinishReason::Stop)
                },
            ]
        );
    }

    #[test]
    fn explicit_end_is_not_double_closed_on_finish() {
        let mut lc = Lifecycle::new();
        lc.text_delta("t0", "hi");
        assert_eq!(
            lc.text_end("t0"),
            vec![LlmEvent::TextEnd {
                id: "t0".into(),
                phase: None,
            }]
        );
        let evts = lc.finish(Usage::default(), None);
        // no extra TextEnd, just step_finish + finish
        assert_eq!(
            evts,
            vec![
                LlmEvent::StepFinish {
                    usage: Usage::default(),
                    finish_reason: None
                },
                LlmEvent::Finish {
                    usage: Usage::default(),
                    finish_reason: None
                },
            ]
        );
    }

    #[test]
    fn finish_is_idempotent() {
        let mut lc = Lifecycle::new();
        lc.text_delta("t0", "hi");
        let first = lc.finish(Usage::default(), None);
        assert!(!first.is_empty());
        assert!(lc.is_finished());
        assert_eq!(lc.finish(Usage::default(), None), Vec::new());
    }

    #[test]
    fn end_without_start_is_noop() {
        let mut lc = Lifecycle::new();
        assert_eq!(lc.text_end("nope"), Vec::new());
    }
}
