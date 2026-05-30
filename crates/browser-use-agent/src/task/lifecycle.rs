//! Task lifecycle value types (codex `tasks/mod.rs` parity).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnAbortReason {
    Interrupted,
    Replaced,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptedTurnHistoryMarker {
    Disabled,
    ContextualUser,
    Developer,
}

impl InterruptedTurnHistoryMarker {
    /// `tasks/mod.rs:74`.
    pub fn from_config(_interrupt_enabled: bool, _multi_agent_v2: bool) -> Self {
        unimplemented!()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    Regular,
    Compact,
    Review,
}

#[derive(Debug, Clone)]
pub enum TurnLifecycleEvent {
    TurnStarted {
        turn_id: String,
    },
    TurnComplete {
        turn_id: String,
        last_agent_message: Option<String>,
    },
    TurnAborted {
        turn_id: String,
        reason: TurnAbortReason,
    },
}
