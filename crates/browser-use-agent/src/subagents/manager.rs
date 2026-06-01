//! `SubagentManager` — registry + mailbox + a `ChildSpawner` seam.
//!
//! This is the orchestration layer. It does NOT run a real child agent (that
//! needs a `ModelClient` + the full turn loop, which is a later integration WP);
//! instead it spawns children through the [`ChildSpawner`] trait seam. Production
//! wires `ChildSpawner` to [`crate::task::TaskDriver::spawn_task`] with a child
//! [`crate::task::SessionTask`]; tests inject a fake that drives a canned child.
//!
//! Parity:
//! - depth limit + computation: `core/src/agent/registry.rs:71-77`.
//! - role application: `core/src/agent/role.rs:38-83`.
//! - EVENT-NOTIFY mailbox wait: `core/src/tools/handlers/multi_agents_v2/
//!   wait.rs:151-159` (parent `rx.changed().await`, then drain).
//! - child-completion fragment: `core/src/context/subagent_notification.rs`.
//! - registry / `<subagents>`: `core/src/agent/registry.rs` + legacy
//!   `environment_context_subagents_for_session`.
//!
//! BUDGET ACCOUNTING: codex tracks per-thread token usage and the parent reads
//! children's usage when assembling context (legacy `multi_agent_usage_hint`).
//! Here that is modeled directly: each child's reported output-token count is
//! aggregated onto the parent's [`SubagentManager::child_usage_total`]. This is a
//! deliberate, simplified addition (see the WP report's caveats), not a 1:1 copy
//! of a single codex call site.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::time::Duration;
use tokio::time::Instant;

use super::depth::DEFAULT_AGENT_MAX_DEPTH;
use super::mailbox::AgentStatus;
use super::mailbox::Mailbox;
use super::registry::AgentRecord;
use super::registry::AgentRegistry;
use super::role::AgentConfigLayer;
use super::role::RoleRegistry;
use super::spawn::check_spawn_depth;
use super::spawn::SpawnAgentArgs;

/// The minimal context a parent passes when spawning a child.
#[derive(Clone, Debug)]
pub struct ParentContext {
    /// The parent's canonical agent path (root = `/root`).
    pub agent_path: String,
    /// The parent's spawn depth (root = 0).
    pub depth: i32,
    /// The base config the role layer is applied on top of (carries the
    /// parent's sticky provider/service_tier choices).
    pub base_config: AgentConfigLayer,
}

/// The fully-resolved spec handed to a [`ChildSpawner`] to actually run a child.
#[derive(Clone, Debug)]
pub struct ChildSpec {
    pub agent_path: String,
    pub agent_id: String,
    pub nickname: Option<String>,
    pub role: Option<String>,
    pub depth: i32,
    /// The initial message/prompt for the child.
    pub message: String,
    /// The parent's requested history-fork mode.
    pub fork_turns: Option<String>,
    /// The child's resolved config (after the role layer was applied).
    pub config: AgentConfigLayer,
}

/// A handle to a spawned child returned by a [`ChildSpawner`].
#[derive(Clone, Debug)]
pub struct ChildHandle {
    pub agent_path: String,
    pub agent_id: String,
}

/// Error type for spawn/wait/message operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentError(pub String);

impl std::fmt::Display for SubagentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SubagentError {}

/// The seam between the manager and the real turn loop.
///
/// Production impl wires `spawn_child` to [`crate::task::TaskDriver::spawn_task`]
/// with a child [`crate::task::SessionTask`] that owns a `ModelClient` + turn
/// loop; on completion the child enqueues a
/// [`super::mailbox::SubagentNotification`] onto the shared [`Mailbox`] (the
/// parent's `wait` wakes via `rx.changed()`). Tests inject a fake that registers
/// a canned child and immediately enqueues a completion notification.
#[async_trait::async_trait]
pub trait ChildSpawner: Send + Sync {
    async fn spawn_child(&self, spec: ChildSpec) -> Result<ChildHandle, SubagentError>;
}

/// Ties the registry + mailbox + a [`ChildSpawner`] together.
pub struct SubagentManager {
    role_registry: RoleRegistry,
    registry: Arc<AgentRegistry>,
    mailbox: Arc<Mailbox>,
    spawner: Arc<dyn ChildSpawner>,
    max_depth: i32,
    /// Monotonic counter for minting unique agent ids/paths.
    next_id: AtomicU64,
    /// Aggregated output-token usage reported by children (budget accounting).
    child_usage_total: AtomicU64,
}

impl SubagentManager {
    /// Construct a manager with the default max depth
    /// (codex `DEFAULT_AGENT_MAX_DEPTH = 1`).
    pub fn new(spawner: Arc<dyn ChildSpawner>) -> Self {
        Self::with_config(spawner, RoleRegistry::new(), DEFAULT_AGENT_MAX_DEPTH)
    }

    /// Construct a manager with an explicit role registry + max depth.
    pub fn with_config(
        spawner: Arc<dyn ChildSpawner>,
        role_registry: RoleRegistry,
        max_depth: i32,
    ) -> Self {
        Self {
            role_registry,
            registry: Arc::new(AgentRegistry::new()),
            mailbox: Arc::new(Mailbox::new()),
            spawner,
            max_depth,
            next_id: AtomicU64::new(1),
            child_usage_total: AtomicU64::new(0),
        }
    }

