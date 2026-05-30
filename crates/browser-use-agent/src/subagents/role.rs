//! Agent roles as a config layer (codex `agent/role.rs` parity).
//!
//! A *role* (codex "agent_type") is a named bundle of overrides that a parent
//! applies to a freshly-spawned child's config: a system-instructions prefix, a
//! model/reasoning lock, a tool allow-list, permission tweaks, and a nickname
//! pool. Codex resolves a role by name (user-defined first, then built-in) and
//! inserts it as a high-precedence config *layer* that nonetheless **preserves**
//! the caller's current provider and service tier unless the role layer sets
//! them explicitly.
//!
//! Parity:
//! - Role config struct: `core/src/config/mod.rs:1890-1898`
//!   `AgentRoleConfig { description: Option<String>, config_file:
//!   Option<PathBuf>, nickname_candidates: Option<Vec<String>> }`.
//! - Resolution + apply: `core/src/agent/role.rs:38-83`
//!   `apply_role_to_config(config, role_name)` resolves user-defined first
//!   (`resolve_role_config` :119-127), then built-in; inserts the role as a
//!   config layer; preserves parent `model_provider`/`service_tier` unless the
//!   role sets them (`apply_role_to_config_inner` :72-81,
//!   `reload_overrides` :201-214).
//! - Built-ins `default`/`explorer`/`worker`: `core/src/agent/role.rs:305-348`.
//!
//! Codex backs roles with real TOML files merged through a `ConfigLayerStack`.
//! That machinery (`codex_config`, `LOCAL_FS`, on-disk role files) is out of
//! scope for this crate, so the layer a role mutates is the small, in-process
//! [`AgentConfigLayer`] below — NOT the crate's `Config`/`AgentConfig`. The
//! resolution order, the built-in set, the user-override-wins rule, and the
//! provider/tier-preservation rule match codex exactly.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// The role name used when a caller omits `agent_type`
/// (codex `agent/role.rs:29` `DEFAULT_ROLE_NAME = "default"`).
pub const DEFAULT_ROLE_NAME: &str = "default";

/// A role declaration (codex `config/mod.rs:1890-1898` `AgentRoleConfig`).
///
/// `config_file` mirrors codex's pointer to the role's TOML overrides; here it
/// is carried for parity (and to drive [`RoleRegistry`] resolution) but the
/// actual override payload is expressed inline via [`RoleOverrides`] so the
/// layer can be applied without a filesystem.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentRoleConfig {
    /// Human-readable description shown in the spawn tool spec.
    pub description: Option<String>,
    /// Path to the role's config-overrides TOML (codex parity; informational
    /// here — the effective overrides are carried in [`AgentRoleConfig::overrides`]).
    pub config_file: Option<PathBuf>,
    /// Nickname pool this role draws from (codex parity).
    pub nickname_candidates: Option<Vec<String>>,
    /// The config overrides this role layers onto a base config. Codex stores
    /// these in the role's TOML; we carry them inline (see module docs).
    pub overrides: RoleOverrides,
}

/// The config-layer payload a role mutates onto a base [`AgentConfigLayer`].
///
/// Each `Some` field is an override the role sets; `None` leaves the base value
/// untouched. `provider`/`service_tier` are deliberately *sticky*: a `None` here
/// means "preserve the caller's current value" (codex
/// `preserve_current_provider`/`preserve_current_service_tier`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoleOverrides {
    /// Pin the child to a specific model.
    pub model: Option<String>,
    /// Pin the child's reasoning effort (free-form to avoid coupling to a model
    /// enum; matches codex's `model_reasoning_effort` string in role TOML).
    pub reasoning_effort: Option<String>,
    /// Role system-instructions appended/locked onto the child.
    pub instructions: Option<String>,
    /// Restrict the child to this tool allow-list (replaces the base list).
    pub tool_allowlist: Option<Vec<String>>,
    /// Whether the child may write to disk / run side-effecting tools.
    pub can_write: Option<bool>,
    /// Provider override. `None` => preserve caller's provider (codex sticky).
    pub provider: Option<String>,
    /// Service-tier override. `None` => preserve caller's tier (codex sticky).
    pub service_tier: Option<String>,
}

/// The minimal "config" a role layers onto — local to this module by design
/// (the task forbids depending on the crate's `Config`/`AgentConfig`).
///
/// This is the seam an integration WP would map onto the real agent config when
/// the spawned child is actually constructed. The fields are exactly the surface
/// a role touches in codex: model/reasoning, instructions, tools, permissions,
/// and the provider/tier the role may or may not override.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentConfigLayer {
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub instructions: String,
    pub tool_allowlist: Vec<String>,
    pub can_write: bool,
    /// Caller-owned, sticky unless the role overrides it.
    pub provider: String,
    /// Caller-owned, sticky unless the role overrides it.
    pub service_tier: Option<String>,
    /// The role this config was last layered with (for diagnostics / registry).
    pub role: Option<String>,
}

impl AgentConfigLayer {
    /// A neutral base config representing the parent's runtime choices that a
    /// role layer will be applied on top of.
    pub fn base(model: impl Into<String>, provider: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            reasoning_effort: None,
            instructions: String::new(),
            tool_allowlist: Vec::new(),
            can_write: true,
            provider: provider.into(),
            service_tier: None,
            role: None,
        }
    }
}

/// Registry of built-in and user-defined roles (codex `agent/role.rs`
/// `built_in::configs` + `Config::agent_roles`).
#[derive(Clone, Debug)]
pub struct RoleRegistry {
    /// User-defined roles, keyed by name. Resolved *before* built-ins (codex
    /// `resolve_role_config` :119-127), so a user role shadows a built-in of the
    /// same name.
    user_defined: BTreeMap<String, AgentRoleConfig>,
}

