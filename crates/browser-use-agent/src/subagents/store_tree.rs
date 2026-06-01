//! `subagents/store_tree.rs` — **Store-backed** agent-tree + agent-status helpers.
//!
//! These are the durable, `Store`-threaded variants of the agent-tree ops. The
//! registry-based variants in [`super::tree`] walk the in-memory live
//! [`AgentRegistry`](super::registry::AgentRegistry); the tui/cli instead hold a
//! durable [`Store`] (28 real call sites) and need faithful `&Store`-based
//! versions of the same operations, distinct from the registry ones. This module
//! ports them verbatim from legacy `browser-use-core`.
//!
//! Ported from `terminal-decodex/crates/browser-use-core/src/lib.rs`:
//! - `canonical_agent_path_from_task_name`            (lib.rs:22244)
//! - `display_agent_path_for_session`                 (lib.rs:22476)
//! - `local_agent_status_value`                       (lib.rs:23181)
//! - `final_statuses_for_v1_wait`                     (lib.rs:22796)
//! - `last_task_message_for_agent`                    (lib.rs:23211)
//! - `cleanup_agent_runtime_state_for_agent_subtree`  (lib.rs:22375)
//! - `collect_agent_tree`        (Store-based, lib.rs:22348 + `_into` :22410)
//! - `root_session_id`           (Store-based, lib.rs:22336)
//! - `resolve_agent_reference_in_tree` (Store-based, lib.rs:22436)
//!
//! The pure path helper `canonical_agent_reference` already exists in
//! [`super::tree`]; it is reused here (not redefined). `AgentSummary`,
//! `SessionMeta`, `SessionStatus`, `failure_from_events`, and
//! `session_result_from_events` are imported from the real `browser-use-store` /
//! `browser-use-protocol` crates rather than duplicated.
//!
//! ## Store APIs used (all real, verified against
//! `crates/browser-use-store/src/lib.rs`)
//! - `Store::load_session` (`parent_id`, `status`)            — :380
//! - `Store::list_child_agents` -> `Vec<AgentSummary>`        — :519
//! - `Store::agent_path_for_session`                          — :542
//! - `Store::events_for_session` -> `Vec<EventRecord>`        — :401
//! - `Store::messages_for_agent` -> `Vec<AgentMessage>`       — :715
//!
//! ## Faithful-equivalent note (documented divergence)
//! Legacy `cleanup_agent_runtime_state_for_agent_subtree` summed
//! `cleanup_agent_runtime_state_for_session`, which tore down per-session
//! `unified_exec` commands + MCP connections (`tools::command` / `mcp`,
//! `browser-use-core` lib.rs:22358) — process-runtime infra that does NOT live in
//! the agent crate's persistence layer. The observable Store behavior is the
//! *subtree session-id collection* (`collect_agent_subtree_session_ids`,
//! lib.rs:22424). We preserve that exactly and let the caller supply the
//! per-session teardown via a closure, summing the per-session counts identically
//! to legacy. [`collect_agent_subtree_session_ids`] is also exposed directly so
//! callers that already own the runtime can drive cleanup themselves.

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use browser_use_protocol::{
    failure_from_events, session_result_from_events, SessionMeta, SessionStatus,
};
use browser_use_store::{AgentSummary, Store};

use super::tree::{canonical_agent_reference, resolve_agent_path_v2};

/// A resolved agent reference within a Store-backed tree.
///
/// Port of legacy `ResolvedAgentReference` (lib.rs:22328): the canonical
/// `session_id` the reference points at, its display `agent_path`, the matched
/// [`AgentSummary`] row (absent when the reference is the tree root, which has no
/// edge row), and whether it resolved to the root session.
#[derive(Clone, Debug)]
pub struct ResolvedAgentReference {
    pub session_id: String,
    pub agent_path: String,
    pub summary: Option<AgentSummary>,
    pub is_root: bool,
}

/// Build the canonical agent path for a freshly-spawned child task.
///
/// Verbatim port of legacy `canonical_agent_path_from_task_name` (lib.rs:22244):
/// trim + validate the task name ([`validate_agent_task_name`]), canonicalize the
/// parent path ([`canonical_agent_path`]), and append: `"{parent}/{task}"`, with
/// the special case that a `/root` parent yields `"/root/{task}"`.
pub fn canonical_agent_path_from_task_name(
    task_name: &str,
    parent_agent_path: &str,
) -> Result<String, String> {
    let task_name = task_name.trim();
    validate_agent_task_name(task_name)?;
    let parent_agent_path = canonical_agent_path(parent_agent_path);
    if parent_agent_path == "/root" {
        Ok(format!("/root/{task_name}"))
    } else {
        Ok(format!("{parent_agent_path}/{task_name}"))
    }
}