    /// Shared mailbox (children enqueue onto it; parents subscribe).
    pub fn mailbox(&self) -> Arc<Mailbox> {
        Arc::clone(&self.mailbox)
    }

    /// Shared registry.
    pub fn registry(&self) -> Arc<AgentRegistry> {
        Arc::clone(&self.registry)
    }

    /// Aggregate output-token usage reported by children so far.
    pub fn child_usage_total(&self) -> u64 {
        self.child_usage_total.load(Ordering::Acquire)
    }

    /// Account `output_tokens` from a child onto the parent's running total.
    pub fn account_child_usage(&self, output_tokens: u64) {
        self.child_usage_total
            .fetch_add(output_tokens, Ordering::AcqRel);
    }

    /// Spawn a child: enforce depth, resolve+apply the role, mint a path/id,
    /// register it, and call the [`ChildSpawner`] seam.
    ///
    /// Returns the [`ChildHandle`]. Errors (depth exceeded, unknown role,
    /// invalid task_name, spawner failure) are surfaced as [`SubagentError`].
    pub async fn spawn(
        &self,
        args: SpawnAgentArgs,
        parent: &ParentContext,
    ) -> Result<ChildHandle, SubagentError> {
        args.validate_task_name().map_err(SubagentError)?;
        // Validate fork_turns up front (parity with codex rejecting bad values).
        args.fork_turns_mode().map_err(SubagentError)?;

        // 1. Depth check (codex registry.rs:71-77 + handler enforcement).
        let child_depth = check_spawn_depth(parent.depth, self.max_depth).map_err(SubagentError)?;

        // 2. Apply the role layer onto a copy of the parent's base config,
        //    preserving provider/tier unless the role overrides them.
        let mut config = parent.base_config.clone();
        if let Some(model) = &args.model {
            config.model = model.clone();
        }
        if let Some(reasoning) = &args.reasoning_effort {
            config.reasoning_effort = Some(reasoning.clone());
        }
        if let Some(service_tier) = &args.service_tier {
            config.service_tier = Some(service_tier.clone());
        }
        let role = self
            .role_registry
            .apply_role_to_config(&mut config, args.role_name())
            .map_err(SubagentError)?;

        // 3. Mint a unique path + id, draw a nickname from the role's pool.
        let seq = self.next_id.fetch_add(1, Ordering::AcqRel);
        let agent_path = format!("{}/{}_{}", parent.agent_path, args.task_name, seq);
        let agent_id = format!("{:04x}{:08x}", seq & 0xffff, rand::random::<u32>());
        let nickname = role
            .nickname_candidates
            .as_ref()
            .and_then(|names| self.registry.reserve_nickname(names));

        // 4. Register in the live registry as Running.
        self.registry.register(AgentRecord {
            agent_path: agent_path.clone(),
            agent_id: agent_id.clone(),
            nickname: nickname.clone(),
            role: config.role.clone(),
            status: AgentStatus::Running,
            depth: child_depth,
        });

        // 5. Hand off to the spawner seam.
        let spec = ChildSpec {
            agent_path: agent_path.clone(),
            agent_id: agent_id.clone(),
            nickname,
            role: config.role.clone(),
            depth: child_depth,
            message: args.message.clone(),
            fork_turns: args.fork_turns.clone(),
            config,
        };
        match self.spawner.spawn_child(spec).await {
            Ok(handle) => Ok(handle),
            Err(err) => {
                // Spawner failed after registration: mark the agent failed so
                // the registry stays truthful.
                self.registry
                    .update_status(&agent_path, AgentStatus::Failed);
                Err(err)
            }
        }
    }

    /// Wait (EVENT-NOTIFY) for any mailbox change up to `timeout`, then drain.
    ///
    /// The parent subscribes, blocks on `rx.changed()`, and only proceeds once a
    /// child has enqueued — NOT a poll loop (codex `wait.rs:151-159`). Returns
    /// the agent's status after draining: if a drained communication is from
    /// `agent_path`, the registry is consulted for the (possibly updated)
    /// status. `None` means the wait timed out without a change.
    pub async fn wait(&self, agent_path: &str, timeout: Duration) -> Option<AgentStatus> {
        let mut rx = self.mailbox.subscribe();
        let deadline = Instant::now() + timeout;
        let woken = Mailbox::wait_for_change(&mut rx, deadline).await;
        if !woken {
            return None;
        }
        // Drain so the queue does not re-fire endlessly; a real session feeds
        // these into the parent's next-turn input.
        let _drained = self.mailbox.drain();
        self.registry.get(agent_path).map(|record| record.status)
    }

    /// Send a message from a parent to a child (or peer) via the mailbox
    /// (codex `enqueue_mailbox_communication`).
    pub fn send_message(&self, communication: super::mailbox::InterAgentCommunication) {
        self.mailbox.enqueue(communication);
    }

    /// List the live agents (codex `live_agents`).
    pub fn list_agents(&self) -> Vec<AgentRecord> {
        self.registry.list_agents()
    }

    /// Close an agent: mark it `Closed` in the registry. Returns `true` if the
    /// agent existed.
    pub fn close_agent(&self, agent_path: &str) -> bool {
        self.registry.update_status(agent_path, AgentStatus::Closed)
    }
}
