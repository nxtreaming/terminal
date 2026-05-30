//! Live sub-agent registry + `<subagents>` env block (codex `agent/registry.rs`
//! + legacy `environment_context_subagents_for_session`).
//!
//! Tracks the live agents spawned in a session — their canonical path, id,
//! nickname, role, status, and depth — and renders the `<subagents>` environment
//! block the parent sees.
//!
//! Parity:
//! - Live-agent metadata + `live_agents`: `core/src/agent/registry.rs:35-42,
//!   155-167` `AgentMetadata { agent_id, agent_path, agent_nickname, agent_role,
//!   last_task_message }`.
//! - Nickname assignment from a candidate pool: `core/src/agent/registry.rs:
//!   202-240` `reserve_agent_nickname(names, preferred)`.
//! - `<subagents>` block shape: legacy
//!   `terminal-decodex/crates/browser-use-core/src/lib.rs`
//!   `environment_context_subagents_for_session` (~:13400-13498).

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::sync::Mutex;

use super::mailbox::AgentStatus;

/// Live metadata for one spawned agent (codex `AgentMetadata` :35-42, plus a
/// `depth` so the registry can answer depth queries without re-deriving from the
/// session source).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRecord {
    /// Canonical agent path, e.g. `/root/explorer_1`.
    pub agent_path: String,
    /// Opaque process-local agent id.
    pub agent_id: String,
    /// Assigned nickname (from the role's `nickname_candidates`), if any.
    pub nickname: Option<String>,
    /// Resolved role name (`default`/`explorer`/`worker`/user-defined).
    pub role: Option<String>,
    pub status: AgentStatus,
    /// Spawn depth of this agent (root = 0).
    pub depth: i32,
}

/// Registry of live sub-agents for a session.
#[derive(Default)]
pub struct AgentRegistry {
    inner: Mutex<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    /// Keyed by canonical agent path for stable ordering and lookup.
    agents: BTreeMap<String, AgentRecord>,
    /// Nicknames already handed out, so the pool does not collide
    /// (codex `used_agent_nicknames`).
    used_nicknames: HashSet<String>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a freshly-spawned agent (codex `register_spawned_thread`
    /// :183-200). If the record carries a nickname it is marked used.
    pub fn register(&self, record: AgentRecord) {
        let mut inner = self.lock();
        if let Some(nickname) = &record.nickname {
            inner.used_nicknames.insert(nickname.clone());
        }
        inner.agents.insert(record.agent_path.clone(), record);
    }

    /// Update an agent's status (codex updates status as the child progresses).
    /// Returns `true` if the agent existed.
    pub fn update_status(&self, agent_path: &str, status: AgentStatus) -> bool {
        let mut inner = self.lock();
        match inner.agents.get_mut(agent_path) {
            Some(record) => {
                record.status = status;
                true
            }
            None => false,
        }
    }

    /// Fetch a clone of an agent's record by path.
    pub fn get(&self, agent_path: &str) -> Option<AgentRecord> {
        self.lock().agents.get(agent_path).cloned()
    }

    /// All live agents (codex `live_agents` :155-167), ordered by path.
    pub fn list_agents(&self) -> Vec<AgentRecord> {
        self.lock().agents.values().cloned().collect()
    }

    /// Pick + reserve a nickname from `candidates`, skipping ones already in use
    /// (codex `reserve_agent_nickname` :202-240). Deterministic (first
    /// available) so tests are reproducible; codex randomizes, but the contract
    /// — "an unused candidate, reserved" — is identical. Returns `None` if all
    /// candidates are exhausted.
    pub fn reserve_nickname(&self, candidates: &[String]) -> Option<String> {
        let mut inner = self.lock();
        let chosen = candidates
            .iter()
            .find(|name| !inner.used_nicknames.contains(*name))
            .cloned()?;
        inner.used_nicknames.insert(chosen.clone());
        Some(chosen)
    }

    /// Render the `<subagents>` environment block (legacy
    /// `environment_context_subagents_for_session`). Empty when there are no
    /// live agents (callers omit the block entirely in that case).
    pub fn render_subagents_block(&self) -> String {
        let agents = self.list_agents();
        if agents.is_empty() {
            return "<subagents>\n</subagents>".to_string();
        }
        let mut out = String::from("<subagents>\n");
        for record in agents {
            let nickname = record.nickname.as_deref().unwrap_or("");
            let role = record.role.as_deref().unwrap_or("default");
            out.push_str(&format!(
                "  <subagent path=\"{path}\" nickname=\"{nickname}\" role=\"{role}\" status=\"{status}\" depth=\"{depth}\" />\n",
                path = record.agent_path,
                status = record.status.as_str(),
                depth = record.depth,
            ));
        }
        out.push_str("</subagents>");
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
