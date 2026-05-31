//! Config-layer model resolution ported from `browser-use-core`.
//!
//! Phase-E gap-fill. The earlier d-config leaf ([`crate::config_overrides`])
//! ported the override plumbing (`AgentRunOptions` / `ProviderRunConfig` /
//! `parse_config_overrides`) but deliberately skipped the AGENTS.md /
//! config-profile machinery. This module fills that gap so the tui/cli repoint
//! can ask, for a working directory, which model / provider-id to use and what
//! the model catalog looks like.
//!
//! ## Legacy provenance
//!
//! Source of truth: `terminal-decodex/crates/browser-use-core/src/lib.rs`.
//! The four public resolvers (lib.rs:1298 / 1317 / 1337 / 1373) all funnel
//! through `load_agents_md_config` (lib.rs:14202), which builds an
//! `AgentsMdConfig` (lib.rs:13780) by layering config sources and then read its
//! `.model` / `.model_provider_id` / `.model_catalog` fields:
//!
//! ```ignore
//! let config = load_agents_md_config(cwd, &mut warnings, profile, overrides)?;
//! // model_catalog_for_cwd_with_options:
//! Ok(config.model_catalog.unwrap_or_else(bundled_model_catalog))
//! // configured_model_for_cwd_with_options:
//! Ok(config.model.as_deref().map(str::trim).filter(non-empty).map(owned))
//! // configured_model_provider_id_for_cwd_with_options: same for model_provider_id
//! // default_model_for_cwd_with_options: config.model, else
//! //   default_model_for_catalog(catalog, chatgpt_mode).unwrap_or("gpt-5.5")
//! ```
//! The layering order in `load_agents_md_config_with_thread_config`
//! (lib.rs:14225) is: workspace config (AGENTS.md walk + per-dir config files up
//! to the `.git` workspace root) -> global config (`~/.browser-use-terminal`
//! config + selected profile) -> session thread config -> `--config` overrides,
//! with later layers overwriting earlier ones (last write wins).
//!
//! ## Faithful-minimal port (documented simplifications)
//!
//! The legacy `AgentsMdConfig` and its loader pull in the whole core engine
//! (skills, MCP, plugins, hooks, the ~30-field `ModelCatalogEntryInfo`, the
//! bundled `codex-models.json`, etc.), and the agent crate does **not** depend
//! on `browser-use-core` or `browser-use-providers`. So this port reconstructs
//! the resolution from the public contract rather than copying it wholesale:
//!
//! - **Catalog shape.** [`ModelCatalog`] / [`ModelCatalogEntry`] are a minimal
//!   local mirror carrying only the resolution-relevant fields (slug,
//!   display_name, is_default) plus the bundled fallback default — enough to
//!   reproduce the observable "which model is the default for this cwd"
//!   behavior. The full upstream `ModelCatalogEntryInfo` (reasoning levels,
//!   service tiers, base instructions, ...) is intentionally omitted.
//! - **Config sources.** The model-relevant layers are honored in the same
//!   precedence: nearest `AGENTS.md` `model` block (workspace) overridden by an
//!   explicit `--config model=` / `model_provider_id=` override. The global
//!   `~/.browser-use-terminal` config / named profile and the session thread
//!   config are not re-read here (no faithful loader exists in the agent crate
//!   and they are not needed by the tui/cli repoint's resolution path); the
//!   precedence slot is preserved by the override + AGENTS.md chain.
//! - **Default model string.** Falls back to [`BUNDLED_DEFAULT_MODEL`]
//!   (`"gpt-5.5"`, matching legacy `default_model_for_cwd_with_options`'s final
//!   `unwrap_or`), surfaced through [`bundled_model_catalog`].

use std::path::Path;

use anyhow::Result;

/// Legacy `default_model_for_cwd_with_options`' final fallback model string
/// (`lib.rs:1362`, `.unwrap_or_else(|| "gpt-5.5".to_string())`).
pub const BUNDLED_DEFAULT_MODEL: &str = "gpt-5.5";

/// A single entry in the resolved model catalog.
///
/// Minimal mirror of `browser_use_providers::ModelCatalogEntryInfo`
/// (providers `lib.rs:828`), carrying only the resolution-relevant fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    /// The model identifier (legacy `slug`).
    pub slug: String,
    /// Human-readable name (legacy `display_name`).
    pub display_name: String,
    /// Whether this entry is the catalog default (legacy `is_default`).
    pub is_default: bool,
}

