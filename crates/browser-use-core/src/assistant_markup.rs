//! Hidden-assistant-markup and plan-mode text processing extracted from `lib.rs`
//! (Phase 0.1 carve).
//!
//! Code motion only — behavior is byte-identical to the original definitions.

use crate::constants::*;
use crate::{assistant_delta_to_append, strip_memory_citations, CollaborationModeKind};

fn split_proposed_plan_blocks(text: &str) -> (String, Option<String>) {
    let mut visible = String::new();
    let mut latest_plan = None;
    let mut current_plan = String::new();
    let mut in_plan = false;
    let mut saw_plan = false;

    for line in text.split_inclusive('\n') {
        let line_without_newline = line.strip_suffix('\n').unwrap_or(line);
        let slug = line_without_newline.trim_start().trim_end();
        if !in_plan && slug == PROPOSED_PLAN_OPEN_TAG {
            in_plan = true;
            saw_plan = true;
            current_plan.clear();
            continue;
        }
        if in_plan && slug == PROPOSED_PLAN_CLOSE_TAG {
            in_plan = false;
            latest_plan = Some(current_plan.clone());
            continue;
        }
        if in_plan {
            current_plan.push_str(line);
        } else {
            visible.push_str(line);
        }
    }

    if in_plan && saw_plan {
        latest_plan = Some(current_plan);
    }
    (visible, latest_plan)
}

fn strip_hidden_assistant_markup_for_stream(
    text: &str,
    mode: CollaborationModeKind,
) -> (String, Option<String>) {
    let (visible, proposed_plan) = strip_hidden_assistant_markup(text, mode);
    (
        trim_partial_hidden_assistant_tag_suffix(&visible, mode),
        proposed_plan,
    )
}

fn trim_partial_hidden_assistant_tag_suffix(text: &str, mode: CollaborationModeKind) -> String {
    let tags: &[&str] = if mode == CollaborationModeKind::Plan {
        &[OAI_MEMORY_CITATION_OPEN_TAG, PROPOSED_PLAN_OPEN_TAG]
    } else {
        &[OAI_MEMORY_CITATION_OPEN_TAG]
    };
    for (idx, _) in text.char_indices() {
        let suffix = &text[idx..];
        if tags
            .iter()
            .any(|tag| suffix.len() < tag.len() && tag.starts_with(suffix))
        {
            return text[..idx].to_string();
        }
    }
    text.to_string()
}

#[derive(Clone, Debug)]
pub(crate) struct HiddenAssistantMarkupStreamFilter {
    mode: CollaborationModeKind,
    raw_text: String,
    visible_text: String,
}

impl HiddenAssistantMarkupStreamFilter {
    pub(crate) fn new(mode: CollaborationModeKind) -> Self {
        Self {
            mode,
            raw_text: String::new(),
            visible_text: String::new(),
        }
    }

    pub(crate) fn push_provider_text(&mut self, incoming: &str) -> Option<(String, Option<String>)> {
        let raw_delta = assistant_delta_to_append(&self.raw_text, incoming)?;
        self.raw_text.push_str(&raw_delta);
        let visible = strip_hidden_assistant_markup_for_stream(&self.raw_text, self.mode).0;
        let visible_delta = assistant_delta_to_append(&self.visible_text, &visible);
        if let Some(delta) = visible_delta.as_deref() {
            self.visible_text.push_str(delta);
        }
        Some((raw_delta, visible_delta))
    }
}

pub(crate) fn strip_hidden_assistant_markup(
    text: &str,
    mode: CollaborationModeKind,
) -> (String, Option<String>) {
    let without_citations = strip_memory_citations(text);
    if mode == CollaborationModeKind::Plan {
        split_proposed_plan_blocks(&without_citations)
    } else {
        (without_citations, None)
    }
}

pub(crate) fn visible_assistant_text_and_proposed_plan(
    text: &str,
    mode: CollaborationModeKind,
) -> (String, Option<String>) {
    strip_hidden_assistant_markup(text, mode)
}
