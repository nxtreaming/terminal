//! Synchronous agent-tree walk + reference resolution over the live
//! [`AgentRegistry`].
//!
//! This ports the legacy `browser-use-core` agent-tree ops onto the agent
//! crate's existing subagents infra. The legacy versions threaded a `Store` and
//! a per-session `parent_session_id` link (`store.list_child_agents`); this
//! crate's [`AgentRegistry`] instead keys every live agent by its canonical
//! `agent_path` (e.g. `/root`, `/root/explorer_1`, `/root/explorer_1/worker_1`)
//! and the parent/child relationship is *derived from the path* — the parent of
//! `/root/a/b` is `/root/a`. The tree-walk + reference-resolution **semantics**
//! are preserved exactly; only the storage substrate differs (no `Store`, no
//! `browser-use-core` dependency).
//!
//! Ported from `terminal-decodex/crates/browser-use-core/src/lib.rs`:
//! - `canonical_agent_reference` (lib.rs:22313)
//! - `root_session_id`           (lib.rs:22336) -> [`root_session`]
//! - `collect_agent_tree`        (lib.rs:22348) -> [`collect_agent_tree`]
//! - `resolve_agent_reference_in_tree` (lib.rs:22436)
//!   -> [`resolve_agent_reference_in_tree`]

use super::mailbox::AgentStatus;
use super::registry::{AgentRecord, AgentRegistry};

/// The canonical root agent path. Legacy uses `/root` as the top of every tree
/// (see `canonical_agent_reference`'s `..` handling, lib.rs:22313).
pub const ROOT_AGENT_PATH: &str = "/root";

/// One node in the collected agent tree: the agent's live record plus its
/// `depth` relative to the walk root (the walk root is depth 0).
///
/// Analogue of the legacy `AgentSummary` rows returned by `collect_agent_tree`,
/// carrying the registry's [`AgentRecord`] verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTreeNode {
    /// The live registry record for this agent.
    pub record: AgentRecord,
    /// Distance in path-segments from the walk root (root == 0).
    pub depth: usize,
}

impl AgentTreeNode {
    /// Convenience: the agent's canonical path.
    pub fn agent_path(&self) -> &str {
        &self.record.agent_path
    }
}

/// Return `true` if `candidate` is a direct child path of `parent`.
///
/// `/root/a` is a child of `/root`; `/root/a/b` is **not** a direct child of
/// `/root` (it is a grandchild). The split mirrors the path-segment structure
/// minted by [`super::manager::SubagentManager::spawn`]
/// (`format!("{parent}/{task}")`).
fn is_direct_child(parent: &str, candidate: &str) -> bool {
    match candidate.strip_prefix(parent) {
        // The remainder must be `/<single-segment>` with no further `/`.
        Some(rest) => {
            let Some(seg) = rest.strip_prefix('/') else {
                return false;
            };
            !seg.is_empty() && !seg.contains('/')
        }
        None => false,
    }
}

/// The parent path of `agent_path`, or `None` if it has no parent segment
/// (i.e. it is a bare root like `/root`).
///
/// Mirrors the `..` branch of legacy `canonical_agent_reference` (lib.rs:22313):
/// strip the trailing `/<segment>`; an empty / single-segment path has no
/// parent.
pub fn parent_path_of(agent_path: &str) -> Option<&str> {
    let trimmed = agent_path.trim_end_matches('/');
    let idx = trimmed.rfind('/')?;
    if idx == 0 {
        // `/root` -> parent is the empty string before the leading slash; treat
        // as "no parent" (this is the top of the tree).
        return None;
    }
    Some(&trimmed[..idx])
}

/// Walk the live agent tree rooted at `root_path` and return every agent in the
/// subtree (root included), each annotated with its depth.
///
/// Ported from legacy `collect_agent_tree` (lib.rs:22348) + `collect_agent_tree_into`
/// (lib.rs:22410): a deterministic depth-first pre-order walk
/// (parent-before-children). The legacy walk recursed through
/// `store.list_child_agents`; here a node's children are the registry records
/// whose path is a direct child of the node's path, taken in the registry's
/// canonical (path-sorted) order so the traversal is stable.
///
/// Unlike the legacy `collect_agent_tree` — which collected only descendants —
/// this includes the `root_path` node itself at depth 0 when it is present in
/// the registry, so a single walk yields the whole subtree the caller asked
/// about. If `root_path` is absent the result is empty.
pub fn collect_agent_tree(registry: &AgentRegistry, root_path: &str) -> Vec<AgentTreeNode> {
    let all = registry.list_agents();
    let mut out = Vec::new();
    let Some(root) = all.iter().find(|r| r.agent_path == root_path) else {
        return out;
    };
    collect_into(&all, root, 0, &mut out);
    out
}

fn collect_into(
    all: &[AgentRecord],
    node: &AgentRecord,
    depth: usize,
    out: &mut Vec<AgentTreeNode>,
) {
    out.push(AgentTreeNode {
        record: node.clone(),
        depth,
    });
    // `list_agents` is path-sorted (BTreeMap), so children come out in stable
    // ascending order — matching the legacy deterministic child ordering.
    for child in all
        .iter()
        .filter(|r| is_direct_child(&node.agent_path, &r.agent_path))
    {
        collect_into(all, child, depth + 1, out);
    }
}

