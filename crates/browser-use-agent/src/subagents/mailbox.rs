//! EVENT-NOTIFY mailbox (codex `session/input_queue.rs` parity).
//!
//! The mechanism a parent uses to be *woken* when a child has news — NOT a poll
//! loop. A child pushes an [`InterAgentCommunication`] and bumps a
//! `tokio::sync::watch` channel; any parent subscribed via [`Mailbox::subscribe`]
//! is woken by `rx.changed().await` and then drains the queue FIFO.
//!
//! Parity:
//! - `core/src/session/input_queue.rs:25-30` `InputQueue { mailbox_tx:
//!   watch::Sender<()>, mailbox_pending_mails: Mutex<VecDeque<…>> }`.
//! - `:42-48` `subscribe_mailbox()` subscribes and `mark_changed()` if there are
//!   already pending mails (so a late subscriber still wakes immediately).
//! - `:50-59` `enqueue_mailbox_communication` pushes then `mailbox_tx
//!   .send_replace(())` to WAKE subscribers.
//! - `:73-80` `drain_mailbox_input_items` drains FIFO.
//! - Parent wait: `core/src/tools/handlers/multi_agents_v2/wait.rs:151-159`
//!   `wait_for_mailbox_change(rx, deadline)` = `timeout_at(deadline,
//!   rx.changed())`.
//! - Child-completion fragment: `core/src/context/subagent_notification.rs:6-42`
//!   `SubagentNotification { agent_reference, status }` rendered inside
//!   `<subagent_notification>…</subagent_notification>`.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::watch;
use tokio::time::timeout_at;
use tokio::time::Instant;

/// A message handed between agents through a [`Mailbox`]
/// (codex `InterAgentCommunication`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterAgentCommunication {
    /// Canonical path of the sending agent (e.g. `/root/worker`).
    pub from_agent_path: String,
    /// Canonical path of the receiving agent.
    pub to_agent_path: String,
    /// Opaque carried items (e.g. serialized response items); free-form here.
    pub items: Vec<String>,
    /// Structured legacy v1 user-input items, preserved for no-store fallback
    /// paths that do not have durable `agent_messages.input_items`.
    pub input_items: Option<Value>,
    /// The human-readable prompt/body of the communication.
    pub prompt: String,
    /// Whether delivery should wake the recipient into a fresh turn
    /// (codex `trigger_turn`).
    pub trigger_turn: bool,
}

impl InterAgentCommunication {
    pub fn new(
        from_agent_path: impl Into<String>,
        to_agent_path: impl Into<String>,
        items: Vec<String>,
        prompt: impl Into<String>,
        trigger_turn: bool,
    ) -> Self {
        Self {
            from_agent_path: from_agent_path.into(),
            to_agent_path: to_agent_path.into(),
            items,
            input_items: None,
            prompt: prompt.into(),
            trigger_turn,
        }
    }

    pub fn new_with_input_items(
        from_agent_path: impl Into<String>,
        to_agent_path: impl Into<String>,
        input_items: Option<Value>,
        prompt: impl Into<String>,
        trigger_turn: bool,
    ) -> Self {
        Self {
            from_agent_path: from_agent_path.into(),
            to_agent_path: to_agent_path.into(),
            items: Vec::new(),
            input_items,
            prompt: prompt.into(),
            trigger_turn,
        }
    }
}

/// Status of a sub-agent (codex `AgentStatus` wire surface).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    PendingInit,
    Running,
    Interrupted,
    Completed(Option<String>),
    Errored(String),
    Shutdown,
    NotFound,
}