/// Validate a spawn task name.
///
/// Verbatim port of legacy `validate_agent_task_name` (lib.rs:22258): non-empty,
/// not a reserved word (`root`/`.`/`..`), no `/`, and only lowercase ASCII
/// letters, digits, and `_`.
fn validate_agent_task_name(task_name: &str) -> Result<(), String> {
    if task_name.is_empty() {
        return Err("task_name must not be empty".to_string());
    }
    if matches!(task_name, "root" | "." | "..") {
        return Err(format!("task_name `{task_name}` is reserved"));
    }
    if task_name.contains('/') {
        return Err("task_name must not contain `/`".to_string());
    }
    if !task_name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Err(
            "task_name must use only lowercase letters, digits, and underscores".to_string(),
        );
    }
    Ok(())
}

/// Normalize an agent path to its canonical `/root/...` form.
///
/// Verbatim port of legacy `canonical_agent_path` (lib.rs:22279): trim outer
/// slashes; empty / `root` -> `/root`; per-segment lowercase, replacing any
/// char outside `[a-z0-9-_.]` with `-` and trimming leading/trailing `-`,
/// dropping empties; prefix with `/root` unless the first segment is already
/// `root`.
fn canonical_agent_path(path: &str) -> String {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed == "root" {
        return "/root".to_string();
    }
    let segments = trimmed
        .split('/')
        .filter_map(|segment| {
            let normalized = segment
                .trim()
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                        ch.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .trim_matches('-')
                .to_string();
            (!normalized.is_empty()).then_some(normalized)
        })
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return "/root".to_string();
    }
    if segments.first().is_some_and(|segment| segment == "root") {
        format!("/{}", segments.join("/"))
    } else {
        format!("/root/{}", segments.join("/"))
    }
}

/// Walk parent links up from `session_id` and return the root session id.
///
/// Verbatim port of legacy Store-based `root_session_id` (lib.rs:22336): load the
/// session, follow `parent_id` until a session with no parent, and return its id.
/// Errors if any session/parent id is unknown.
pub fn root_session_id(store: &Store, session_id: &str) -> Result<String> {
    let mut current = store
        .load_session(session_id)?
        .with_context(|| format!("unknown session id: {session_id}"))?;
    while let Some(parent_id) = current.parent_id.clone() {
        current = store
            .load_session(&parent_id)?
            .with_context(|| format!("unknown parent session id: {parent_id}"))?;
    }
    Ok(current.id)
}

/// Collect every agent in the subtree below `parent_session_id` (excluding the
/// parent itself), depth-first pre-order, pruning at `closed` agents.
///
/// Verbatim port of legacy Store-based `collect_agent_tree` (lib.rs:22348) +
/// `collect_agent_tree_into` (lib.rs:22410): for each child edge from
/// `store.list_child_agents`, push the [`AgentSummary`], and recurse into the
/// child only when its edge `status != "closed"`.
pub fn collect_agent_tree(store: &Store, parent_session_id: &str) -> Result<Vec<AgentSummary>> {
    let mut out = Vec::new();
    collect_agent_tree_into(store, parent_session_id, &mut out)?;
    Ok(out)
}

/// Recursive worker for [`collect_agent_tree`] (legacy `collect_agent_tree_into`,
/// lib.rs:22410).
fn collect_agent_tree_into(
    store: &Store,
    parent_session_id: &str,
    out: &mut Vec<AgentSummary>,
) -> Result<()> {
    for child in store.list_child_agents(parent_session_id)? {
        out.push(child.clone());
        if child.status != "closed" {
            collect_agent_tree_into(store, &child.child_session_id, out)?;
        }
    }
    Ok(())
}

/// Collect the session ids of the whole agent subtree rooted at `root_session_id`
/// (root included), depth-first pre-order, without pruning closed edges.
///
/// Verbatim port of legacy `collect_agent_subtree_session_ids` (lib.rs:22424).
/// Exposed publicly so callers owning the per-session runtime can drive teardown
/// themselves (see the faithful-equivalent note in the module docs).
pub fn collect_agent_subtree_session_ids(
    store: &Store,
    root_session_id: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    out.push(root_session_id.to_string());
    for child in store.list_child_agents(root_session_id)? {
        collect_agent_subtree_session_ids(store, &child.child_session_id, out)?;
    }
    Ok(())
}

