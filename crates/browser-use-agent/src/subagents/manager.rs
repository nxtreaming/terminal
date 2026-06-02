//! `SubagentManager` — registry + mailbox + a `ChildSpawner` seam.
//!
//! This is the orchestration layer. It spawns children through the
//! [`ChildSpawner`] trait. Production wires that trait to the CLI/TUI child-runner
//! path, which creates a child session and drives the normal model turn loop;
//! tests inject a fake that drives a canned child.
//!
//! Parity:
//! - depth computation: `core/src/agent/registry.rs:71-77`. MultiAgentV2 does
//!   not disable nested spawning at `agent_max_depth`; the depth is still
//!   recorded for metadata.
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

use std::collections::BTreeSet;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use browser_use_providers::model_request_info_for_catalog;
use tokio::time::Duration;
use tokio::time::Instant;

use super::depth::{next_spawn_depth, DEFAULT_AGENT_MAX_DEPTH};
use super::mailbox::AgentStatus;
use super::mailbox::Mailbox;
use super::registry::AgentRecord;
use super::registry::AgentRegistry;
use super::role::{
    default_agent_nickname_candidates, AgentConfigLayer, AgentRoleConfig, RoleRegistry,
    DEFAULT_ROLE_NAME,
};
use super::spawn::{ForkTurns, SpawnAgentArgs};
use super::tree::resolve_agent_path_v2;
use super::tree::resolve_agent_reference_in_tree_v2;
use super::tree::ROOT_AGENT_PATH;

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
    pub run_id: String,
    pub nickname: Option<String>,
    pub role: Option<String>,
    pub depth: i32,
    /// The initial message/prompt for the child.
    pub message: String,
    /// Structured initial v1 input items when the child should start from a
    /// direct user-input payload rather than flattened text.
    pub input_items: Option<serde_json::Value>,
    /// Whether the initial v2 text prompt should be delivered as Codex's
    /// inter-agent communication envelope.
    pub input_is_inter_agent_communication: bool,
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
    max_concurrent_threads_per_session: Option<usize>,
    /// Monotonic counter for minting unique agent ids/paths.
    next_id: AtomicU64,
    /// Aggregated output-token usage reported by children (budget accounting).
    child_usage_total: AtomicU64,
    /// Serializes hidden spawn reservations so concurrent spawn calls cannot
    /// oversubscribe the session or race on an agent path before the child
    /// thread is successfully created.
    spawn_reservations: Mutex<SpawnReservations>,
}