/// The resolved model catalog for a cwd.
///
/// Minimal mirror of `browser_use_providers::ModelCatalog` (providers
/// `lib.rs:823`, `{ models: Vec<ModelCatalogEntryInfo> }`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCatalog {
    /// The catalog entries.
    pub models: Vec<ModelCatalogEntry>,
}

impl ModelCatalog {
    /// The default model id for this catalog: the first entry flagged
    /// `is_default`, mirroring legacy `default_model_for_catalog`
    /// (`lib.rs:1365`, finds the preset with `is_default`).
    pub fn default_model(&self) -> Option<String> {
        self.models
            .iter()
            .find(|entry| entry.is_default)
            .map(|entry| entry.slug.clone())
    }
}

/// The bundled fallback catalog used when no cwd config supplies one.
///
/// Mirrors `browser_use_providers::bundled_model_catalog` (providers
/// `lib.rs:1096`) at the resolution level: a single default entry pinned to
/// [`BUNDLED_DEFAULT_MODEL`]. The full bundled `codex-models.json` is not
/// embedded here (see module docs).
pub fn bundled_model_catalog() -> ModelCatalog {
    ModelCatalog {
        models: vec![ModelCatalogEntry {
            slug: BUNDLED_DEFAULT_MODEL.to_string(),
            display_name: BUNDLED_DEFAULT_MODEL.to_string(),
            is_default: true,
        }],
    }
}

/// Options bundle for the fake/test agent path.
///
/// Ported from legacy `FakeAgentOptions<'a>` (lib.rs:117). The legacy struct is
/// a single borrowed field; the same lifetime-borrowed, `Copy` shape is
/// preserved so callers handing in `&str` slices port across unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FakeAgentOptions<'a> {
    /// Inline python program for the fake agent to "run" (test harness only).
    pub python_code: Option<&'a str>,
}

/// A single `AGENTS.md` `model` layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AgentsMdLayer {
    model: Option<String>,
    model_provider_id: Option<String>,
}

/// The model catalog available for `cwd`.
///
/// Ported from legacy `model_catalog_for_cwd_with_options` (lib.rs:1298):
/// `Ok(config.model_catalog.unwrap_or_else(bundled_model_catalog))`. When the
/// cwd config resolves a model, it is promoted to the catalog default so callers
/// observe the same "selected model is the default" behavior.
pub fn model_catalog_for_cwd_with_options(
    cwd: impl AsRef<Path>,
    config_profile: Option<&str>,
    config_overrides: &[(String, toml::Value)],
) -> Result<ModelCatalog> {
    let mut catalog = bundled_model_catalog();
    if let Some(model) =
        configured_model_for_cwd_with_options(cwd, config_profile, config_overrides)?
    {
        promote_default_model(&mut catalog, &model);
    }
    Ok(catalog)
}

/// Convenience wrapper resolving the catalog with default options.
///
/// Ported from legacy `model_catalog_for_cwd` (lib.rs:1313).
pub fn model_catalog_for_cwd(cwd: impl AsRef<Path>) -> Result<ModelCatalog> {
    model_catalog_for_cwd_with_options(cwd, None, &[])
}

/// The model explicitly configured for `cwd`, if any layer sets one.
///
/// Ported from legacy `configured_model_for_cwd_with_options` (lib.rs:1317):
/// reads `config.model`, trims it, and drops it when empty. Returns `Ok(None)`
/// when no layer configures a model. Precedence (highest first): `--config
/// model=` override -> nearest `AGENTS.md` `model` block.
pub fn configured_model_for_cwd_with_options(
    cwd: impl AsRef<Path>,
    config_profile: Option<&str>,
    config_overrides: &[(String, toml::Value)],
) -> Result<Option<String>> {
    let _ = config_profile;
    if let Some(model) = config_override_str(config_overrides, "model") {
        return Ok(non_empty_trimmed(&model));
    }
    if let Some(layer) = agents_md_layer_for_cwd(cwd.as_ref()) {
        if let Some(model) = layer.model {
            return Ok(non_empty_trimmed(&model));
        }
    }
    Ok(None)
}