/// Tear down per-session runtime state for every session in an agent subtree and
/// return the total number of items cleaned up.
///
/// Faithful equivalent of legacy `cleanup_agent_runtime_state_for_agent_subtree`
/// (lib.rs:22375). Legacy summed `cleanup_agent_runtime_state_for_session` over
/// the subtree (lib.rs:22358 == unified-exec command teardown + MCP connection
/// teardown). That process-runtime infra does not live in the agent crate's
/// persistence layer, so the per-session teardown is supplied by the caller as
/// `cleanup_session`; the Store-driven part — collecting the subtree session ids
/// (lib.rs:22424) and summing the per-session counts — is preserved exactly.
///
/// With a no-op `cleanup_session` this returns `0` like legacy on an idle tree.
pub fn cleanup_agent_runtime_state_for_agent_subtree<F>(
    store: &Store,
    root_session_id: &str,
    mut cleanup_session: F,
) -> Result<usize>
where
    F: FnMut(&str) -> usize,
{
    let mut session_ids = Vec::new();
    collect_agent_subtree_session_ids(store, root_session_id, &mut session_ids)?;
    Ok(session_ids
        .iter()
        .map(|session_id| cleanup_session(session_id))
        .sum())
}

/// Resolve a human-supplied agent reference against the live tree containing
/// `current_session_id`.
///
/// Verbatim port of legacy Store-based `resolve_agent_reference_in_tree`
/// (lib.rs:22436):
/// 1. trim; empty -> `Ok(None)`;
/// 2. find the tree root ([`root_session_id`]) and the caller's display path
///    ([`display_agent_path_for_session`]);
/// 3. canonicalize the reference relative to that path (reusing the pure
///    [`canonical_agent_reference`] from [`super::tree`]);
/// 4. if the reference equals the root session id, or canonicalizes to `/root`,
///    resolve to the root (`summary: None`, `is_root: true`);
/// 5. otherwise scan the collected tree (skipping `closed` agents) for an agent
///    whose `child_session_id` equals the raw reference, or whose `agent_path`
///    equals either the raw reference or the canonical path.
pub fn resolve_agent_reference_in_tree(
    store: &Store,
    current_session_id: &str,
    reference: &str,
) -> Result<Option<ResolvedAgentReference>> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Ok(None);
    }
    let root_id = root_session_id(store, current_session_id)?;
    let current_agent_path = display_agent_path_for_session(store, current_session_id)?;
    let canonical = canonical_agent_reference(reference, &current_agent_path);
    if reference == root_id || canonical == "/root" {
        return Ok(Some(ResolvedAgentReference {
            session_id: root_id,
            agent_path: "/root".to_string(),
            summary: None,
            is_root: true,
        }));
    }
    Ok(collect_agent_tree(store, &root_id)?
        .into_iter()
        .filter(|agent| agent.status != "closed")
        .find_map(|agent| {
            let path = agent.agent_path.clone().unwrap_or_else(|| {
                display_agent_path_for_session(store, &agent.child_session_id)
                    .unwrap_or_else(|_| agent.child_session_id.clone())
            });
            (agent.child_session_id == reference
                || agent.agent_path.as_deref() == Some(reference)
                || agent.agent_path.as_deref() == Some(canonical.as_str()))
            .then(|| ResolvedAgentReference {
                session_id: agent.child_session_id.clone(),
                agent_path: path,
                summary: Some(agent),
                is_root: false,
            })
        }))
}

/// Resolve a Codex MultiAgentV2 agent reference against the Store-backed tree.
///
/// This keeps the durable-tree behavior but uses Codex's stricter v2 path
/// grammar: raw session ids are accepted first, then `AgentPath`-style absolute
/// or relative task paths are resolved. Legacy shorthands such as bare `root`,
/// `parent`, `.`, `..`, nicknames, and `@mentions` are intentionally excluded
/// from model-facing v2 tools.
pub fn resolve_agent_reference_in_tree_v2(
    store: &Store,
    current_session_id: &str,
    reference: &str,
) -> Result<Option<ResolvedAgentReference>> {
    if reference.is_empty() {
        anyhow::bail!("agent path must not be empty");
    }
    let root_id = root_session_id(store, current_session_id)?;
    if reference == root_id {
        return Ok(Some(ResolvedAgentReference {
            session_id: root_id,
            agent_path: "/root".to_string(),
            summary: None,
            is_root: true,
        }));
    }

    let tree = collect_agent_tree(store, &root_id)?;
    if let Some(agent) = tree
        .iter()
        .filter(|agent| agent.status != "closed")
        .find(|agent| agent.child_session_id == reference)
        .cloned()
    {
        let path = agent.agent_path.clone().unwrap_or_else(|| {
            display_agent_path_for_session(store, &agent.child_session_id)
                .unwrap_or_else(|_| agent.child_session_id.clone())
        });
        return Ok(Some(ResolvedAgentReference {
            session_id: agent.child_session_id.clone(),
            agent_path: path,
            summary: Some(agent),
            is_root: false,
        }));
    }

    let current_agent_path = display_agent_path_for_session(store, current_session_id)?;
    let agent_path =
        resolve_agent_path_v2(&current_agent_path, reference).map_err(anyhow::Error::msg)?;
    if agent_path == "/root" {
        return Ok(Some(ResolvedAgentReference {
            session_id: root_id,
            agent_path,
            summary: None,
            is_root: true,
        }));
    }

    Ok(tree
        .into_iter()
        .filter(|agent| agent.status != "closed")
        .find_map(|agent| {
            let path = agent.agent_path.clone().unwrap_or_else(|| {
                display_agent_path_for_session(store, &agent.child_session_id)
                    .unwrap_or_else(|_| agent.child_session_id.clone())
            });
            (path == agent_path).then(|| ResolvedAgentReference {
                session_id: agent.child_session_id.clone(),
                agent_path: path,
                summary: Some(agent),
                is_root: false,
            })
        }))
}