/// Canonicalize a human-supplied agent reference relative to `current_agent_path`.
///
/// Ported verbatim from legacy `canonical_agent_reference` (lib.rs:22313):
/// - trim, strip a single leading `@`, trim again;
/// - empty -> the current path;
/// - absolute (`/…`) -> returned unchanged;
/// - `.` / `self` -> the current path;
/// - `..` -> the parent of the current path (or [`ROOT_AGENT_PATH`] if none);
/// - otherwise relative -> `{current_agent_path}/{reference}`.
pub fn canonical_agent_reference(reference: &str, current_agent_path: &str) -> String {
    let reference = reference.trim().trim_start_matches('@').trim();
    if reference.is_empty() {
        return current_agent_path.to_string();
    }
    if reference.starts_with('/') {
        return reference.to_string();
    }
    if reference == "." || reference == "self" {
        return current_agent_path.to_string();
    }
    if reference == ".." {
        return parent_path_of(current_agent_path)
            .unwrap_or(ROOT_AGENT_PATH)
            .to_string();
    }
    format!("{current_agent_path}/{reference}")
}

/// Resolve an agent reference (mention, path, `.`/`..`/`self`, or nickname)
/// against the tree containing `current_agent_path`, returning the matched live
/// [`AgentRecord`].
///
/// Ported from legacy `resolve_agent_reference_in_tree` (lib.rs:22436):
/// 1. find the tree root by walking parents up from `current_agent_path`
///    ([`root_session`]);
/// 2. canonicalize the reference relative to `current_agent_path`
///    ([`canonical_agent_reference`]);
/// 3. match the canonical path exactly against the collected tree (root
///    included);
/// 4. as a convenience fallback (legacy resolves nicknames via a separate
///    lookup), match a unique agent `nickname` case-sensitively.
///
/// Returns `None` when nothing matches.
pub fn resolve_agent_reference_in_tree(
    registry: &AgentRegistry,
    current_agent_path: &str,
    reference: &str,
) -> Option<AgentRecord> {
    let root_path =
        root_session(registry, current_agent_path).unwrap_or_else(|| ROOT_AGENT_PATH.to_string());
    let canonical = canonical_agent_reference(reference, current_agent_path);
    let tree = collect_agent_tree(registry, &root_path);

    // Exact canonical-path match (the legacy primary resolution).
    if let Some(node) = tree.iter().find(|n| n.record.agent_path == canonical) {
        return Some(node.record.clone());
    }

    // Nickname fallback: a single agent whose nickname equals the raw (trimmed,
    // de-@'d) reference token.
    let token = reference.trim().trim_start_matches('@').trim();
    if !token.is_empty() {
        let mut hits = tree
            .iter()
            .filter(|n| n.record.nickname.as_deref() == Some(token));
        if let Some(node) = hits.next() {
            if hits.next().is_none() {
                return Some(node.record.clone());
            }
        }
    }
    None
}

/// Walk parent links upward from `agent_path` and return the root agent's path.
///
/// Ported from legacy `root_session_id` (lib.rs:22336), which followed
/// `parent_session_id` up to the agent with no parent. Here the parent is
/// derived from the path ([`parent_path_of`]): climb until a node has no parent
/// segment, or until the next parent is not present in the registry.
///
/// Returns the root agent's path, or `None` if `agent_path` itself is not in the
/// registry.
pub fn root_session(registry: &AgentRegistry, agent_path: &str) -> Option<String> {
    // The starting node must exist (matches legacy loading the session first).
    registry.get(agent_path)?;
    let mut current = agent_path.to_string();
    loop {
        let Some(parent) = parent_path_of(&current) else {
            return Some(current);
        };
        // Stop if the parent is not a live agent (defensive against detached
        // subtrees); the highest live ancestor is the root.
        if registry.get(parent).is_none() {
            return Some(current);
        }
        current = parent.to_string();
    }
}

#[cfg(test)]
mod tree_unit_tests {
    use super::*;

    fn rec(path: &str, depth: i32, status: AgentStatus) -> AgentRecord {
        AgentRecord {
            agent_path: path.to_string(),
            agent_id: format!("id{}", path),
            nickname: None,
            role: None,
            status,
            depth,
        }
    }

    /// Build:
    ///   /root (0)
    ///     /root/alpha (1)
    ///       /root/alpha/leaf (2)  nickname=worker
    ///     /root/beta (1)
    fn registry() -> AgentRegistry {
        let r = AgentRegistry::new();
        r.register(rec("/root", 0, AgentStatus::Running));
        r.register(rec("/root/alpha", 1, AgentStatus::Running));
        r.register(rec("/root/beta", 1, AgentStatus::Running));
        let mut leaf = rec("/root/alpha/leaf", 2, AgentStatus::Running);
        leaf.nickname = Some("worker".to_string());
        r.register(leaf);
        r
    }

