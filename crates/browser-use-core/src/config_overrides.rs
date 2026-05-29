//! TOML config-override parsing extracted from `lib.rs` (Phase 0.1 carve).
//!
//! Code motion only — behavior is byte-identical to the original definitions.

use anyhow::{anyhow, bail, Result};

use crate::{AgentRunOptions, ConfigOverrides};

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

fn canonicalize_config_override_key(key: &str) -> String {
    if key == "use_legacy_landlock" {
        "features.use_legacy_landlock".to_string()
    } else {
        key.to_string()
    }
}

fn parse_config_override_toml_value(raw: &str) -> Option<toml::Value> {
    let wrapped = format!("_x_ = {raw}");
    let mut table = toml::from_str::<toml::Table>(&wrapped).ok()?;
    table.remove("_x_")
}

pub(crate) fn config_has_any_override(
    options: &AgentRunOptions,
    keys: &[&str],
    prefixes: &[String],
) -> bool {
    config_override_pairs_have_any_override(&options.config_overrides, keys, prefixes)
        || options
            .session_thread_config
            .as_ref()
            .is_some_and(|value| toml_value_has_any_override(value, "", keys, prefixes))
}

fn config_override_pairs_have_any_override(
    overrides: &[(String, toml::Value)],
    keys: &[&str],
    prefixes: &[String],
) -> bool {
    overrides
        .iter()
        .any(|(key, _)| config_path_matches(key, keys, prefixes))
}

fn toml_value_has_any_override(
    value: &toml::Value,
    prefix: &str,
    keys: &[&str],
    prefixes: &[String],
) -> bool {
    let Some(table) = value.as_table() else {
        return false;
    };
    table.iter().any(|(key, value)| {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        config_path_matches(&path, keys, prefixes)
            || toml_value_has_any_override(value, &path, keys, prefixes)
    })
}

fn config_path_matches(key: &str, keys: &[&str], prefixes: &[String]) -> bool {
    keys.iter().any(|candidate| key == *candidate)
        || prefixes.iter().any(|prefix| key.starts_with(prefix))
}