/// Display path for a session: its stored `agent_path`, else `/root` for a
/// parentless session, else the raw session id.
///
/// Verbatim port of legacy `display_agent_path_for_session` (lib.rs:22476): prefer
/// a non-blank `store.agent_path_for_session`; otherwise load the session and
/// return `/root` when it has no parent, else the session id itself.
pub fn display_agent_path_for_session(store: &Store, session_id: &str) -> Result<String> {
    if let Some(path) = store.agent_path_for_session(session_id)? {
        if !path.trim().is_empty() {
            return Ok(path);
        }
    }
    let session = store
        .load_session(session_id)?
        .with_context(|| format!("unknown session id: {session_id}"))?;
    if session.parent_id.is_none() {
        Ok("/root".to_string())
    } else {
        Ok(session_id.to_string())
    }
}

/// Compute the public status value for a single agent session.
///
/// Verbatim port of legacy `local_agent_status_value` (lib.rs:23181):
/// - a `closed` edge -> `"shutdown"`;
/// - else, an active interruption boundary -> `"interrupted"`;
/// - else, a `Cancelled` session -> `"shutdown"`;
/// - else, derive from the event log's latest terminal event;
/// - else map the [`SessionStatus`]: `Created` -> `"pending_init"`,
///   `Running` -> `"running"`, `Done` -> `{ "completed": null }`,
///   `Failed` -> `{ "errored": "failed" }`, `Cancelled` -> `"shutdown"`.
///
/// `failure_from_events` / `session_result_from_events` are the real
/// `browser-use-protocol` helpers (lib.rs:558 / :409).
pub fn local_agent_status_value(
    store: &Store,
    session: &SessionMeta,
    agent: Option<&AgentSummary>,
) -> Result<Value> {
    let events = store.events_for_session(&session.id)?;
    if agent.is_some_and(|agent| agent.status == "closed") {
        return Ok(Value::String("shutdown".to_string()));
    }
    if session_was_interrupted(&events) {
        return Ok(Value::String("interrupted".to_string()));
    }
    if session.status == SessionStatus::Cancelled {
        return Ok(Value::String("shutdown".to_string()));
    }
    if let Some(terminal) = latest_terminal_status_from_events(&events) {
        return Ok(terminal);
    }
    Ok(match session.status {
        SessionStatus::Created => Value::String("pending_init".to_string()),
        SessionStatus::Running => Value::String("running".to_string()),
        SessionStatus::Done => serde_json::json!({
            "completed": null,
        }),
        SessionStatus::Failed => serde_json::json!({
            "errored": "failed",
        }),
        SessionStatus::Cancelled => Value::String("shutdown".to_string()),
    })
}

fn latest_terminal_status_from_events(
    events: &[browser_use_protocol::EventRecord],
) -> Option<Value> {
    events
        .iter()
        .rev()
        .find(|event| matches!(event.event_type.as_str(), "session.done" | "session.failed"))
        .map(|event| match event.event_type.as_str() {
            "session.done" => serde_json::json!({
                "completed": session_result_from_events(std::slice::from_ref(event)),
            }),
            "session.failed" => serde_json::json!({
                "errored": failure_from_events(std::slice::from_ref(event))
                    .unwrap_or_else(|| "failed".to_string()),
            }),
            _ => Value::Null,
        })
}