/// The model to use for `cwd`, falling back to the bundled catalog default.
///
/// Ported from legacy `default_model_for_cwd_with_options` (lib.rs:1337):
/// returns `config.model` if set, else `default_model_for_catalog(catalog,
/// chatgpt_mode)`, else [`BUNDLED_DEFAULT_MODEL`]. `chatgpt_mode` is threaded
/// through for signature parity.
pub fn default_model_for_cwd_with_options(
    cwd: impl AsRef<Path>,
    config_profile: Option<&str>,
    config_overrides: &[(String, toml::Value)],
    chatgpt_mode: bool,
) -> Result<String> {
    let _ = chatgpt_mode;
    if let Some(model) =
        configured_model_for_cwd_with_options(&cwd, config_profile, config_overrides)?
    {
        return Ok(model);
    }
    let catalog = model_catalog_for_cwd_with_options(&cwd, config_profile, config_overrides)?;
    Ok(catalog
        .default_model()
        .unwrap_or_else(|| BUNDLED_DEFAULT_MODEL.to_string()))
}

/// The provider id explicitly configured for `cwd`, if any layer sets one.
///
/// Ported from legacy `configured_model_provider_id_for_cwd_with_options`
/// (lib.rs:1373): reads `config.model_provider_id`, trims it, drops it when
/// empty. Returns `Ok(None)` when no layer configures one. Precedence (highest
/// first): `--config model_provider_id=` override -> nearest `AGENTS.md` block.
pub fn configured_model_provider_id_for_cwd_with_options(
    cwd: impl AsRef<Path>,
    config_profile: Option<&str>,
    config_overrides: &[(String, toml::Value)],
) -> Result<Option<String>> {
    let _ = config_profile;
    if let Some(provider) = config_override_str(config_overrides, "model_provider_id") {
        return Ok(non_empty_trimmed(&provider));
    }
    if let Some(layer) = agents_md_layer_for_cwd(cwd.as_ref()) {
        if let Some(provider) = layer.model_provider_id {
            return Ok(non_empty_trimmed(&provider));
        }
    }
    Ok(None)
}

/// Convenience wrapper resolving the configured model with default options.
pub fn configured_model_for_cwd(cwd: impl AsRef<Path>) -> Result<Option<String>> {
    configured_model_for_cwd_with_options(cwd, None, &[])
}

/// Convenience wrapper resolving the default model with default options.
pub fn default_model_for_cwd(cwd: impl AsRef<Path>, chatgpt_mode: bool) -> Result<String> {
    default_model_for_cwd_with_options(cwd, None, &[], chatgpt_mode)
}

/// Convenience wrapper resolving the configured provider id with default
/// options.
pub fn configured_model_provider_id_for_cwd(cwd: impl AsRef<Path>) -> Result<Option<String>> {
    configured_model_provider_id_for_cwd_with_options(cwd, None, &[])
}

/// Promote `model` to the catalog default: clear every `is_default` flag, then
/// set it on the matching entry, inserting one if the model is not already
/// present so the resolved model is always discoverable as the default.
fn promote_default_model(catalog: &mut ModelCatalog, model: &str) {
    let mut found = false;
    for entry in catalog.models.iter_mut() {
        if entry.slug == model {
            entry.is_default = true;
            found = true;
        } else {
            entry.is_default = false;
        }
    }
    if !found {
        catalog.models.insert(
            0,
            ModelCatalogEntry {
                slug: model.to_string(),
                display_name: model.to_string(),
                is_default: true,
            },
        );
    }
}

/// Trim a value and drop it when empty, mirroring the legacy
/// `.map(str::trim).filter(|v| !v.is_empty())` chain.
fn non_empty_trimmed(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Look up a string-valued `--config key=value` override.
///
/// The override list is the same shape
/// [`crate::config_overrides::ConfigOverrides`] / `parse_config_overrides`
/// produces (`Vec<(String, toml::Value)>`). The last matching entry wins,
/// mirroring last-write-wins override application.
fn config_override_str(config_overrides: &[(String, toml::Value)], key: &str) -> Option<String> {
    config_overrides
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.as_str().map(|s| s.to_string()))
}

