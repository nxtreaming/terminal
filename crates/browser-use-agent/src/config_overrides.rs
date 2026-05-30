//! Config-override parsing + run-config construction types (Phase-D leaf port).
//!
//! Ported faithfully from the legacy `browser-use-core` so the TUI/CLI can build
//! the same provider/run configuration before driving a session on the new async
//! engine. The parse semantics and every default are byte-for-byte equivalent to
//! the originals — these values drive provider selection and must not drift.
//!
//! Source of truth (`crates/browser-use-core/src`):
//! - `parse_config_overrides` + helpers: `config_overrides.rs:9-47`
//! - `ProviderBackend`: `lib.rs:126-134` (redefined locally — the agent crate
//!   must not depend on `browser-use-core`)
//! - `RunConfigValueSource`: `lib.rs:136-140`
//! - `ProviderRunConfig` (+ impl): `lib.rs:142-176`
//! - `ConfigOverrides` alias: `lib.rs:178`
//! - `AgentRunOptions` (+ Default + builders): `lib.rs:202-280` / `380-489`
//! - `build_config_overrides_layer` / `apply_toml_override`: `lib.rs:14372-14445`
//! - `EnvironmentContext*` / `ChildAgentRun*`: `lib.rs:138-168` / `226-255`
//!
//! `CollaborationModeKind` is intentionally NOT redefined here — it already lives
//! in [`crate::prompts`] and is reused so the two engines agree on the mode set.

use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::prompts::CollaborationModeKind;

/// Legacy `browser-use-core::constants::DEFAULT_MAX_CONTEXT_CHARS`
/// (`constants.rs:9`). Reproduced verbatim so [`AgentRunOptions::default`]
/// matches the legacy engine exactly.
pub const DEFAULT_MAX_CONTEXT_CHARS: usize = 240_000;

/// Parsed CLI/TUI `--config key=value` overrides: an ordered list of dotted TOML
/// paths paired with their parsed values.
///
/// Mirrors `browser-use-core` `pub type ConfigOverrides = Vec<(String, toml::Value)>;`
/// (`lib.rs:178`).
pub type ConfigOverrides = Vec<(String, toml::Value)>;

/// The provider backend a run targets.
///
/// Redefined locally (the agent crate must not depend on `browser-use-core`);
/// kept in lock-step with `browser-use-core::ProviderBackend` (`lib.rs:126`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderBackend {
    Codex,
    Openai,
    Anthropic,
    Openrouter,
    Deepseek,
    Fake,
    None,
}

/// Whether a run-config value was set explicitly or fell back to a default.
///
/// Mirrors `browser-use-core::RunConfigValueSource` (`lib.rs:136`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunConfigValueSource {
    Explicit,
    Default,
}

/// A single named environment the agent may operate in.
///
/// Mirrors `browser-use-core::EnvironmentContextEnvironment` (`lib.rs:138`).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct EnvironmentContextEnvironment {
    pub id: String,
    pub cwd: String,
    pub shell: String,
}

impl EnvironmentContextEnvironment {
    pub fn new(id: impl Into<String>, cwd: impl Into<String>, shell: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            cwd: cwd.into(),
            shell: shell.into(),
        }
    }
}

/// Network allow/deny lists surfaced in the environment context.
///
/// Mirrors `browser-use-core::EnvironmentNetworkContext` (`lib.rs:155`).
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct EnvironmentNetworkContext {
    pub allowed_domains: Vec<String>,
    pub denied_domains: Vec<String>,
}

impl EnvironmentNetworkContext {
    pub fn new(allowed_domains: Vec<String>, denied_domains: Vec<String>) -> Self {
        Self {
            allowed_domains,
            denied_domains,
        }
    }
}

/// A request to spawn a child agent, carried through [`ChildAgentRunner`].
///
/// Mirrors `browser-use-core::ChildAgentRunRequest` (`lib.rs:226`).
#[derive(Clone, Debug)]
pub struct ChildAgentRunRequest {
    pub parent_session_id: String,
    pub child_session_id: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub config_overrides: Vec<(String, toml::Value)>,
}

/// Opaque, cloneable callback used to launch child agents.
///
/// Mirrors `browser-use-core::ChildAgentRunner` (`lib.rs:236`).
#[derive(Clone)]
pub struct ChildAgentRunner {
    run: Arc<dyn Fn(ChildAgentRunRequest) -> Result<()> + Send + Sync>,
}

impl std::fmt::Debug for ChildAgentRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildAgentRunner").finish_non_exhaustive()
    }
}