impl AgentStatus {
    /// Short label used in the in-model `<subagents>` environment block.
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentStatus::PendingInit => "pending_init",
            AgentStatus::Running => "running",
            AgentStatus::Interrupted => "interrupted",
            AgentStatus::Completed(_) => "completed",
            AgentStatus::Errored(_) => "errored",
            AgentStatus::Shutdown => "shutdown",
            AgentStatus::NotFound => "not_found",
        }
    }

    pub fn is_final(&self) -> bool {
        matches!(
            self,
            AgentStatus::Completed(_)
                | AgentStatus::Errored(_)
                | AgentStatus::Shutdown
                | AgentStatus::NotFound
        )
    }

    pub fn is_live(&self) -> bool {
        !matches!(self, AgentStatus::Shutdown | AgentStatus::NotFound)
    }

    pub fn wire_value(&self) -> serde_json::Value {
        serde_json::to_value(self)
            .unwrap_or_else(|_| serde_json::Value::String(self.as_str().into()))
    }
}

/// A child-completion notification injected into the parent as a contextual user
/// fragment (codex `context/subagent_notification.rs:6-42`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentNotification {
    /// Reference to the agent (codex `agent_reference`; its `agent_path`).
    pub agent_reference: String,
    pub status: AgentStatus,
}

impl SubagentNotification {
    pub fn new(agent_reference: impl Into<String>, status: AgentStatus) -> Self {
        Self {
            agent_reference: agent_reference.into(),
            status,
        }
    }

    /// Opening/closing markers (codex `type_markers` :29-31).
    pub fn markers() -> (&'static str, &'static str) {
        ("<subagent_notification>", "</subagent_notification>")
    }

    /// Render the fragment wrapped in `<subagent_notification>…</…>` with a JSON
    /// `{agent_path, status}` body (codex `body` :33-41).
    pub fn render(&self) -> String {
        let (open, close) = Self::markers();
        let body = serde_json::json!({
            "agent_path": &self.agent_reference,
            "status": self.status.wire_value(),
        });
        format!("{open}\n{body}\n{close}")
    }
}

/// EVENT-NOTIFY mailbox: a `watch` wake channel + a FIFO pending queue
/// (codex `InputQueue` mailbox half).
pub struct Mailbox {
    /// Wake channel. Every `enqueue` does `send_replace(())`; subscribers learn
    /// "something changed" without the payload travelling through the channel.
    tx: watch::Sender<()>,
    /// FIFO of undelivered communications.
    pending: Mutex<VecDeque<InterAgentCommunication>>,
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Mailbox {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(());
        Self {
            tx,
            pending: Mutex::new(VecDeque::new()),
        }
    }

    /// Subscribe for wake notifications (codex `subscribe_mailbox` :42-48).
    ///
    /// If there are already pending mails, the receiver is `mark_changed()` so a
    /// late subscriber wakes on the very first `changed().await` rather than
    /// missing the bump that happened before it subscribed.
    pub fn subscribe(&self) -> watch::Receiver<()> {
        let mut rx = self.tx.subscribe();
        if self.has_pending() {
            rx.mark_changed();
        }
        rx
    }

    /// Push a communication then WAKE all subscribers
    /// (codex `enqueue_mailbox_communication` :50-59).
    pub fn enqueue(&self, communication: InterAgentCommunication) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(communication);
        // `send_replace` always notifies, even if no value changed.
        self.tx.send_replace(());
    }

    /// Drain all pending communications in FIFO order
    /// (codex `drain_mailbox_input_items` :73-80).
    pub fn drain(&self) -> Vec<InterAgentCommunication> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    /// Whether any communications are queued.
    pub fn has_pending(&self) -> bool {
        !self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Whether any pending communication requests a fresh turn
    /// (codex `has_trigger_turn_mailbox_items` :65-71).
    pub fn has_trigger_turn_pending(&self) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|c| c.trigger_turn)
    }

    /// Block until the mailbox is bumped or `deadline` elapses
    /// (codex `wait_for_mailbox_change` `wait.rs:151-159`).
    ///
    /// Returns `true` if woken by a bump, `false` on timeout or a closed sender.
    /// This is the EVENT-NOTIFY wait: there is NO poll loop — the future is
    /// pending until `changed()` resolves.
    pub async fn wait_for_change(rx: &mut watch::Receiver<()>, deadline: Instant) -> bool {
        matches!(timeout_at(deadline, rx.changed()).await, Ok(Ok(())))
    }
}