/// Walk from `cwd` up to the filesystem root, returning the nearest `AGENTS.md`
/// `model` layer. The nearest wins.
fn agents_md_layer_for_cwd(cwd: &Path) -> Option<AgentsMdLayer> {
    let mut dir: Option<&Path> = Some(cwd);
    while let Some(current) = dir {
        let candidate = current.join("AGENTS.md");
        if let Some(layer) = parse_agents_md_layer(&candidate) {
            return Some(layer);
        }
        dir = current.parent();
    }
    None
}

/// Read and parse the `AGENTS.md` at `path`, if present and it has a `model`
/// block.
fn parse_agents_md_layer(path: &Path) -> Option<AgentsMdLayer> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_agents_md_layer_text(&text)
}

/// Parse a fenced ```` ```model ```` TOML block out of AGENTS.md text.
///
/// Example block:
/// ```text
/// ```model
/// model = "gpt-4o"
/// model_provider_id = "openai"
/// ```
/// ```
fn parse_agents_md_layer_text(text: &str) -> Option<AgentsMdLayer> {
    let mut in_block = false;
    let mut model: Option<String> = None;
    let mut model_provider_id: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "```model" {
            in_block = true;
            continue;
        }
        if in_block && trimmed == "```" {
            break;
        }
        if in_block {
            if let Some(rest) = trimmed.strip_prefix("model_provider_id") {
                if let Some(v) = parse_toml_scalar_value(rest) {
                    model_provider_id = Some(v);
                }
            } else if let Some(rest) = trimmed.strip_prefix("model") {
                if let Some(v) = parse_toml_scalar_value(rest) {
                    model = Some(v);
                }
            }
        }
    }
    if model.is_none() && model_provider_id.is_none() {
        return None;
    }
    Some(AgentsMdLayer {
        model,
        model_provider_id,
    })
}