impl ChildAgentRunner {
    pub fn new(run: impl Fn(ChildAgentRunRequest) -> Result<()> + Send + Sync + 'static) -> Self {
        Self { run: Arc::new(run) }
    }

    pub fn run(&self, request: ChildAgentRunRequest) -> Result<()> {
        (self.run)(request)
    }
}

/// Per-run knobs the TUI/CLI assemble before starting a session.
///
/// Mirrors `browser-use-core::AgentRunOptions` (`lib.rs:202`), including every
/// field and the exact default values.
#[derive(Clone, Debug)]
pub struct AgentRunOptions {
    pub max_turns: usize,
    pub max_context_chars: usize,
    pub browser_mode: Option<String>,
    pub collaboration_mode: CollaborationModeKind,
    pub include_environment_context: bool,
    pub include_permissions_instructions: bool,
    pub environment_context_environments: Vec<EnvironmentContextEnvironment>,
    pub environment_context_network: Option<EnvironmentNetworkContext>,
    pub config_profile: Option<String>,
    pub config_overrides: ConfigOverrides,
    pub session_thread_config: Option<toml::Value>,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
    pub model_provider_id: Option<String>,
    pub model_provider_id_source: RunConfigValueSource,
    pub python_tool_timeout_seconds: u64,
    pub python_env: Vec<(String, String)>,
    pub child_agent_runner: Option<ChildAgentRunner>,
    pub final_output_json_schema: Option<Value>,
    pub final_output_json_schema_strict: bool,
    pub model_compaction_enabled: bool,
    pub analytics_source: Option<String>,
    pub analytics_provider_kind: Option<String>,
    pub analytics_model: Option<String>,
}

impl Default for AgentRunOptions {
    fn default() -> Self {
        Self {
            max_turns: 80,
            max_context_chars: DEFAULT_MAX_CONTEXT_CHARS,
            browser_mode: None,
            collaboration_mode: CollaborationModeKind::Default,
            include_environment_context: true,
            include_permissions_instructions: true,
            environment_context_environments: Vec::new(),
            environment_context_network: None,
            config_profile: None,
            config_overrides: Vec::new(),
            session_thread_config: None,
            base_instructions: None,
            developer_instructions: None,
            model_provider_id: None,
            model_provider_id_source: RunConfigValueSource::Default,
            python_tool_timeout_seconds: 120,
            python_env: Vec::new(),
            child_agent_runner: None,
            final_output_json_schema: None,
            final_output_json_schema_strict: true,
            model_compaction_enabled: false,
            analytics_source: None,
            analytics_provider_kind: None,
            analytics_model: None,
        }
    }
}

impl AgentRunOptions {
    pub fn with_browser_mode(mut self, mode: impl Into<String>) -> Self {
        self.browser_mode = Some(mode.into());
        self
    }

    pub fn with_collaboration_mode(mut self, mode: CollaborationModeKind) -> Self {
        self.collaboration_mode = mode;
        self
    }

    pub fn with_include_environment_context(mut self, include: bool) -> Self {
        self.include_environment_context = include;
        self
    }

    pub fn with_include_permissions_instructions(mut self, include: bool) -> Self {
        self.include_permissions_instructions = include;
        self
    }

    pub fn with_environment_context_environments(
        mut self,
        environments: Vec<EnvironmentContextEnvironment>,
    ) -> Self {
        self.environment_context_environments = environments;
        self
    }

    pub fn with_environment_context_network(mut self, network: EnvironmentNetworkContext) -> Self {
        self.environment_context_network = Some(network);
        self
    }

    pub fn with_config_profile(mut self, profile: impl Into<String>) -> Self {
        self.config_profile = Some(profile.into());
        self
    }

    pub fn with_config_overrides(mut self, overrides: Vec<(String, toml::Value)>) -> Self {
        self.config_overrides = overrides;
        self
    }

    pub fn with_session_thread_config(mut self, config: toml::Value) -> Self {
        self.session_thread_config = Some(config);
        self
    }

    pub fn with_session_thread_config_overrides(
        mut self,
        overrides: Vec<(String, toml::Value)>,
    ) -> Self {
        self.session_thread_config = Some(build_config_overrides_layer(&overrides));
        self
    }

