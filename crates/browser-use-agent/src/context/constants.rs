//! Byte/token ratios and context-message name strings.
//!
//! Parity:
//!   * `APPROX_BYTES_PER_TOKEN = 4` mirrors codex
//!     `approx_tokens_from_byte_count_i64 = bytes.div_ceil(4)` and legacy
//!     `browser-use-core::APPROX_CHARS_PER_TOKEN = 4`.
//!   * `RESIZED_IMAGE_BYTES_ESTIMATE = 7_373` mirrors codex
//!     `RESIZED_IMAGE_BYTES_ESTIMATE` / legacy
//!     `RESIZED_IMAGE_CONTEXT_BYTES_ESTIMATE`.
//!   * `ORIGINAL_IMAGE_PATCH_SIZE = 32`, `ORIGINAL_IMAGE_MAX_PATCHES = 10_000`
//!     mirror codex/legacy original-detail patch budgeting.
//!   * the `*_CONTEXT_MESSAGE_NAME` family copies the exact string values from
//!     legacy `browser-use-core/src/constants.rs`.

/// Codex 4-bytes-per-token history accounting heuristic.
pub const APPROX_BYTES_PER_TOKEN: i64 = 4;

/// Estimated model-visible byte cost of a resized screenshot image.
pub const RESIZED_IMAGE_BYTES_ESTIMATE: i64 = 7_373;

/// Patch edge size (pixels) for original-detail image token estimation.
pub const ORIGINAL_IMAGE_PATCH_SIZE: usize = 32;

/// Maximum number of patches counted for an original-detail image.
pub const ORIGINAL_IMAGE_MAX_PATCHES: usize = 10_000;

// ---------------------------------------------------------------------------
// *_CONTEXT_MESSAGE_NAME family.
//
// Exact string values copied from legacy
// `browser-use-core/src/constants.rs`. WP-A4 (`inject`) owns their *usage*;
// WP-A3 owns their *definition* so the accounting/normalization surface and the
// injection surface share one source of truth.
// ---------------------------------------------------------------------------

/// Name tag for the injected workspace-context message.
pub const WORKSPACE_CONTEXT_MESSAGE_NAME: &str = "workspace_context";

/// Name tag for the injected permissions-context message.
pub const PERMISSIONS_CONTEXT_MESSAGE_NAME: &str = "permissions_context";

/// Name tag for the injected multi-agent usage-hint context message.
pub const MULTI_AGENT_USAGE_HINT_CONTEXT_MESSAGE_NAME: &str = "multi_agent_usage_hint";

/// Name tag for the injected model-switch context message.
pub const MODEL_SWITCH_CONTEXT_MESSAGE_NAME: &str = "model_switch_context";

/// Name tag for the injected personality-context message.
pub const PERSONALITY_CONTEXT_MESSAGE_NAME: &str = "personality_context";

/// Name tag for the injected goal-context message.
pub const GOAL_CONTEXT_MESSAGE_NAME: &str = "goal_context";

/// Name tag for the injected hook-context message.
pub const HOOK_CONTEXT_MESSAGE_NAME: &str = "hook_context";

/// Name tag for the injected collaboration-context message.
pub const COLLABORATION_CONTEXT_MESSAGE_NAME: &str = "collaboration_context";

/// Name tag for the injected typed-mention context message.
pub const MENTION_CONTEXT_MESSAGE_NAME: &str = "typed_mention_context";

/// Name tag for the injected generated-image context message.
pub const GENERATED_IMAGE_CONTEXT_MESSAGE_NAME: &str = "generated_image_context";
