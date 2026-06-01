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
//! This crate maps the supported role TOML keys into the small in-process
//! [`AgentConfigLayer`] below. The resolution order, built-in set,
//! user-override-wins rule, and provider/tier-preservation rule match Codex.

use std::collections::BTreeMap;
use std::path::PathBuf;

use browser_use_providers::{bundled_model_catalog, ModelCatalog, ModelPresetInfo};

/// The role name used when a caller omits `agent_type`
/// (codex `agent/role.rs:29` `DEFAULT_ROLE_NAME = "default"`).
pub const DEFAULT_ROLE_NAME: &str = "default";

const AGENT_NAMES: &str = r#"Euclid
Archimedes
Ptolemy
Hypatia
Avicenna
Averroes
Aquinas
Copernicus
Kepler
Galileo
Bacon
Descartes
Pascal
Fermat
Huygens
Leibniz
Newton
Halley
Euler
Lagrange
Laplace
Volta
Gauss
Ampere
Faraday
Darwin
Lovelace
Boole
Pasteur
Maxwell
Mendel
Curie
Planck
Tesla
Poincare
Noether
Hilbert
Einstein
Raman
Bohr
Turing
Hubble
Feynman
Franklin
McClintock
Meitner
Herschel
Linnaeus
Wegener
Chandrasekhar
Sagan
Goodall
Carson
Carver
Socrates
Plato
Aristotle
Epicurus
Cicero
Confucius
Mencius
Zeno
Locke
Hume
Kant
Hegel
Kierkegaard
Mill
Nietzsche
Peirce
James
Dewey
Russell
Popper
Sartre
Beauvoir
Arendt
Rawls
Singer
Anscombe
Parfit
Kuhn
Boyle
Hooke
Harvey
Dalton
Ohm
Helmholtz
Gibbs
Lorentz
Schrodinger
Heisenberg
Pauli
Dirac
Bernoulli
Godel
Nash
Banach
Ramanujan
Erdos
Jason"#;

pub fn default_agent_nickname_candidates() -> Vec<String> {
    AGENT_NAMES
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// A role declaration (codex `config/mod.rs:1890-1898` `AgentRoleConfig`).
///
/// `config_file` mirrors codex's pointer to the role's TOML overrides. Runtime
/// config loading resolves supported role TOML keys into [`RoleOverrides`] so
/// the effective layer can be applied at spawn time.
#[derive(Clone, Debug, Default, PartialEq)]
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
#[derive(Clone, Debug, Default, PartialEq)]
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
    /// Raw role config-layer overrides. Codex applies the role TOML through the
    /// normal config stack; this carries fields beyond the small set projected
    /// above so child CLI/TUI runs can apply them too.
    pub config_overrides: Vec<(String, toml::Value)>,
}

impl RoleOverrides {
    pub fn merge(&mut self, other: RoleOverrides) {
        if other.model.is_some() {
            self.model = other.model;
        }
        if other.reasoning_effort.is_some() {
            self.reasoning_effort = other.reasoning_effort;
        }
        if other.instructions.is_some() {
            self.instructions = other.instructions;
        }
        if other.tool_allowlist.is_some() {
            self.tool_allowlist = other.tool_allowlist;
        }
        if other.can_write.is_some() {
            self.can_write = other.can_write;
        }
        if other.provider.is_some() {
            self.provider = other.provider;
        }
        if other.service_tier.is_some() {
            self.service_tier = other.service_tier;
        }
        self.config_overrides.extend(other.config_overrides);
    }
}

/// The child config values a role layers onto. The fields are exactly the
/// surface a role touches in codex: model/reasoning, instructions, tools,
/// permissions, and the provider/tier the role may or may not override.
#[derive(Clone, Debug, PartialEq)]
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
    /// Raw role config overrides to pass into the child run.
    pub config_overrides: Vec<(String, toml::Value)>,
    /// The model catalog/list available to spawn override validation.
    pub model_catalog: Option<ModelCatalog>,
    pub available_models: Vec<ModelPresetInfo>,
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
            config_overrides: Vec::new(),
            model_catalog: None,
            available_models: bundled_model_catalog().presets(true),
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

    pub fn with_user_defined(user_defined: BTreeMap<String, AgentRoleConfig>) -> Self {
        Self { user_defined }
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
    if !overrides.config_overrides.is_empty() {
        config
            .config_overrides
            .extend(overrides.config_overrides.clone());
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
            description: Some(r#"Use `explorer` for specific codebase questions.
Explorers are fast and authoritative.
They must be used to ask specific, well-scoped questions on the codebase.
Rules:
- In order to avoid redundant work, you should avoid exploring the same problem that explorers have already covered. Typically, you should trust the explorer results without additional verification. You are still allowed to inspect the code yourself to gain the needed context!
- You are encouraged to spawn up multiple explorers in parallel when you have multiple distinct questions to ask about the codebase that can be answered independently. This allows you to get more information faster without waiting for one question to finish before asking the next. While waiting for the explorer results, you can continue working on other local tasks that do not depend on those results. This parallelism is a key advantage of delegation, so use it whenever you have multiple questions to ask.
- Reuse existing explorers for related questions."#.to_string()),
            config_file: Some(PathBuf::from("explorer.toml")),
            nickname_candidates: None,
            overrides: RoleOverrides::default(),
        },
    );

    roles.insert(
        "worker".to_string(),
        AgentRoleConfig {
            description: Some(r#"Use for execution and production work.
Typical tasks:
- Implement part of a feature
- Fix tests or bugs
- Split large refactors into independent chunks
Rules:
- Explicitly assign **ownership** of the task (files / responsibility). When the subtask involves code changes, you should clearly specify which files or modules the worker is responsible for. This helps avoid merge conflicts and ensures accountability. For example, you can say "Worker 1 is responsible for updating the authentication module, while Worker 2 will handle the database layer." By defining clear ownership, you can delegate more effectively and reduce coordination overhead.
- Always tell workers they are **not alone in the codebase**, and they should not revert the edits made by others, and they should adjust their implementation to accommodate the changes made by others. This is important because there may be multiple workers making changes in parallel, and they need to be aware of each other's work to avoid conflicts and ensure a cohesive final product."#.to_string()),
            config_file: None,
            nickname_candidates: None,
            overrides: RoleOverrides::default(),
        },
    );

    roles
}
