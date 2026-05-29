//! Memory-citation parsing and event emission extracted from `lib.rs` (Phase 0.1 carve).
//!
//! Code motion only — behavior is byte-identical to the original definitions.

use std::collections::HashSet;

use anyhow::Result;
use browser_use_store::Store;
use serde_json::Value;

use crate::constants::*;

pub(crate) fn strip_memory_citations(text: &str) -> String {
    let mut visible = String::new();
    let mut rest = text;
    while let Some(open_idx) = rest.find(OAI_MEMORY_CITATION_OPEN_TAG) {
        visible.push_str(&rest[..open_idx]);
        let after_open = &rest[open_idx + OAI_MEMORY_CITATION_OPEN_TAG.len()..];
        let Some(close_idx) = after_open.find(OAI_MEMORY_CITATION_CLOSE_TAG) else {
            return visible;
        };
        rest = &after_open[close_idx + OAI_MEMORY_CITATION_CLOSE_TAG.len()..];
    }
    visible.push_str(rest);
    visible
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MemoryCitationEntry {
    path: String,
    line_start: usize,
    line_end: usize,
    note: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MemoryCitation {
    entries: Vec<MemoryCitationEntry>,
    rollout_ids: Vec<String>,
}

fn memory_citation_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(open_idx) = rest.find(OAI_MEMORY_CITATION_OPEN_TAG) {
        let after_open = &rest[open_idx + OAI_MEMORY_CITATION_OPEN_TAG.len()..];
        let Some(close_idx) = after_open.find(OAI_MEMORY_CITATION_CLOSE_TAG) else {
            break;
        };
        blocks.push(after_open[..close_idx].to_string());
        rest = &after_open[close_idx + OAI_MEMORY_CITATION_CLOSE_TAG.len()..];
    }
    blocks
}

fn parse_memory_citation_blocks(citations: Vec<String>) -> Option<MemoryCitation> {
    let mut entries = Vec::new();
    let mut rollout_ids = Vec::new();
    let mut seen_rollout_ids = HashSet::new();

    for citation in citations {
        if let Some(entries_block) =
            extract_memory_citation_block(&citation, "<citation_entries>", "</citation_entries>")
        {
            entries.extend(
                entries_block
                    .lines()
                    .filter_map(parse_memory_citation_entry),
            );
        }

        if let Some(ids_block) = extract_memory_citation_ids_block(&citation) {
            for id in ids_block
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
            {
                if seen_rollout_ids.insert(id.to_string()) {
                    rollout_ids.push(id.to_string());
                }
            }
        }
    }

    if entries.is_empty() && rollout_ids.is_empty() {
        None
    } else {
        Some(MemoryCitation {
            entries,
            rollout_ids,
        })
    }
}

fn parse_memory_citation_entry(line: &str) -> Option<MemoryCitationEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let (location, note) = line.rsplit_once("|note=[")?;
    let note = note.strip_suffix(']')?.trim().to_string();
    let (path, line_range) = location.rsplit_once(':')?;
    let (line_start, line_end) = line_range.split_once('-')?;

    Some(MemoryCitationEntry {
        path: path.trim().to_string(),
        line_start: line_start.trim().parse().ok()?,
        line_end: line_end.trim().parse().ok()?,
        note,
    })
}

fn extract_memory_citation_block<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let (_, rest) = text.split_once(open)?;
    let (body, _) = rest.split_once(close)?;
    Some(body)
}

fn extract_memory_citation_ids_block(text: &str) -> Option<&str> {
    extract_memory_citation_block(text, "<rollout_ids>", "</rollout_ids>")
        .or_else(|| extract_memory_citation_block(text, "<thread_ids>", "</thread_ids>"))
}

fn memory_citation_to_json(citation: &MemoryCitation) -> Value {
    serde_json::json!({
        "entries": citation
            .entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "path": entry.path.as_str(),
                    "line_start": entry.line_start,
                    "line_end": entry.line_end,
                    "note": entry.note.as_str(),
                })
            })
            .collect::<Vec<_>>(),
        "rollout_ids": &citation.rollout_ids,
    })
}

pub(crate) fn append_memory_citation_events(
    store: &Store,
    session_id: &str,
    source: &str,
    text: &str,
) -> Result<()> {
    let Some(citation) = parse_memory_citation_blocks(memory_citation_blocks(text)) else {
        return Ok(());
    };
    let mut payload = memory_citation_to_json(&citation);
    payload["source"] = Value::String(source.to_string());
    store.append_event(session_id, "memory.citation", payload)?;
    Ok(())
}