/// Parse `= "value"` / `= 'value'` / `= value` (bare). Strips the `=`,
/// surrounding whitespace, and matched quotes.
fn parse_toml_scalar_value(rest: &str) -> Option<String> {
    let rest = rest.trim();
    let rest = rest.strip_prefix('=')?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let unquoted = rest.trim_matches('"').trim_matches('\'').to_string();
    if unquoted.is_empty() {
        return None;
    }
    Some(unquoted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn overrides(pairs: &[(&str, &str)]) -> Vec<(String, toml::Value)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), toml::Value::String(v.to_string())))
            .collect()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "config_model_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fake_agent_options_defaults() {
        let opts = FakeAgentOptions::default();
        assert_eq!(opts.python_code, None);
    }

    #[test]
    fn fake_agent_options_is_copy_and_borrows() {
        // Confirms the legacy single-field, lifetime-borrowed, `Copy` shape.
        let code = "print(1)".to_string();
        let opts = FakeAgentOptions {
            python_code: Some(code.as_str()),
        };
        let copied = opts; // Copy: opts still usable below.
        assert_eq!(opts.python_code, Some("print(1)"));
        assert_eq!(copied.python_code, Some("print(1)"));
    }

    #[test]
    fn bundled_catalog_default_is_gpt_5_5() {
        let catalog = bundled_model_catalog();
        assert_eq!(
            catalog.default_model(),
            Some(BUNDLED_DEFAULT_MODEL.to_string())
        );
        assert_eq!(catalog.default_model(), Some("gpt-5.5".to_string()));
        assert_eq!(catalog.models.len(), 1);
        assert!(catalog.models[0].is_default);
    }

    #[test]
    fn default_model_falls_back_to_bundled_default_when_unconfigured() {
        let dir = temp_dir("default");
        let resolved = default_model_for_cwd_with_options(&dir, None, &[], true).unwrap();
        assert_eq!(resolved, BUNDLED_DEFAULT_MODEL);
        // Nothing configured -> configured_* are None.
        assert_eq!(
            configured_model_for_cwd_with_options(&dir, None, &[]).unwrap(),
            None
        );
        assert_eq!(
            configured_model_provider_id_for_cwd_with_options(&dir, None, &[]).unwrap(),
            None
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_override_takes_precedence_over_agents_md() {
        let dir = temp_dir("override");
        // Even with an AGENTS.md present, an explicit override wins.
        fs::write(
            dir.join("AGENTS.md"),
            "```model\nmodel = \"gpt-4.1\"\nmodel_provider_id = \"openai\"\n```\n",
        )
        .unwrap();
        let ov = overrides(&[("model", "o3"), ("model_provider_id", "custom")]);
        assert_eq!(
            configured_model_for_cwd_with_options(&dir, None, &ov).unwrap(),
            Some("o3".to_string())
        );
        assert_eq!(
            default_model_for_cwd_with_options(&dir, None, &ov, true).unwrap(),
            "o3"
        );
        assert_eq!(
            configured_model_provider_id_for_cwd_with_options(&dir, None, &ov).unwrap(),
            Some("custom".to_string())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn agents_md_resolves_when_no_override() {
        let dir = temp_dir("agentsmd");
        fs::write(
            dir.join("AGENTS.md"),
            "# Project\n\n```model\nmodel = \"gpt-4.1\"\nmodel_provider_id = \"openai\"\n```\n",
        )
        .unwrap();
        assert_eq!(
            configured_model_for_cwd_with_options(&dir, None, &[]).unwrap(),
            Some("gpt-4.1".to_string())
        );
        assert_eq!(
            configured_model_provider_id_for_cwd_with_options(&dir, None, &[]).unwrap(),
            Some("openai".to_string())
        );
        assert_eq!(
            default_model_for_cwd_with_options(&dir, None, &[], true).unwrap(),
            "gpt-4.1"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn agents_md_nearest_wins_walking_up() {
        let root = temp_dir("nearest");
        let child = root.join("sub").join("deep");
        fs::create_dir_all(&child).unwrap();
        fs::write(
            root.join("AGENTS.md"),
            "```model\nmodel = \"gpt-4o\"\n```\n",
        )
        .unwrap();
        // Nearer AGENTS.md wins.
        fs::write(
            root.join("sub").join("AGENTS.md"),
            "```model\nmodel = \"gpt-4.1\"\n```\n",
        )
        .unwrap();
        assert_eq!(
            configured_model_for_cwd_with_options(&child, None, &[]).unwrap(),
            Some("gpt-4.1".to_string())
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn catalog_promotes_resolved_model_as_default() {
        let dir = temp_dir("catalog");
        fs::write(dir.join("AGENTS.md"), "```model\nmodel = \"o3\"\n```\n").unwrap();
        let catalog = model_catalog_for_cwd_with_options(&dir, None, &[]).unwrap();
        // Resolved model becomes the catalog default.
        assert_eq!(catalog.default_model(), Some("o3".to_string()));
        // Exactly one default entry.
        let default_count = catalog.models.iter().filter(|e| e.is_default).count();
        assert_eq!(default_count, 1);
        assert!(catalog.models.iter().any(|e| e.slug == "o3"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn catalog_default_when_unconfigured_matches_bundled() {
        let dir = temp_dir("catalog_default");
        let catalog = model_catalog_for_cwd_with_options(&dir, None, &[]).unwrap();
        assert_eq!(catalog, bundled_model_catalog());
        assert_eq!(
            catalog.default_model(),
            Some(BUNDLED_DEFAULT_MODEL.to_string())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_override_value_is_dropped_like_legacy() {
        // Legacy trims and drops empty `config.model`; an override of "   "
        // resolves to None (not Some("")).
        let dir = temp_dir("empty");
        let ov = overrides(&[("model", "   ")]);
        assert_eq!(
            configured_model_for_cwd_with_options(&dir, None, &ov).unwrap(),
            None
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_toml_scalar_value_handles_quotes_and_bare() {
        assert_eq!(
            parse_toml_scalar_value(" = \"gpt-4o\""),
            Some("gpt-4o".to_string())
        );
        assert_eq!(
            parse_toml_scalar_value(" = 'gpt-4o'"),
            Some("gpt-4o".to_string())
        );
        assert_eq!(
            parse_toml_scalar_value(" = bare-model"),
            Some("bare-model".to_string())
        );
        assert_eq!(parse_toml_scalar_value(" = "), None);
        assert_eq!(parse_toml_scalar_value("no-equals"), None);
    }

    #[test]
    fn config_override_str_last_write_wins() {
        let ov = overrides(&[("model", "first"), ("model", "second")]);
        assert_eq!(
            config_override_str(&ov, "model"),
            Some("second".to_string())
        );
        assert_eq!(config_override_str(&ov, "absent"), None);
    }
}
