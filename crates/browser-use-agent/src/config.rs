//! Pure data snapshot of agent configuration shared across subsystems (WP-A0).

use crate::tools::AskForApproval;

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub model: String,
    pub provider: String,
    pub approval_policy: AskForApproval,
    pub stream_max_retries: u32,
    /// `history.rs:303-332` selects the token-counting branch on this flag.
    pub server_reasoning_included: bool,
    /// `turn.rs:872` — gates parallel tool dispatch.
    pub supports_parallel_tool_calls: bool,
    /// normalize `strip_images` gate.
    pub supports_image_input: bool,
    pub auto_compact_scope_limit: i64,
    pub model_context_window: Option<i64>,
    /// `tasks/mod.rs:74` — selects the interrupted-turn history marker.
    pub agent_interrupt_message_enabled: bool,
    pub multi_agent_v2: bool,
}