#[derive(Default)]
struct SpawnReservations {
    paths: BTreeSet<String>,
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
        Self::with_config_and_limits(spawner, role_registry, max_depth, None)
    }

    pub fn with_config_and_limits(
        spawner: Arc<dyn ChildSpawner>,
        role_registry: RoleRegistry,
        max_depth: i32,
        max_concurrent_threads_per_session: Option<usize>,
    ) -> Self {
        Self {
            role_registry,
            registry: Arc::new(AgentRegistry::new()),
            mailbox: Arc::new(Mailbox::new()),
            spawner,
            max_depth,
            max_concurrent_threads_per_session,
            next_id: AtomicU64::new(1),
            child_usage_total: AtomicU64::new(0),
            spawn_reservations: Mutex::new(SpawnReservations::default()),
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

    pub fn max_depth(&self) -> i32 {
        self.max_depth
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

    /// Spawn a child: compute depth, resolve+apply v2-allowed overrides, mint a
    /// path/id, register it, and call the [`ChildSpawner`] seam.
    ///
    /// Returns the [`ChildHandle`]. Errors (depth exceeded, unknown role,
    /// invalid task_name, spawner failure) are surfaced as [`SubagentError`].
    pub async fn spawn(
        &self,
        args: SpawnAgentArgs,
        parent: &ParentContext,
    ) -> Result<ChildHandle, SubagentError> {
        let spec = {
            let mut reservations = self
                .spawn_reservations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            args.validate_task_name().map_err(SubagentError)?;
            args.validate_overrides().map_err(SubagentError)?;
            // Validate fork_turns up front (parity with codex rejecting bad values).
            let fork_turns_mode = args.fork_turns_mode().map_err(SubagentError)?;
            reject_full_history_spawn_overrides(&args, fork_turns_mode).map_err(SubagentError)?;

            // 1. Depth metadata. Codex v2 keeps subagent tools available even at
            // `agent_max_depth`; it only disables older/v1 collaboration tools there.
            let _configured_max_depth = self.max_depth;
            let child_depth = next_spawn_depth(parent.depth);
            self.ensure_parent_record(parent);
            self.check_concurrent_thread_limit(reservations.paths.len())?;

            // 2. Apply requested model/reasoning overrides and the role layer
            //    onto a copy of the parent's live config. Codex validates
            //    model/reasoning before role layering, then resolves the final
            //    service tier after the role may have replaced the model/tier.
            let mut config = parent.base_config.clone();
            let parent_service_tier = config.service_tier.clone();
            if let Some(service_tier) = &args.service_tier {
                config.service_tier = Some(service_tier.clone());
            }
            let role = if matches!(fork_turns_mode, ForkTurns::All) {
                config
                    .role
                    .as_deref()
                    .and_then(|name| self.role_registry.resolve(name))
                    .or_else(|| self.role_registry.resolve(DEFAULT_ROLE_NAME))
                    .unwrap_or_else(AgentRoleConfig::default)
            } else {
                apply_requested_spawn_agent_model_overrides(
                    &mut config,
                    args.model.as_deref(),
                    args.reasoning_effort.as_deref(),
                )
                .map_err(SubagentError)?;
                self.role_registry
                    .apply_role_to_config(&mut config, args.role_name())
                    .map_err(SubagentError)?
            };
            apply_model_catalog_override(&mut config);
            apply_spawn_agent_service_tier(
                &mut config,
                parent_service_tier.as_deref(),
                args.service_tier.as_deref(),
            )
            .map_err(SubagentError)?;

            // 3. Mint a unique path + id, draw a nickname from the role's pool.
            let _seq = self.next_id.fetch_add(1, Ordering::AcqRel);
            let agent_path = child_agent_path(&parent.agent_path, &args.task_name);
            if self.registry.contains_path(&agent_path) || reservations.paths.contains(&agent_path)
            {
                return Err(SubagentError(format!(
                    "agent path `{agent_path}` already exists"
                )));
            }
            let agent_id = browser_use_store::new_thread_id();
            let run_id = browser_use_store::new_thread_id();
            let nickname_candidates = role
                .nickname_candidates
                .clone()
                .unwrap_or_else(default_agent_nickname_candidates);
            let nickname = self.registry.reserve_nickname(&nickname_candidates);
            reservations.paths.insert(agent_path.clone());

            let spec = ChildSpec {
                agent_path,
                agent_id,
                run_id,
                nickname: nickname.clone(),
                role: config.role.clone(),
                depth: child_depth,
                message: args.message.clone(),
                input_items: args.input_items.clone(),
                input_is_inter_agent_communication: args.input_is_inter_agent_communication,
                fork_turns: args.fork_turns.clone(),
                config,
            };
            self.registry.reserve_path(&spec.agent_path);
            spec
        };
        let agent_path = spec.agent_path.clone();
        let record = AgentRecord {
            agent_path: spec.agent_path.clone(),
            agent_id: spec.agent_id.clone(),
            nickname: spec.nickname.clone(),
            role: spec.role.clone(),
            status: AgentStatus::Running,
            depth: spec.depth,
            last_task_message: Some(spec.message.clone()),
        };
        match self.spawner.spawn_child(spec).await {
            Ok(handle) => {
                let mut reservations = self
                    .spawn_reservations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                self.registry.register(record);
                reservations.paths.remove(&agent_path);
                Ok(handle)
            }
            Err(err) => {
                let mut reservations = self
                    .spawn_reservations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                self.registry.release_reserved_path(&agent_path);
                reservations.paths.remove(&agent_path);
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

    /// Wait for any mailbox update and drain pending messages. This is the
    /// Codex v2 shape: the wait reports whether the mailbox changed, while the
    /// actual content is delivered through the normal mailbox/context path.
    pub async fn wait_any(&self, timeout: Duration) -> bool {
        let mut rx = self.mailbox.subscribe();
        let deadline = Instant::now() + timeout;
        Mailbox::wait_for_change(&mut rx, deadline).await
    }

    /// Send a message from a parent to a child (or peer) via the mailbox
    /// (codex `enqueue_mailbox_communication`).
    pub fn send_message(&self, communication: super::mailbox::InterAgentCommunication) {
        self.mailbox.enqueue(communication);
    }

    /// Resolve a target reference relative to `parent` and enqueue an
    /// inter-agent message. When `trigger_turn` is true, root is rejected to
    /// match Codex's `followup_task` rule.
    pub fn send_message_to_agent(
        &self,
        parent: &ParentContext,
        target: &str,
        message: &str,
        trigger_turn: bool,
    ) -> Result<AgentRecord, SubagentError> {
        if message.trim().is_empty() {
            return Err(SubagentError(
                "Empty message can't be sent to an agent".to_string(),
            ));
        }
        self.ensure_parent_record(parent);
        let target = resolve_agent_reference_in_tree_v2(&self.registry, &parent.agent_path, target)
            .map_err(SubagentError)?
            .ok_or_else(|| SubagentError(format!("live agent path `{target}` not found")))?;
        if trigger_turn && target.agent_path == ROOT_AGENT_PATH {
            return Err(SubagentError(
                "Tasks can't be assigned to the root agent".to_string(),
            ));
        }
        self.registry
            .update_last_task_message(&target.agent_path, message.to_string());
        self.mailbox
            .enqueue(super::mailbox::InterAgentCommunication::new(
                parent.agent_path.clone(),
                target.agent_path.clone(),
                Vec::new(),
                message.to_string(),
                trigger_turn,
            ));
        Ok(target)
    }

    /// Legacy collaboration tools target agents by thread id, not by v2 task
    /// path/reference syntax.
    pub fn send_message_to_agent_id(
        &self,
        parent: &ParentContext,
        target_id: &str,
        message: &str,
        trigger_turn: bool,
    ) -> Result<AgentRecord, SubagentError> {
        self.send_message_to_agent_id_with_items(parent, target_id, message, None, trigger_turn)
    }

    pub fn send_message_to_agent_id_with_items(
        &self,
        parent: &ParentContext,
        target_id: &str,
        message: &str,
        input_items: Option<serde_json::Value>,
        trigger_turn: bool,
    ) -> Result<AgentRecord, SubagentError> {
        if message.trim().is_empty() {
            return Err(SubagentError(
                "Empty message can't be sent to an agent".to_string(),
            ));
        }
        self.ensure_parent_record(parent);
        let target = self
            .registry
            .list_agents()
            .into_iter()
            .find(|record| record.agent_id == target_id)
            .ok_or_else(|| SubagentError(format!("agent with id {target_id} not found")))?;
        if trigger_turn && target.agent_path == ROOT_AGENT_PATH {
            return Err(SubagentError(
                "Tasks can't be assigned to the root agent".to_string(),
            ));
        }
        self.registry
            .update_last_task_message(&target.agent_path, message.to_string());
        self.mailbox.enqueue(
            super::mailbox::InterAgentCommunication::new_with_input_items(
                parent.agent_path.clone(),
                target.agent_path.clone(),
                input_items,
                message.to_string(),
                trigger_turn,
            ),
        );
        Ok(target)
    }

    /// Legacy no-store managers do not own the child task driver or cancellation
    /// token, so they cannot provide Codex-style interrupt semantics. Production
    /// store-backed children are cancelled by the store path.
    pub fn interrupt_agent_id(&self, target_id: &str) -> Result<AgentRecord, SubagentError> {
        let target = self
            .registry
            .list_agents()
            .into_iter()
            .find(|record| record.agent_id == target_id)
            .ok_or_else(|| SubagentError(format!("agent with id {target_id} not found")))?;
        Err(SubagentError(format!(
            "interrupt is only supported for store-backed agents; `{}` has no cancellable runtime handle",
            target.agent_path
        )))
    }

    /// List the live agents (codex `live_agents`).
    pub fn list_agents(&self) -> Vec<AgentRecord> {
        self.registry.list_agents()
    }

    /// List agents, optionally filtered by a path prefix relative to `parent`.
    pub fn list_agents_filtered(
        &self,
        parent: &ParentContext,
        path_prefix: Option<&str>,
    ) -> Result<Vec<AgentRecord>, SubagentError> {
        self.ensure_parent_record(parent);
        let prefix = path_prefix
            .map(|prefix| resolve_agent_path_v2(&parent.agent_path, prefix))
            .transpose()
            .map_err(SubagentError)?;
        Ok(self
            .registry
            .list_agents()
            .into_iter()
            .filter(|record| record.status.is_live())
            .filter(|record| {
                if let Some(prefix) = prefix.as_deref() {
                    record.agent_path == prefix
                        || record
                            .agent_path
                            .strip_prefix(prefix)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                } else {
                    true
                }
            })
            .collect())
    }

    /// Close an agent: mark it `Closed` in the registry. Returns `true` if the
    /// agent existed.
    pub fn close_agent(&self, agent_path: &str) -> bool {
        self.registry
            .update_subtree_status(agent_path, AgentStatus::Shutdown)
            .is_some()
    }

    /// Close an agent reference relative to `parent`, returning the target's
    /// previous status. Root cannot be closed.
    pub fn close_agent_reference(
        &self,
        parent: &ParentContext,
        target: &str,
    ) -> Result<AgentStatus, SubagentError> {
        self.ensure_parent_record(parent);
        let record = resolve_agent_reference_in_tree_v2(&self.registry, &parent.agent_path, target)
            .map_err(SubagentError)?
            .ok_or_else(|| SubagentError(format!("live agent path `{target}` not found")))?;
        if record.agent_path == ROOT_AGENT_PATH {
            return Err(SubagentError("root is not a spawned agent".to_string()));
        }
        self.registry
            .update_subtree_status(&record.agent_path, AgentStatus::Shutdown)
            .ok_or_else(|| SubagentError(format!("live agent path `{target}` not found")))
    }

    /// Legacy collaboration close targets agents by thread id.
    pub fn close_agent_id(&self, target_id: &str) -> Result<AgentStatus, SubagentError> {
        let record = self
            .registry
            .list_agents()
            .into_iter()
            .find(|record| record.agent_id == target_id)
            .ok_or_else(|| SubagentError(format!("agent with id {target_id} not found")))?;
        if record.agent_path == ROOT_AGENT_PATH {
            return Err(SubagentError("root is not a spawned agent".to_string()));
        }
        self.registry
            .update_subtree_status(&record.agent_path, AgentStatus::Shutdown)
            .ok_or_else(|| SubagentError(format!("agent with id {target_id} not found")))
    }

    fn ensure_parent_record(&self, parent: &ParentContext) {
        if self.registry.contains_path(&parent.agent_path) {
            return;
        }
        self.registry.register(AgentRecord {
            agent_path: parent.agent_path.clone(),
            agent_id: parent.agent_path.clone(),
            nickname: None,
            role: Some("default".to_string()),
            status: AgentStatus::Running,
            depth: parent.depth,
            last_task_message: if parent.agent_path == ROOT_AGENT_PATH {
                Some("Main thread".to_string())
            } else {
                None
            },
        });
    }

    fn check_concurrent_thread_limit(&self, reserved: usize) -> Result<(), SubagentError> {
        let Some(limit) = self.max_concurrent_threads_per_session else {
            return Ok(());
        };
        let live_threads = self
            .registry
            .list_agents()
            .into_iter()
            .filter(|record| record.agent_path != ROOT_AGENT_PATH && record.status.is_live())
            .count()
            + reserved;
        if live_threads >= limit {
            return Err(SubagentError(format!(
                "max_concurrent_threads_per_session limit reached ({limit})"
            )));
        }
        Ok(())
    }
}

fn reject_full_history_spawn_overrides(
    args: &SpawnAgentArgs,
    fork_turns_mode: ForkTurns,
) -> Result<(), String> {
    if matches!(fork_turns_mode, ForkTurns::All)
        && (args.role_name().is_some() || args.model.is_some() || args.reasoning_effort.is_some())
    {
        return Err(
            "Full-history forked agents inherit the parent agent type, model, and reasoning effort; omit agent_type, model, and reasoning_effort, or spawn without a full-history fork."
                .to_string(),
        );
    }
    Ok(())
}

fn apply_requested_spawn_agent_model_overrides(
    config: &mut AgentConfigLayer,
    requested_model: Option<&str>,
    requested_reasoning_effort: Option<&str>,
) -> Result<(), String> {
    if requested_model.is_none() && requested_reasoning_effort.is_none() {
        return Ok(());
    }

    if let Some(requested_model) = requested_model {
        let selected_model_name = find_spawn_agent_model_name(config, requested_model)?;
        let selected_model_info =
            model_request_info_for_catalog(&selected_model_name, config.model_catalog.as_ref());
        config.model = selected_model_name.clone();
        if let Some(reasoning_effort) = requested_reasoning_effort {
            let reasoning_effort = normalize_reasoning_effort(reasoning_effort);
            validate_spawn_agent_reasoning_effort(
                &selected_model_name,
                &selected_model_info.supported_reasoning_efforts,
                &reasoning_effort,
            )?;
            config.reasoning_effort = Some(reasoning_effort);
        } else {
            config.reasoning_effort = selected_model_info.default_reasoning_effort;
        }
        return Ok(());
    }

    if let Some(reasoning_effort) = requested_reasoning_effort {
        let reasoning_effort = normalize_reasoning_effort(reasoning_effort);
        let model_info =
            model_request_info_for_catalog(&config.model, config.model_catalog.as_ref());
        validate_spawn_agent_reasoning_effort(
            &config.model,
            &model_info.supported_reasoning_efforts,
            &reasoning_effort,
        )?;
        config.reasoning_effort = Some(reasoning_effort);
    }

    Ok(())
}

fn apply_spawn_agent_service_tier(
    config: &mut AgentConfigLayer,
    parent_service_tier: Option<&str>,
    requested_service_tier: Option<&str>,
) -> Result<(), String> {
    let candidate_service_tiers = [
        config.service_tier.clone(),
        requested_service_tier.map(str::to_string),
        parent_service_tier.map(str::to_string),
    ];
    if candidate_service_tiers.iter().all(Option::is_none) {
        config.service_tier = None;
        return Ok(());
    }

    if config.model.trim().is_empty() {
        return Err(
            "spawn_agent could not resolve the child model for service tier validation".to_string(),
        );
    }
    let model_info = model_request_info_for_catalog(&config.model, config.model_catalog.as_ref());
    if let Some(requested_service_tier) = requested_service_tier {
        let requested_service_tier = requested_service_tier.trim();
        if !supports_value(&model_info.supported_service_tiers, requested_service_tier) {
            let supported_service_tiers = if model_info.supported_service_tiers.is_empty() {
                "none".to_string()
            } else {
                model_info.supported_service_tiers.join(", ")
            };
            return Err(format!(
                "Service tier `{requested_service_tier}` is not supported for model `{}`. Supported service tiers: {supported_service_tiers}",
                config.model
            ));
        }
    }

    config.service_tier = candidate_service_tiers
        .into_iter()
        .flatten()
        .map(|tier| tier.trim().to_string())
        .find(|tier| supports_value(&model_info.supported_service_tiers, tier));
    Ok(())
}

fn find_spawn_agent_model_name(
    config: &AgentConfigLayer,
    requested_model: &str,
) -> Result<String, String> {
    let requested_model = requested_model.trim();
    let available_models = config
        .available_models
        .iter()
        .map(|preset| preset.id.clone())
        .collect::<Vec<_>>();
    available_models
        .iter()
        .find(|model| model.as_str() == requested_model)
        .cloned()
        .ok_or_else(|| {
            let available = available_models.join(", ");
            format!(
                "Unknown model `{requested_model}` for spawn_agent. Available models: {available}"
            )
        })
}

fn apply_model_catalog_override(config: &mut AgentConfigLayer) {
    let Some(path) = config
        .config_overrides
        .iter()
        .rev()
        .find(|(key, _)| key == "model_catalog_json")
        .and_then(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let Ok(json) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(catalog) = serde_json::from_str::<browser_use_providers::ModelCatalog>(&json) else {
        return;
    };
    config.available_models = catalog.presets(true);
    config.model_catalog = Some(catalog);
}

fn validate_spawn_agent_reasoning_effort(
    model: &str,
    supported_reasoning_efforts: &[String],
    requested_reasoning_effort: &str,
) -> Result<(), String> {
    if supports_value(supported_reasoning_efforts, requested_reasoning_effort) {
        return Ok(());
    }

    let supported = supported_reasoning_efforts.join(", ");
    Err(format!(
        "Reasoning effort `{requested_reasoning_effort}` is not supported for model `{model}`. Supported reasoning efforts: {supported}"
    ))
}

fn supports_value(supported: &[String], requested: &str) -> bool {
    supported.iter().any(|value| value == requested)
}

fn normalize_reasoning_effort(reasoning_effort: &str) -> String {
    reasoning_effort
        .trim()
        .to_ascii_lowercase()
        .replace('-', "_")
}

fn child_agent_path(parent_agent_path: &str, task_name: &str) -> String {
    let parent = parent_agent_path.trim_end_matches('/');
    format!("{parent}/{task_name}")
}