pub fn session_was_interrupted(events: &[browser_use_protocol::EventRecord]) -> bool {
    events
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.event_type.as_str(),
                "session.cancelled"
                    | "session.interrupted"
                    | "session.input"
                    | "session.followup"
                    | "session.done"
                    | "session.failed"
            )
        })
        .is_some_and(|event| {
            event.event_type == "session.interrupted"
                || (event.event_type == "session.cancelled"
                    && event
                        .payload
                        .get("reason")
                        .and_then(Value::as_str)
                        .is_some_and(|reason| reason.to_ascii_lowercase().contains("interrupt")))
        })
}

/// Collect the final statuses of v1 `wait` targets that have stopped running.
///
/// Verbatim port of legacy `final_statuses_for_v1_wait` (lib.rs:22796): for each
/// target, `"not_found"` if the session is missing; skip targets still
/// active or `Created` (still in flight); otherwise record
/// [`local_agent_status_value`].
pub fn final_statuses_for_v1_wait(store: &Store, targets: &[&str]) -> Result<Map<String, Value>> {
    let mut statuses = Map::new();
    for target in targets {
        let Some(session) = store.load_session(target)? else {
            statuses.insert(
                (*target).to_string(),
                Value::String("not_found".to_string()),
            );
            continue;
        };
        if session.status.is_active() || session.status == SessionStatus::Created {
            continue;
        }
        let summary = store.agent_summary_for_child(&session.id)?;
        let status = local_agent_status_value(store, &session, summary.as_ref())?;
        if status == Value::String("interrupted".to_string()) {
            continue;
        }
        let key = display_agent_path_for_session(store, &session.id)
            .unwrap_or_else(|_| (*target).to_string());
        statuses.insert(key, status);
    }
    Ok(statuses)
}

/// The most recent task-bearing message for an agent: the later of its latest
/// `session.followup`/`session.input` event text and its latest inbox message.
///
/// Verbatim port of legacy `last_task_message_for_agent` (lib.rs:23211): scan the
/// event log in reverse for the newest `session.followup`/`session.input` with a
/// `text` payload, take the newest inbox message (`messages_for_agent().last()`),
/// and return whichever of the two is newer by timestamp (mail wins on a tie, as
/// legacy uses `mail.0 >= event.0`).
pub fn last_task_message_for_agent(store: &Store, session_id: &str) -> Result<Option<String>> {
    let events = store.events_for_session(session_id)?;
    let latest_event_message = events.iter().rev().find_map(|event| {
        matches!(
            event.event_type.as_str(),
            "session.followup" | "session.input"
        )
        .then(|| event.payload.get("text").and_then(Value::as_str))
        .flatten()
        .map(|text| (event.ts_ms, text.to_string()))
    });
    let latest_mail_message = store
        .messages_for_agent(session_id)?
        .into_iter()
        .last()
        .map(|message| (message.created_ms, message.content));
    Ok(match (latest_event_message, latest_mail_message) {
        (Some(event), Some(mail)) => Some(if mail.0 >= event.0 { mail.1 } else { event.1 }),
        (Some(event), None) => Some(event.1),
        (None, Some(mail)) => Some(mail.1),
        (None, None) => None,
    })
}

#[cfg(test)]
mod store_tree_tests {
    use super::*;
    use browser_use_protocol::SessionStatus;