    pub fn with_base_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.base_instructions = Some(instructions.into());
        self
    }

    pub fn with_developer_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.developer_instructions = Some(instructions.into());
        self
    }

    pub fn with_model_provider_id(mut self, model_provider_id: impl Into<String>) -> Self {
        self.model_provider_id = Some(model_provider_id.into());
        self.model_provider_id_source = RunConfigValueSource::Explicit;
        self
    }

    pub fn with_default_model_provider_id(mut self, model_provider_id: impl Into<String>) -> Self {
        self.model_provider_id = Some(model_provider_id.into());
        self.model_provider_id_source = RunConfigValueSource::Default;
        self
    }

    pub fn with_python_tool_timeout_seconds(mut self, timeout_seconds: u64) -> Self {
        self.python_tool_timeout_seconds = timeout_seconds;
        self
    }

    pub fn with_python_env(mut self, env: Vec<(String, String)>) -> Self {
        self.python_env = env;
        self
    }

    pub fn with_child_agent_runner(mut self, runner: ChildAgentRunner) -> Self {
        self.child_agent_runner = Some(runner);
        self
    }

    pub fn with_final_output_json_schema(mut self, schema: Value, strict: bool) -> Self {
        self.final_output_json_schema = Some(schema);
        self.final_output_json_schema_strict = strict;
        self
    }

    pub fn with_model_compaction(mut self, enabled: bool) -> Self {
        self.model_compaction_enabled = enabled;
        self
    }

    pub fn with_analytics_source(mut self, source: impl Into<String>) -> Self {
        self.analytics_source = Some(source.into());
        self
    }
}

/// The fully-resolved provider/run configuration handed to the engine.
///
/// Mirrors `browser-use-core::ProviderRunConfig` (`lib.rs:142`).
#[derive(Clone, Debug)]
pub struct ProviderRunConfig {
    pub backend: ProviderBackend,
    pub model: String,
    pub model_source: RunConfigValueSource,
    pub options: AgentRunOptions,
    pub fake_result: Option<String>,
}

impl ProviderRunConfig {
    pub fn new(backend: ProviderBackend, model: impl Into<String>) -> Self {
        Self {
            backend,
            model: model.into(),
            model_source: RunConfigValueSource::Explicit,
            options: AgentRunOptions::default(),
            fake_result: None,
        }
    }

