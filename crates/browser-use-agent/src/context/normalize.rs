//! Call/output pairing + image stripping (re-exports from assembly).
//!
//! Codex keeps the normalization helpers in its history/normalize modules; here
//! their bodies live in `assembly` (to avoid a circular split between
//! estimation and normalization). This module is the stable
//! `context::normalize::*` surface.

pub use super::assembly::{
    ensure_call_outputs_present, for_prompt, remove_corresponding_for, remove_orphan_outputs,
    strip_images_when_unsupported,
};