impl Default for RoleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RoleRegistry {
    /// A registry with only the built-in roles available.
    pub fn new() -> Self {
        Self {
            user_defined: BTreeMap::new(),
        }
    }

    /// Register (or replace) a user-defined role. A user role with the same name
    /// as a built-in overrides the built-in (codex `resolve_role_config`).
    pub fn register_user_role(&mut self, name: impl Into<String>, role: AgentRoleConfig) {
        self.user_defined.insert(name.into(), role);
    }

    /// The user-defined roles map (resolution order precedes built-ins).
    pub fn user_defined(&self) -> &BTreeMap<String, AgentRoleConfig> {
        &self.user_defined
    }

    /// Resolve a role by name: user-defined first, then built-in
    /// (codex `resolve_role_config` :119-127).
    pub fn resolve(&self, role_name: &str) -> Option<AgentRoleConfig> {
        self.user_defined
            .get(role_name)
            .cloned()
            .or_else(|| built_in_roles().get(role_name).cloned())
    }

    /// Whether `role_name` resolves to a user-defined (vs built-in) role.
    /// Mirrors codex's `is_built_in = !config.agent_roles.contains_key(name)`
    /// (`agent/role.rs:61`), inverted.
    pub fn is_user_defined(&self, role_name: &str) -> bool {
        self.user_defined.contains_key(role_name)
    }

    /// Apply the named role as a layer onto `config`, preserving the caller's
    /// provider/service_tier unless the role overrides them (codex
    /// `apply_role_to_config` :38-83).
    ///
    /// Returns the role's resolved [`AgentRoleConfig`] (so the caller can mint a
    /// nickname from its candidate pool). Errors with a codex-shaped message
    /// when the role name is unknown.
    pub fn apply_role_to_config(
        &self,
        config: &mut AgentConfigLayer,
        role_name: Option<&str>,
    ) -> Result<AgentRoleConfig, String> {
        let role_name = role_name.unwrap_or(DEFAULT_ROLE_NAME);
        let role = self
            .resolve(role_name)
            .ok_or_else(|| format!("unknown agent_type '{role_name}'"))?;

        apply_overrides(config, role_name, &role.overrides);
        Ok(role)
    }
}

/// Layer a role's overrides onto `config`. `provider`/`service_tier` are sticky:
/// only overwritten when the role explicitly sets them (codex
/// `apply_role_to_config_inner` :72-81 + `reload_overrides` :201-214).
fn apply_overrides(config: &mut AgentConfigLayer, role_name: &str, overrides: &RoleOverrides) {
    if let Some(model) = &overrides.model {
        config.model = model.clone();
    }
    if let Some(reasoning) = &overrides.reasoning_effort {
        config.reasoning_effort = Some(reasoning.clone());
    }
    if let Some(instructions) = &overrides.instructions {
        config.instructions = instructions.clone();
    }
    if let Some(tools) = &overrides.tool_allowlist {
        config.tool_allowlist = tools.clone();
    }
    if let Some(can_write) = overrides.can_write {
        config.can_write = can_write;
    }
    // Sticky caller-owned settings: preserve unless the role sets them.
    if let Some(provider) = &overrides.provider {
        config.provider = provider.clone();
    }
    if let Some(service_tier) = &overrides.service_tier {
        config.service_tier = Some(service_tier.clone());
    }
    config.role = Some(role_name.to_string());
}

/// The built-in role set (codex `agent/role.rs:305-348`:
/// `default`/`explorer`/`worker`; `awaiter` is temp-removed in codex too).
///
/// Built each call from constants — cheap and avoids a `LazyLock`/`OnceLock`
/// dependency in this small module; the set is tiny.
pub fn built_in_roles() -> BTreeMap<String, AgentRoleConfig> {
    let mut roles = BTreeMap::new();

    roles.insert(
        DEFAULT_ROLE_NAME.to_string(),
        AgentRoleConfig {
            description: Some("Default agent.".to_string()),
            config_file: None,
            nickname_candidates: None,
            overrides: RoleOverrides::default(),
        },
    );

    roles.insert(
        "explorer".to_string(),
        AgentRoleConfig {
            description: Some(
                "Use `explorer` for specific codebase questions. Explorers are fast and \
                 authoritative and must be used to ask specific, well-scoped questions on the \
                 codebase."
                    .to_string(),
            ),
            config_file: Some(PathBuf::from("explorer.toml")),
            nickname_candidates: Some(vec![
                "Ada".to_string(),
                "Lin".to_string(),
                "Hopper".to_string(),
                "Turing".to_string(),
            ]),
            overrides: RoleOverrides {
                instructions: Some(
                    "You are an explorer sub-agent: answer the specific question about the \
                     codebase concisely and authoritatively."
                        .to_string(),
                ),
                // Explorers are read-only investigators.
                can_write: Some(false),
                ..RoleOverrides::default()
            },
        },
    );

    roles.insert(
        "worker".to_string(),
        AgentRoleConfig {
            description: Some(
                "Use for execution and production work: implement part of a feature, fix tests \
                 or bugs, or split large refactors into independent chunks."
                    .to_string(),
            ),
            config_file: None,
            nickname_candidates: Some(vec![
                "Bolt".to_string(),
                "Forge".to_string(),
                "Mason".to_string(),
                "Smith".to_string(),
            ]),
            overrides: RoleOverrides {
                instructions: Some(
                    "You are a worker sub-agent: you own the assigned files/responsibility, you \
                     are not alone in the codebase, and you must not revert others' edits."
                        .to_string(),
                ),
                can_write: Some(true),
                ..RoleOverrides::default()
            },
        },
    );

    roles
}
