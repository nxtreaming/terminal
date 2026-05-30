//! Agent-wide error type (frozen scaffold, WP-A0).
//!
//! Mirrors codex `turn.rs:357` error semantics: `TurnAborted` is reported via a
//! `TurnAborted` event rather than as a hard error in the common path.

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("turn aborted")]
    TurnAborted,
    #[error("context window exceeded")]
    ContextWindowExceeded,
    #[error("usage limit reached")]
    UsageLimitReached,
    #[error("invalid image request")]
    InvalidImageRequest,
    #[error("provider error: {0}")]
    Provider(String),
    #[error("tool error: {0}")]
    Tool(String),
    #[error(transparent)]
    Store(anyhow::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl AgentError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Provider(_))
    }
}
