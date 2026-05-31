//! Parent/child run linkage over the registry + mailbox.
//!
//! Ports the legacy `browser-use-core` `update_parent_from_child_run`
//! (`terminal-decodex/crates/browser-use-core/src/lib.rs:20767`, body in
//! `update_parent_from_child_run_with_hooks` :20776) onto the agent crate's
//! existing infra. The legacy version, on a child finishing, looked up the
//! child's `parent_session_id`, updated the child link's status/error on the
//! parent, and enqueued a `<subagent_notification>` into the parent's mailbox so
//! the parent wakes and learns the child is done.
//!
//! Here there is no `Store` and no `parent_session_id` column: the parent is
//! *derived from the child's canonical path* ([`tree::parent_path_of`]), the
//! child's outcome is recorded on its [`AgentRecord`] via
//! [`AgentRegistry::update_status`], and the parent is notified by enqueuing a
//! rendered [`SubagentNotification`] onto the shared [`Mailbox`] as an
//! [`InterAgentCommunication`] (the parent's `wait`/`drain` path). NO `Store`,
//! NO `browser-use-core` dependency.

use super::mailbox::{AgentStatus, InterAgentCommunication, Mailbox, SubagentNotification};
use super::registry::AgentRegistry;
use super::tree::parent_path_of;

/// Outcome of a finished child run, mirroring the legacy `run_error: Option<_>`
/// (`None` == success) plus an optional human-readable summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRunOutcome {
    /// Whether the child run succeeded (legacy: `run_error.is_none()`).
    pub success: bool,
    /// Optional result/error summary carried to the parent (legacy threads the
    /// error string / result into the notification + parent record).
    pub summary: Option<String>,
}

impl ChildRunOutcome {
    /// A successful run with an optional summary.
    pub fn success(summary: impl Into<Option<String>>) -> Self {
        Self {
            success: true,
            summary: summary.into(),
        }
    }

    /// A failed run; `error` becomes the carried summary.
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            success: false,
            summary: Some(error.into()),
        }
    }

    /// The [`AgentStatus`] this outcome records on the child
    /// (legacy maps `run_error` -> `Completed`/`Failed`).
    pub fn child_status(&self) -> AgentStatus {
        if self.success {
            AgentStatus::Completed
        } else {
            AgentStatus::Failed
        }
    }
}

/// Result of linking a finished child back to its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentLinkUpdate {
    /// Canonical path of the parent that was notified.
    pub parent_path: String,
    /// Canonical path of the child whose status was recorded.
    pub child_path: String,
    /// The status applied to the child (`Completed`/`Failed`).
    pub child_status: AgentStatus,
}

/// Record a finished child's outcome and notify its parent.
///
/// Ported from legacy `update_parent_from_child_run` (lib.rs:20767). Behavior
/// preserved on the new substrate:
///
/// 1. **Resolve the parent.** Derive the parent path from the child path
///    ([`parent_path_of`]). If the child has no parent (it is a root) the
///    function is a no-op and returns `None` — exactly as the legacy early-returns
///    when `parent_session_id` is `None`.
/// 2. **Verify the child is live.** If the child is not in the registry, no-op
///    `None` (legacy bails when the child link/session is absent).
/// 3. **Record the child outcome.** Set the child's [`AgentStatus`] to
///    `Completed`/`Failed` via [`AgentRegistry::update_status`] (legacy updates
///    the child link's status/error).
/// 4. **Notify the parent.** Enqueue a rendered [`SubagentNotification`] onto the
///    [`Mailbox`] as an [`InterAgentCommunication`] addressed parent <- child,
///    with `trigger_turn = true` so the parent wakes (legacy
///    `enqueue_mailbox_communication` of the `<subagent_notification>`).
///
/// Returns `Some(ParentLinkUpdate)` describing what was linked, or `None` when
/// there was nothing to link.
pub fn update_parent_from_child_run(
    registry: &AgentRegistry,
    mailbox: &Mailbox,
    child_path: &str,
    outcome: &ChildRunOutcome,
) -> Option<ParentLinkUpdate> {
    // 1. Resolve the parent (None for a root child).
    let parent_path = parent_path_of(child_path)?.to_string();

    // 2. The child must be a live agent.
    let _child = registry.get(child_path)?;

    // 3. Record the child outcome on the registry.
    let child_status = outcome.child_status();
    registry.update_status(child_path, child_status);

    // 4. Notify the parent via the mailbox.
    let notification = SubagentNotification::new(child_path.to_string(), child_status);
    let mut prompt = notification.render();
    if let Some(summary) = &outcome.summary {
        // Carry the summary alongside the rendered notification (legacy threads
        // the result/error text into the parent-facing record).
        prompt.push('\n');
        prompt.push_str(summary);
    }
    mailbox.enqueue(InterAgentCommunication::new(
        child_path.to_string(),
        parent_path.clone(),
        Vec::new(),
        prompt,
        true,
    ));

    Some(ParentLinkUpdate {
        parent_path,
        child_path: child_path.to_string(),
        child_status,
    })
}