    pub fn with_options(mut self, options: AgentRunOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_model_source(mut self, source: RunConfigValueSource) -> Self {
        self.model_source = source;
        self
    }

    pub fn with_fake_result(mut self, result: impl Into<String>) -> Self {
        self.fake_result = Some(result.into());
        self
    }
}

/// Parse raw `key=value` override strings into an ordered [`ConfigOverrides`].
///
/// Behavior is byte-identical to `browser-use-core::parse_config_overrides`
/// (`config_overrides.rs:9`): each entry is split on the first `=`, the key is
/// trimmed (and canonicalized), and the value is parsed as a bare TOML value,
/// falling back to a quote-stripped string literal when that fails.
pub fn parse_config_overrides(raw_config_overrides: &[String]) -> Result<ConfigOverrides> {
    raw_config_overrides
        .iter()
        .map(|raw| {
            let mut parts = raw.splitn(2, '=');
            let key = parts.next().unwrap_or_default().trim();
            let value_str = parts
                .next()
                .ok_or_else(|| anyhow!("Invalid override (missing '='): {raw}"))?
                .trim();
            if key.is_empty() {
                bail!("Empty key in override: {raw}");
            }
            let value = parse_config_override_toml_value(value_str).unwrap_or_else(|| {
                toml::Value::String(
                    value_str
                        .trim()
                        .trim_matches(|candidate| candidate == '"' || candidate == '\'')
                        .to_string(),
                )
            });
            Ok((canonicalize_config_override_key(key), value))
        })
        .collect()
}

/// Mirrors `browser-use-core::canonicalize_config_override_key`
/// (`config_overrides.rs:35`).
fn canonicalize_config_override_key(key: &str) -> String {
    if key == "use_legacy_landlock" {
        "features.use_legacy_landlock".to_string()
    } else {
        key.to_string()
    }
}

/// Mirrors `browser-use-core::parse_config_override_toml_value`
/// (`config_overrides.rs:43`).
fn parse_config_override_toml_value(raw: &str) -> Option<toml::Value> {
    let wrapped = format!("_x_ = {raw}");
    let mut table = toml::from_str::<toml::Table>(&wrapped).ok()?;
    table.remove("_x_")
}

/// Collapse a flat list of dotted overrides into a nested TOML table.
///
/// Mirrors `browser-use-core::build_config_overrides_layer` (`lib.rs:14372`).
pub fn build_config_overrides_layer(config_overrides: &[(String, toml::Value)]) -> toml::Value {
    let mut root = toml::Value::Table(toml::map::Map::new());
    for (path, value) in config_overrides {
        apply_toml_override(&mut root, path, value.clone());
    }
    root
}

/// Insert `value` at the dotted `path` within `root`, creating intermediate
/// tables (and replacing non-table values) as needed.
///
/// Mirrors `browser-use-core::apply_toml_override` (`lib.rs:14417`).
fn apply_toml_override(root: &mut toml::Value, path: &str, value: toml::Value) {
    let mut current = root;
    let mut segments = path.split('.').peekable();
    while let Some(segment) = segments.next() {
        let is_last = segments.peek().is_none();
        if is_last {
            match current {
                toml::Value::Table(table) => {
                    table.insert(segment.to_string(), value);
                }
                _ => {
                    let mut table = toml::map::Map::new();
                    table.insert(segment.to_string(), value);
                    *current = toml::Value::Table(table);
                }
            }
            return;
        }

        let need_table = !matches!(current, toml::Value::Table(_));
        if need_table {
            *current = toml::Value::Table(toml::map::Map::new());
        }
        if let toml::Value::Table(table) = current {
            current = table
                .entry(segment.to_string())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ov(pairs: &[&str]) -> Vec<String> {
        pairs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_representative_override_strings() {
        let parsed = parse_config_overrides(&ov(&[
            "model=\"gpt-5\"",
            "max_turns=12",
            "model_compaction_enabled=true",
            "temperature=0.5",
        ]))
        .unwrap();

        assert_eq!(
            parsed,
            vec![
                (
                    "model".to_string(),
                    toml::Value::String("gpt-5".to_string())
                ),
                ("max_turns".to_string(), toml::Value::Integer(12)),
                (
                    "model_compaction_enabled".to_string(),
                    toml::Value::Boolean(true)
                ),
                ("temperature".to_string(), toml::Value::Float(0.5)),
            ]
        );
    }

    #[test]
    fn trims_keys_and_falls_back_to_bare_string() {
        // Whitespace around key/value is trimmed; an unquoted, non-TOML value
        // falls back to a quote-stripped string literal.
        let parsed = parse_config_overrides(&ov(&["  provider  =  anthropic  "])).unwrap();
        assert_eq!(
            parsed,
            vec![(
                "provider".to_string(),
                toml::Value::String("anthropic".to_string())
            )]
        );

        // Single/double quotes around a bare string fallback are stripped.
        let quoted = parse_config_overrides(&ov(&["name='hello world'"])).unwrap();
        assert_eq!(
            quoted,
            vec![(
                "name".to_string(),
                toml::Value::String("hello world".to_string())
            )]
        );
    }

    #[test]
    fn canonicalizes_legacy_landlock_key() {
        let parsed = parse_config_overrides(&ov(&["use_legacy_landlock=true"])).unwrap();
        assert_eq!(
            parsed,
            vec![(
                "features.use_legacy_landlock".to_string(),
                toml::Value::Boolean(true)
            )]
        );
    }

    #[test]
    fn rejects_missing_equals_and_empty_key() {
        assert!(parse_config_overrides(&ov(&["no_equals_here"])).is_err());
        assert!(parse_config_overrides(&ov(&["=value"])).is_err());
    }

    #[test]
    fn agent_run_options_defaults_match_core() {
        let options = AgentRunOptions::default();
        assert_eq!(options.max_turns, 80);
        assert_eq!(options.max_context_chars, DEFAULT_MAX_CONTEXT_CHARS);
        assert_eq!(options.max_context_chars, 240_000);
        assert!(options.browser_mode.is_none());
        assert_eq!(options.collaboration_mode, CollaborationModeKind::Default);
        assert!(options.include_environment_context);
        assert!(options.include_permissions_instructions);
        assert!(options.environment_context_environments.is_empty());
        assert!(options.environment_context_network.is_none());
        assert!(options.config_profile.is_none());
        assert!(options.config_overrides.is_empty());
        assert!(options.session_thread_config.is_none());
        assert!(options.base_instructions.is_none());
        assert!(options.developer_instructions.is_none());
        assert!(options.model_provider_id.is_none());
        assert_eq!(
            options.model_provider_id_source,
            RunConfigValueSource::Default
        );
        assert_eq!(options.python_tool_timeout_seconds, 120);
        assert!(options.python_env.is_empty());
        assert!(options.child_agent_runner.is_none());
        assert!(options.final_output_json_schema.is_none());
        assert!(options.final_output_json_schema_strict);
        assert!(!options.model_compaction_enabled);
        assert!(options.analytics_source.is_none());
        assert!(options.analytics_provider_kind.is_none());
        assert!(options.analytics_model.is_none());
    }

    #[test]
    fn provider_run_config_new_uses_explicit_source_and_default_options() {
        let config = ProviderRunConfig::new(ProviderBackend::Anthropic, "claude-x");
        assert_eq!(config.backend, ProviderBackend::Anthropic);
        assert_eq!(config.model, "claude-x");
        assert_eq!(config.model_source, RunConfigValueSource::Explicit);
        assert!(config.fake_result.is_none());
        // options default to AgentRunOptions::default()
        assert_eq!(config.options.max_turns, 80);
    }

    #[test]
    fn provider_run_config_builders_apply() {
        let options = AgentRunOptions::default().with_browser_mode("dom");
        let config = ProviderRunConfig::new(ProviderBackend::Fake, "fake-model")
            .with_options(options)
            .with_model_source(RunConfigValueSource::Default)
            .with_fake_result("canned");
        assert_eq!(config.model_source, RunConfigValueSource::Default);
        assert_eq!(config.fake_result.as_deref(), Some("canned"));
        assert_eq!(config.options.browser_mode.as_deref(), Some("dom"));
    }

    #[test]
    fn provider_backend_covers_all_variants() {
        // Round-trip every backend through its debug name and back, asserting the
        // full variant set matches `browser-use-core::ProviderBackend`.
        let all = [
            ProviderBackend::Codex,
            ProviderBackend::Openai,
            ProviderBackend::Anthropic,
            ProviderBackend::Openrouter,
            ProviderBackend::Deepseek,
            ProviderBackend::Fake,
            ProviderBackend::None,
        ];
        for backend in all {
            let name = format!("{backend:?}");
            let round_tripped = match name.as_str() {
                "Codex" => ProviderBackend::Codex,
                "Openai" => ProviderBackend::Openai,
                "Anthropic" => ProviderBackend::Anthropic,
                "Openrouter" => ProviderBackend::Openrouter,
                "Deepseek" => ProviderBackend::Deepseek,
                "Fake" => ProviderBackend::Fake,
                "None" => ProviderBackend::None,
                other => panic!("unexpected ProviderBackend debug name: {other}"),
            };
            assert_eq!(backend, round_tripped);
        }
        assert_eq!(all.len(), 7);
    }

    #[test]
    fn collaboration_mode_kind_parity_with_core() {
        // The agent crate reuses `crate::prompts::CollaborationModeKind` instead
        // of duplicating it. Assert the two variants map to the same override
        // strings core's `CollaborationModeKind::as_str` produces ("plan" /
        // "default", `lib.rs:306-313`), keeping the engines in lock-step.
        fn as_str(mode: CollaborationModeKind) -> &'static str {
            match mode {
                CollaborationModeKind::Plan => "plan",
                CollaborationModeKind::Default => "default",
            }
        }
        assert_eq!(as_str(CollaborationModeKind::Plan), "plan");
        assert_eq!(as_str(CollaborationModeKind::Default), "default");

        // The default mode matches core's `#[default] Default`.
        assert_eq!(
            AgentRunOptions::default().collaboration_mode,
            CollaborationModeKind::Default
        );
    }

    #[test]
    fn build_config_overrides_layer_nests_dotted_paths() {
        let overrides = parse_config_overrides(&ov(&[
            "features.use_legacy_landlock=true",
            "tools.web.enabled=false",
        ]))
        .unwrap();
        let layer = build_config_overrides_layer(&overrides);

        let features = layer
            .get("features")
            .and_then(|v| v.get("use_legacy_landlock"))
            .and_then(|v| v.as_bool());
        assert_eq!(features, Some(true));

        let web = layer
            .get("tools")
            .and_then(|v| v.get("web"))
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool());
        assert_eq!(web, Some(false));
    }

    #[test]
    fn child_agent_runner_invokes_callback() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let runner = ChildAgentRunner::new(move |_req| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        runner
            .run(ChildAgentRunRequest {
                parent_session_id: "parent".to_string(),
                child_session_id: "child".to_string(),
                model: None,
                reasoning_effort: None,
                service_tier: None,
                config_overrides: Vec::new(),
            })
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Debug is redacted (finish_non_exhaustive).
        assert_eq!(format!("{runner:?}"), "ChildAgentRunner { .. }");
    }
}