    /// Open a tempdir-backed `Store` (same pattern as
    /// `infra/persistence.rs` / the store crate's own tests). The `TempDir` is
    /// returned so the caller keeps it alive for the test's lifetime.
    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("open store");
        (dir, store)
    }

    /// Seed a small parent/child agent tree via the real Store API:
    ///   root (parentless)
    ///     /root/alpha   (child, nickname "worker")
    ///       /root/alpha/leaf
    ///     /root/beta
    /// Returns (root_id, alpha_id, leaf_id, beta_id).
    fn seed_tree(store: &Store) -> (String, String, String, String) {
        let root = store
            .create_session(None, std::path::Path::new("/tmp"))
            .expect("root");
        store
            .set_status(&root.id, SessionStatus::Running)
            .expect("root running");
        let alpha = store
            .create_child_session(
                &root.id,
                std::path::Path::new("/tmp"),
                Some("/root/alpha"),
                Some("worker"),
                None,
            )
            .expect("alpha");
        let leaf = store
            .create_child_session(
                &alpha.id,
                std::path::Path::new("/tmp"),
                Some("/root/alpha/leaf"),
                None,
                None,
            )
            .expect("leaf");
        let beta = store
            .create_child_session(
                &root.id,
                std::path::Path::new("/tmp"),
                Some("/root/beta"),
                None,
                None,
            )
            .expect("beta");
        (root.id, alpha.id, leaf.id, beta.id)
    }

    #[test]
    fn canonical_path_from_task_name_forms() {
        assert_eq!(
            canonical_agent_path_from_task_name("worker", "/root").unwrap(),
            "/root/worker"
        );
        assert_eq!(
            canonical_agent_path_from_task_name("w2", "/root/alpha").unwrap(),
            "/root/alpha/w2"
        );
        // Parent path is canonicalized first.
        assert_eq!(
            canonical_agent_path_from_task_name("w", "root").unwrap(),
            "/root/w"
        );
        // Validation failures.
        assert!(canonical_agent_path_from_task_name("", "/root").is_err());
        assert!(canonical_agent_path_from_task_name("root", "/root").is_err());
        assert!(canonical_agent_path_from_task_name("a/b", "/root").is_err());
        assert!(canonical_agent_path_from_task_name("Bad", "/root").is_err());
    }

    #[test]
    fn root_session_id_walks_to_top() {
        let (_dir, store) = store();
        let (root, _alpha, leaf, _beta) = seed_tree(&store);
        assert_eq!(root_session_id(&store, &leaf).unwrap(), root);
        assert_eq!(root_session_id(&store, &root).unwrap(), root);
    }

    #[test]
    fn root_session_id_unknown_errors() {
        let (_dir, store) = store();
        assert!(root_session_id(&store, "nope").is_err());
    }

    #[test]
    fn collect_agent_tree_is_preorder_and_prunes_closed() {
        let (_dir, store) = store();
        let (root, _alpha, _leaf, _beta) = seed_tree(&store);

        // The walk is depth-first pre-order; sibling order follows the Store's
        // `list_child_agents` ordering (`updated_ms DESC`), which is timing-
        // dependent, so assert membership via a sorted view rather than a fixed
        // sibling order. The structural guarantee we check: every live agent in
        // the subtree is present, and `leaf` directly follows its parent `alpha`.
        let tree = collect_agent_tree(&store, &root).unwrap();
        let paths: Vec<&str> = tree
            .iter()
            .map(|a| a.agent_path.as_deref().unwrap())
            .collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec!["/root/alpha", "/root/alpha/leaf", "/root/beta"]
        );
        let alpha_idx = paths.iter().position(|p| *p == "/root/alpha").unwrap();
        assert_eq!(paths[alpha_idx + 1], "/root/alpha/leaf");

        // Closing alpha's edge prunes its subtree (leaf no longer recursed into),
        // but the closed edge itself is still emitted (legacy pushes then prunes).
        store.set_child_agent_status(&_alpha, "closed").unwrap();
        let pruned = collect_agent_tree(&store, &root).unwrap();
        let mut pruned_paths: Vec<&str> = pruned
            .iter()
            .map(|a| a.agent_path.as_deref().unwrap())
            .collect();
        pruned_paths.sort_unstable();
        assert_eq!(pruned_paths, vec!["/root/alpha", "/root/beta"]);
    }

    #[test]
    fn display_agent_path_prefers_stored_then_root_then_id() {
        let (_dir, store) = store();
        let (root, alpha, _leaf, _beta) = seed_tree(&store);
        // Root is parentless and has no agent_path -> "/root".
        assert_eq!(
            display_agent_path_for_session(&store, &root).unwrap(),
            "/root"
        );
        // Child has a stored agent_path.
        assert_eq!(
            display_agent_path_for_session(&store, &alpha).unwrap(),
            "/root/alpha"
        );
    }

    #[test]
    fn resolve_reference_root_path_and_nickname() {
        let (_dir, store) = store();
        let (root, alpha, leaf, _beta) = seed_tree(&store);

        // Empty -> None.
        assert!(resolve_agent_reference_in_tree(&store, &root, "  ")
            .unwrap()
            .is_none());

        // Root by canonical path.
        let r = resolve_agent_reference_in_tree(&store, &leaf, "/root")
            .unwrap()
            .unwrap();
        assert!(r.is_root);
        assert_eq!(r.session_id, root);
        assert_eq!(r.agent_path, "/root");
        assert!(r.summary.is_none());

        // Root by raw session id.
        let r2 = resolve_agent_reference_in_tree(&store, &leaf, &root)
            .unwrap()
            .unwrap();
        assert!(r2.is_root);

        // Absolute child path from the root.
        let r3 = resolve_agent_reference_in_tree(&store, &root, "/root/alpha")
            .unwrap()
            .unwrap();
        assert!(!r3.is_root);
        assert_eq!(r3.session_id, alpha);
        assert_eq!(r3.agent_path, "/root/alpha");
        assert!(r3.summary.is_some());

        // Raw child session id resolves to that child.
        let r4 = resolve_agent_reference_in_tree(&store, &root, &leaf)
            .unwrap()
            .unwrap();
        assert_eq!(r4.session_id, leaf);
        assert_eq!(r4.agent_path, "/root/alpha/leaf");

        // Unknown reference -> None.
        assert!(
            resolve_agent_reference_in_tree(&store, &root, "/root/ghost")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolve_skips_closed_agents() {
        let (_dir, store) = store();
        let (root, _alpha, leaf, _beta) = seed_tree(&store);
        store.set_child_agent_status(&leaf, "closed").unwrap();
        // Closing the leaf's edge means it is pruned from resolution.
        assert!(
            resolve_agent_reference_in_tree(&store, &root, "/root/alpha/leaf")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn local_status_shutdown_pending_running_completed_errored() {
        let (_dir, store) = store();
        let (root, alpha, _leaf, beta) = seed_tree(&store);

        // Closed edge -> shutdown (regardless of session status).
        let alpha_summary = store.agent_summary_for_child(&alpha).unwrap().unwrap();
        let alpha_session = store.load_session(&alpha).unwrap().unwrap();
        store.set_child_agent_status(&alpha, "closed").unwrap();
        let closed_summary = store.agent_summary_for_child(&alpha).unwrap().unwrap();
        assert_eq!(
            local_agent_status_value(&store, &alpha_session, Some(&closed_summary)).unwrap(),
            Value::String("shutdown".to_string())
        );
        // Sanity: with the open summary it is NOT shutdown (Created -> pending_init).
        assert_eq!(
            local_agent_status_value(&store, &alpha_session, Some(&alpha_summary)).unwrap(),
            Value::String("pending_init".to_string())
        );

        // Running session -> "running".
        let root_session = store.load_session(&root).unwrap().unwrap();
        assert_eq!(
            local_agent_status_value(&store, &root_session, None).unwrap(),
            Value::String("running".to_string())
        );

        // A session.failed event -> { "errored": <msg> }.
        store
            .append_event(
                &beta,
                "session.failed",
                serde_json::json!({ "error": "boom" }),
            )
            .unwrap();
        let beta_session = store.load_session(&beta).unwrap().unwrap();
        assert_eq!(
            local_agent_status_value(&store, &beta_session, None).unwrap(),
            serde_json::json!({ "errored": "boom" })
        );
    }

    #[test]
    fn interrupted_status_is_not_sticky_after_followup_completion() {
        let (_dir, store) = store();
        let (_root, alpha, _leaf, _beta) = seed_tree(&store);
        store
            .append_event(
                &alpha,
                "session.cancelled",
                serde_json::json!({ "reason": "interrupted by send_input" }),
            )
            .unwrap();

        let alpha_session = store.load_session(&alpha).unwrap().unwrap();
        assert_eq!(
            local_agent_status_value(&store, &alpha_session, None).unwrap(),
            Value::String("interrupted".to_string())
        );

        store
            .append_event(
                &alpha,
                "session.followup",
                serde_json::json!({ "text": "continue" }),
            )
            .unwrap();
        store
            .append_event(
                &alpha,
                "session.done",
                serde_json::json!({ "result": "finished after resume" }),
            )
            .unwrap();
        store.set_status(&alpha, SessionStatus::Done).unwrap();

        let events = store.events_for_session(&alpha).unwrap();
        assert!(!session_was_interrupted(&events));
        let alpha_session = store.load_session(&alpha).unwrap().unwrap();
        assert_eq!(
            local_agent_status_value(&store, &alpha_session, None).unwrap(),
            serde_json::json!({ "completed": "finished after resume" })
        );
    }

    #[test]
    fn failed_status_is_not_sticky_after_later_completion() {
        let (_dir, store) = store();
        let (_root, alpha, _leaf, _beta) = seed_tree(&store);
        store
            .append_event(
                &alpha,
                "session.failed",
                serde_json::json!({ "error": "boom" }),
            )
            .unwrap();
        store.set_status(&alpha, SessionStatus::Failed).unwrap();
        let alpha_session = store.load_session(&alpha).unwrap().unwrap();
        assert_eq!(
            local_agent_status_value(&store, &alpha_session, None).unwrap(),
            serde_json::json!({ "errored": "boom" })
        );

        store
            .append_event(
                &alpha,
                "session.followup",
                serde_json::json!({ "text": "try again" }),
            )
            .unwrap();
        store
            .append_event(
                &alpha,
                "session.done",
                serde_json::json!({ "result": "recovered" }),
            )
            .unwrap();
        store.set_status(&alpha, SessionStatus::Done).unwrap();

        let alpha_session = store.load_session(&alpha).unwrap().unwrap();
        assert_eq!(
            local_agent_status_value(&store, &alpha_session, None).unwrap(),
            serde_json::json!({ "completed": "recovered" })
        );
    }

    #[test]
    fn closed_interrupted_child_reports_shutdown_not_interrupted() {
        let (_dir, store) = store();
        let (_root, alpha, _leaf, _beta) = seed_tree(&store);
        store
            .append_event(
                &alpha,
                "session.cancelled",
                serde_json::json!({ "reason": "interrupted by send_input" }),
            )
            .unwrap();
        store.set_status(&alpha, SessionStatus::Cancelled).unwrap();
        store
            .close_child_agent(&alpha, "closed by close_agent")
            .unwrap();

        let alpha_session = store.load_session(&alpha).unwrap().unwrap();
        let alpha_summary = store.agent_summary_for_child(&alpha).unwrap().unwrap();
        assert_eq!(
            local_agent_status_value(&store, &alpha_session, Some(&alpha_summary)).unwrap(),
            Value::String("shutdown".to_string())
        );
        let statuses = final_statuses_for_v1_wait(&store, &[alpha.as_str()]).unwrap();
        assert_eq!(
            statuses["/root/alpha"],
            Value::String("shutdown".to_string())
        );
    }

    #[test]
    fn final_statuses_for_v1_wait_filters_active_and_marks_missing() {
        let (_dir, store) = store();
        let (root, alpha, _leaf, beta) = seed_tree(&store);
        // root is Running (active) -> skipped; alpha is Created -> skipped.
        // beta -> Failed so it is reported; "ghost" is missing.
        store.set_status(&beta, SessionStatus::Failed).unwrap();

        let targets = [root.as_str(), alpha.as_str(), beta.as_str(), "ghost"];
        let statuses = final_statuses_for_v1_wait(&store, &targets).unwrap();

        assert!(!statuses.contains_key(&root));
        assert!(!statuses.contains_key(&alpha));
        assert_eq!(
            statuses.get("ghost"),
            Some(&Value::String("not_found".to_string()))
        );
        assert_eq!(
            statuses.get("/root/beta"),
            Some(&serde_json::json!({ "errored": "failed" }))
        );
    }

    #[test]
    fn last_task_message_prefers_newer_of_event_and_mail() {
        let (_dir, store) = store();
        let (root, alpha, _leaf, _beta) = seed_tree(&store);

        // No event, no mail -> None.
        assert!(last_task_message_for_agent(&store, &alpha)
            .unwrap()
            .is_none());

        // Only a followup event.
        store
            .append_event(
                &alpha,
                "session.followup",
                serde_json::json!({ "text": "from event" }),
            )
            .unwrap();
        assert_eq!(
            last_task_message_for_agent(&store, &alpha).unwrap(),
            Some("from event".to_string())
        );

        // Add a later inbox message (mail wins as it is newer / ties to mail).
        store
            .send_agent_message(&root, &alpha, "from mail", false)
            .unwrap();
        assert_eq!(
            last_task_message_for_agent(&store, &alpha).unwrap(),
            Some("from mail".to_string())
        );
    }

    #[test]
    fn cleanup_subtree_collects_ids_and_sums_counts() {
        let (_dir, store) = store();
        let (root, alpha, leaf, beta) = seed_tree(&store);

        // The ids the subtree walk visits (root included, pre-order, no pruning).
        // The root is always emitted first (pushed before recursion); sibling
        // order is `updated_ms DESC` (timing-dependent), so check root-first plus
        // full membership rather than a fixed sibling order.
        let mut ids = Vec::new();
        collect_agent_subtree_session_ids(&store, &root, &mut ids).unwrap();
        assert_eq!(ids[0], root);
        let mut sorted = ids.clone();
        sorted.sort();
        let mut expected = vec![root.clone(), alpha.clone(), leaf, beta];
        expected.sort();
        assert_eq!(sorted, expected);

        // A no-op cleanup sums to zero (legacy idle-tree behavior).
        assert_eq!(
            cleanup_agent_runtime_state_for_agent_subtree(&store, &root, |_| 0).unwrap(),
            0
        );
        // A per-session cleanup returning 1 sums to the subtree size.
        let total = cleanup_agent_runtime_state_for_agent_subtree(&store, &root, |_| 1).unwrap();
        assert_eq!(total, ids.len());
    }
}