    #[test]
    fn direct_child_detection() {
        assert!(is_direct_child("/root", "/root/a"));
        assert!(!is_direct_child("/root", "/root/a/b"));
        assert!(!is_direct_child("/root", "/root"));
        assert!(!is_direct_child("/root", "/rootx"));
        assert!(!is_direct_child("/root", "/other/a"));
    }

    #[test]
    fn parent_path_walks_one_segment() {
        assert_eq!(parent_path_of("/root/alpha/leaf"), Some("/root/alpha"));
        assert_eq!(parent_path_of("/root/alpha"), Some("/root"));
        assert_eq!(parent_path_of("/root"), None);
    }

    #[test]
    fn collect_tree_is_preorder_and_depth_annotated() {
        let r = registry();
        let tree = collect_agent_tree(&r, "/root");
        let shape: Vec<(&str, usize)> = tree.iter().map(|n| (n.agent_path(), n.depth)).collect();
        assert_eq!(
            shape,
            vec![
                ("/root", 0),
                ("/root/alpha", 1),
                ("/root/alpha/leaf", 2),
                ("/root/beta", 1),
            ]
        );
    }

    #[test]
    fn collect_tree_from_subtree_root_reparents_depth() {
        let r = registry();
        let tree = collect_agent_tree(&r, "/root/alpha");
        let paths: Vec<&str> = tree.iter().map(|n| n.agent_path()).collect();
        assert_eq!(paths, vec!["/root/alpha", "/root/alpha/leaf"]);
        assert_eq!(tree[0].depth, 0);
        assert_eq!(tree[1].depth, 1);
    }

    #[test]
    fn collect_tree_unknown_root_is_empty() {
        let r = registry();
        assert!(collect_agent_tree(&r, "/nope").is_empty());
    }

    #[test]
    fn canonical_reference_forms() {
        assert_eq!(
            canonical_agent_reference("  @alpha ", "/root"),
            "/root/alpha"
        );
        assert_eq!(
            canonical_agent_reference("/root/beta", "/root/alpha"),
            "/root/beta"
        );
        assert_eq!(canonical_agent_reference(".", "/root/alpha"), "/root/alpha");
        assert_eq!(
            canonical_agent_reference("self", "/root/alpha"),
            "/root/alpha"
        );
        assert_eq!(canonical_agent_reference("..", "/root/alpha"), "/root");
        assert_eq!(canonical_agent_reference("..", "/root"), ROOT_AGENT_PATH);
        assert_eq!(canonical_agent_reference("", "/root/alpha"), "/root/alpha");
    }

    #[test]
    fn resolve_relative_mention_from_root() {
        let r = registry();
        let got = resolve_agent_reference_in_tree(&r, "/root", "@alpha").unwrap();
        assert_eq!(got.agent_path, "/root/alpha");
    }

    #[test]
    fn resolve_absolute_path() {
        let r = registry();
        let got = resolve_agent_reference_in_tree(&r, "/root/beta", "/root/alpha/leaf").unwrap();
        assert_eq!(got.agent_path, "/root/alpha/leaf");
    }

    #[test]
    fn resolve_self_and_parent() {
        let r = registry();
        let me = resolve_agent_reference_in_tree(&r, "/root/alpha", "self").unwrap();
        assert_eq!(me.agent_path, "/root/alpha");
        let parent = resolve_agent_reference_in_tree(&r, "/root/alpha", "..").unwrap();
        assert_eq!(parent.agent_path, "/root");
    }

    #[test]
    fn resolve_root_itself() {
        let r = registry();
        // From a deep node, an absolute reference to the root resolves to root.
        let got = resolve_agent_reference_in_tree(&r, "/root/alpha/leaf", "/root").unwrap();
        assert_eq!(got.agent_path, "/root");
    }

    #[test]
    fn resolve_nickname_fallback() {
        let r = registry();
        let got = resolve_agent_reference_in_tree(&r, "/root", "worker").unwrap();
        assert_eq!(got.agent_path, "/root/alpha/leaf");
    }

    #[test]
    fn resolve_unknown_is_none() {
        let r = registry();
        assert!(resolve_agent_reference_in_tree(&r, "/root", "@ghost").is_none());
    }

    #[test]
    fn root_session_walks_to_top() {
        let r = registry();
        assert_eq!(
            root_session(&r, "/root/alpha/leaf").as_deref(),
            Some("/root")
        );
        assert_eq!(root_session(&r, "/root").as_deref(), Some("/root"));
    }

    #[test]
    fn root_session_unknown_is_none() {
        let r = registry();
        assert!(root_session(&r, "/missing").is_none());
    }

    #[test]
    fn root_session_stops_at_highest_live_ancestor() {
        // Detached subtree: /root/alpha/leaf exists but /root/alpha does not.
        let r = AgentRegistry::new();
        r.register(rec("/root/alpha/leaf", 2, AgentStatus::Running));
        assert_eq!(
            root_session(&r, "/root/alpha/leaf").as_deref(),
            Some("/root/alpha/leaf")
        );
    }
}