#[cfg(test)]
mod parent_link_unit_tests {
    use super::*;
    use crate::subagents::registry::AgentRecord;

    fn rec(path: &str) -> AgentRecord {
        AgentRecord {
            agent_path: path.to_string(),
            agent_id: format!("id{path}"),
            nickname: None,
            role: None,
            status: AgentStatus::Running,
            depth: path.matches('/').count() as i32 - 1,
        }
    }

    fn registry() -> AgentRegistry {
        let r = AgentRegistry::new();
        r.register(rec("/root"));
        r.register(rec("/root/child"));
        r
    }

    #[test]
    fn child_success_records_completed_and_notifies_parent() {
        let r = registry();
        let mailbox = Mailbox::new();
        let update = update_parent_from_child_run(
            &r,
            &mailbox,
            "/root/child",
            &ChildRunOutcome::success(Some("all done".to_string())),
        )
        .expect("child has a parent");

        assert_eq!(update.parent_path, "/root");
        assert_eq!(update.child_path, "/root/child");
        assert_eq!(update.child_status, AgentStatus::Completed);

        // Child status recorded on the registry.
        assert_eq!(r.get("/root/child").unwrap().status, AgentStatus::Completed);

        // Parent inbox got exactly one communication, addressed parent <- child.
        let drained = mailbox.drain();
        assert_eq!(drained.len(), 1);
        let msg = &drained[0];
        assert_eq!(msg.from_agent_path, "/root/child");
        assert_eq!(msg.to_agent_path, "/root");
        assert!(msg.trigger_turn);
        assert!(msg.prompt.contains("<subagent_notification>"));
        assert!(msg.prompt.contains("completed"));
        assert!(msg.prompt.contains("all done"));
    }

    #[test]
    fn child_failure_records_failed() {
        let r = registry();
        let mailbox = Mailbox::new();
        let update = update_parent_from_child_run(
            &r,
            &mailbox,
            "/root/child",
            &ChildRunOutcome::failure("boom"),
        )
        .expect("child has a parent");

        assert_eq!(update.child_status, AgentStatus::Failed);
        assert_eq!(r.get("/root/child").unwrap().status, AgentStatus::Failed);
        let drained = mailbox.drain();
        assert_eq!(drained.len(), 1);
        assert!(drained[0].prompt.contains("failed"));
        assert!(drained[0].prompt.contains("boom"));
    }

    #[test]
    fn root_child_has_no_parent_is_noop() {
        let r = registry();
        let mailbox = Mailbox::new();
        let update =
            update_parent_from_child_run(&r, &mailbox, "/root", &ChildRunOutcome::success(None));
        assert!(update.is_none());
        assert!(mailbox.drain().is_empty());
        // Root status untouched.
        assert_eq!(r.get("/root").unwrap().status, AgentStatus::Running);
    }

    #[test]
    fn unknown_child_is_noop() {
        let r = registry();
        let mailbox = Mailbox::new();
        let update = update_parent_from_child_run(
            &r,
            &mailbox,
            "/root/ghost",
            &ChildRunOutcome::success(None),
        );
        assert!(update.is_none());
        assert!(mailbox.drain().is_empty());
    }
}
