//! Skill + plugin DISCOVERY (codex / legacy parity).
//!
//! A *skill* is a directory tree containing a `SKILL.md` file with a
//! `---`-delimited YAML frontmatter header (`name`, `description`,
//! `metadata.short-description`) and a markdown body. Skills are discovered by
//! walking a list of *roots* in **precedence order**; the first root to define a
//! given skill name wins (higher-precedence root shadows lower-precedence ones).
//!
//! ## Parity sources
//!
//! * codex `core-skills/src/loader.rs`:
//!   - `SKILLS_FILENAME = "SKILL.md"` (loader.rs:107).
//!   - `SkillFrontmatter { name, description, metadata.short-description }`
//!     parsed from `---`-delimited YAML frontmatter (loader.rs:39-53,
//!     `extract_frontmatter` loader.rs:1025).
//!   - dedup keeps the FIRST occurrence of a `path_to_skills_md`, in root scan
//!     order (`outcome.skills.retain(seen.insert(...))` loader.rs:196-199); the
//!     scan order is highest-precedence-first so the first-seen wins.
//!   - scope precedence `Repo(0) < User(1) < System(2) < Admin(3)`
//!     (`scope_rank` loader.rs:213-221).
//!   - `MAX_SCAN_DEPTH = 6` (loader.rs:123).
//! * legacy `browser-use-core/src/lib.rs`:
//!   - roots assembled in `available_skill_summaries` (lib.rs:16848): user
//!     (`<home>/skills`, `<home>/.agents/skills`, rank 3), bundled/system
//!     (`<home>/.tmp/skills` rank 0), plugin skill roots (rank 3), repo
//!     (`<cwd>/.agents/skills`, `<cwd>/.browser-use-terminal/skills`, rank 2);
//!     then sorted by `(scope_rank, name, path)`.
//!   - SKILL.md hand-rolled line parser `skill_frontmatter_value` (lib.rs:17129)
//!     reads `---`-delimited lines, splits on the first `:`, and trims `"`/`'`.
//!     We mirror this exactly (legacy is the authoritative wire shape and does
//!     NOT pull in `serde_yaml`); body description fallback mirrors
//!     `skill_body_description_from_markdown` (lib.rs:17119): first non-empty,
//!     non-`#` line of the body.
//!   - dedup is by canonicalized `SKILL.md` path, first-seen wins
//!     (`seen.insert(canonical)` lib.rs:16974), in root scan order.
//!   - `collect_skill_summaries` recursion bottoms out at the first directory
//!     containing `SKILL.md` (does not descend further), `MAX_DEPTH = 5`
//!     (lib.rs:16959).
//!
//! ## Precedence note (parity debt)
//!
//! Legacy and codex disagree on the *numeric* scope ranks but AGREE on the
//! resolution rule: roots are scanned highest-precedence-first and the first
//! root to contribute a `SKILL.md` path wins. We model precedence by the **order
//! of the `roots` slice** passed to [`discover_skills`] (caller supplies roots
//! highest-precedence-first) plus a per-name first-seen dedup, which is faithful
//! to both. We do NOT re-derive codex's `SkillScope` enum here (that is config
//! plumbing owned by a later integration WP); see [`SkillSource`].

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Maximum directory depth scanned below a discovery root before giving up.
///
/// Codex uses `MAX_SCAN_DEPTH = 6` (loader.rs:123); legacy uses `MAX_DEPTH = 5`
/// (lib.rs:16959). We take codex's value (the larger) so no codex-discoverable
/// skill is missed; the difference only matters for pathologically deep trees.
pub const MAX_SCAN_DEPTH: usize = 6;

/// The canonical skill manifest filename. Codex `SKILLS_FILENAME` (loader.rs:107)
/// / legacy `dir.join("SKILL.md")` (lib.rs:16963).
pub const SKILL_FILENAME: &str = "SKILL.md";

/// The provenance of a discovered skill root, mirroring the precedence layering
/// codex/legacy apply. Lower discriminant = higher precedence (scanned first),
/// matching the legacy root ordering in `available_skill_summaries`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SkillSource {
    /// Repository / project-local skills (`<cwd>/.agents/skills`, …). Highest
    /// precedence — a repo skill shadows a same-named user/plugin/system skill.
    Repo,
    /// Per-user skills (`<home>/skills`, `<home>/.agents/skills`).
    User,
    /// Skills contributed by an installed plugin.
    Plugin,
    /// Bundled / system skills (`<home>/.tmp/skills`, `/etc/.../skills`). Lowest
    /// precedence.
    System,
}

/// A discovery root: a directory to scan, plus its provenance and an optional
/// name prefix (plugins namespace their skills as `Display:name`, mirroring
/// legacy `format!("{prefix}:{name}")` at lib.rs:17055).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillRoot {
    /// The directory to scan for `SKILL.md` files.
    pub path: PathBuf,
    /// Where this root came from (drives precedence + dedup).
    pub source: SkillSource,
    /// Optional `Prefix:` prepended to discovered skill names (plugins only).
    pub name_prefix: Option<String>,
}

impl SkillRoot {
    /// A bare root with no name prefix.
    pub fn new(path: impl Into<PathBuf>, source: SkillSource) -> Self {
        Self {
            path: path.into(),
            source,
            name_prefix: None,
        }
    }

    /// A plugin root that namespaces its skills under `prefix:`.
    pub fn plugin(path: impl Into<PathBuf>, prefix: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            source: SkillSource::Plugin,
            name_prefix: Some(prefix.into()),
        }
    }
}

/// A single discovered skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    /// The skill name (frontmatter `name`, possibly `prefix:`-namespaced for
    /// plugin skills; falls back to the directory name when frontmatter omits
    /// `name`, mirroring legacy `fallback_name` at lib.rs:17040).
    pub name: String,
    /// The model-visible one-line description (frontmatter `description`, then
    /// `metadata.short-description`, then the first body line — legacy
    /// `skill_frontmatter_description_from_markdown` / `skill_body_description`).
    pub description: Option<String>,
    /// The skill body (markdown after the frontmatter).
    pub body: String,
    /// Absolute path to the discovered `SKILL.md`.
    pub path: PathBuf,
    /// The provenance of the root that contributed this skill.
    pub source: SkillSource,
}

/// Discover all skills under `roots`, in **precedence order**.
///
/// Roots are scanned in slice order (caller supplies them highest-precedence
/// first). Within each root the tree is walked depth-first to [`MAX_SCAN_DEPTH`];
/// the first directory containing a [`SKILL_FILENAME`] is parsed as a skill and
/// the walk does NOT descend into it further (matching legacy
/// `collect_skill_summaries`, which `return`s on the first `SKILL.md`).
///
/// Dedup keeps the FIRST occurrence of a given **skill name** (after
/// `prefix:`-namespacing), so a higher-precedence root shadows a same-named
/// skill in a lower-precedence root. The returned `Vec` is in discovery order
/// (precedence order, then directory-walk order within a root).
///
/// This is network-free and tolerant of missing/unreadable roots (they are
/// skipped), matching codex/legacy "fail open" behavior.
pub fn discover_skills(roots: &[SkillRoot]) -> Vec<Skill> {
    let mut seen_names: HashSet<String> = HashSet::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    let mut out: Vec<Skill> = Vec::new();

    for root in roots {
        let mut found: Vec<Skill> = Vec::new();
        scan_dir(&root.path, &root.path, root, 0, &mut seen_paths, &mut found);
        for skill in found {
            // First-seen-name wins (precedence): a later (lower-precedence) root
            // does not override a name already contributed.
            if seen_names.insert(skill.name.clone()) {
                out.push(skill);
            }
        }
    }

    out
}

/// Recursive depth-first scan. Mirrors legacy `collect_skill_summaries`
/// (lib.rs:16948): a directory that directly contains `SKILL.md` is treated as a
/// skill and the recursion stops there; otherwise each subdirectory is scanned
/// up to [`MAX_SCAN_DEPTH`].
fn scan_dir(
    root: &Path,
    dir: &Path,
    skill_root: &SkillRoot,
    depth: usize,
    seen_paths: &mut HashSet<PathBuf>,
    out: &mut Vec<Skill>,
) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }

    let skill_md = dir.join(SKILL_FILENAME);
    if skill_md.is_file() {
        // Dedup by canonicalized path, first-seen wins (legacy lib.rs:16974,
        // codex loader.rs:196). Canonicalize so two roots reaching the same file
        // via different paths collapse.
        let canonical = fs::canonicalize(&skill_md).unwrap_or_else(|_| skill_md.clone());
        if seen_paths.insert(canonical) {
            if let Some(skill) = parse_skill_file(root, &skill_md, skill_root) {
                out.push(skill);
            }
        }
        return;
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    // Sort entries for deterministic discovery order (read_dir order is OS
    // dependent; legacy relies on the later `(scope_rank, name, path)` sort, but
    // we want a stable order here too).
    let mut subdirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();
    for sub in subdirs {
        scan_dir(root, &sub, skill_root, depth + 1, seen_paths, out);
    }
}

/// Parse one `SKILL.md` into a [`Skill`].
///
/// Returns `None` only when the file cannot be read (fail open). Name resolution
/// mirrors legacy `skill_summary_from_skill_md` (lib.rs:17026): frontmatter
/// `name`, else a directory-derived fallback; then a `prefix:` is prepended for
/// plugin roots.
fn parse_skill_file(root: &Path, path: &Path, skill_root: &SkillRoot) -> Option<Skill> {
    let contents = fs::read_to_string(path).ok()?;

    let fallback = fallback_name(root, path);
    let raw_name = frontmatter_value(&contents, "name").unwrap_or(fallback);
    let name = match skill_root.name_prefix.as_deref() {
        Some(prefix) if !prefix.trim().is_empty() => format!("{}:{}", prefix.trim(), raw_name),
        _ => raw_name,
    };

    let description = description_from_skill_md(&contents);
    let body = strip_frontmatter(&contents).trim().to_string();

    Some(Skill {
        name,
        description,
        body,
        path: path.to_path_buf(),
        source: skill_root.source,
    })
}

/// Directory-derived fallback name when frontmatter omits `name`.
///
/// Legacy `skill_summary_from_skill_md` (lib.rs:17032): the path of the skill
/// directory relative to the root, components joined by `/`, dropping empty and
/// `.system` segments; if that is empty, the directory's own file name; if that
/// too is empty, the literal `"skill"`.
fn fallback_name(root: &Path, path: &Path) -> String {
    let parent = path.parent().unwrap_or(root);
    let relative = parent.strip_prefix(root).unwrap_or(parent);
    let joined = relative
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .filter(|seg| !seg.is_empty() && *seg != ".system")
        .collect::<Vec<_>>()
        .join("/");
    if !joined.is_empty() {
        return joined;
    }
    parent
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("skill")
        .to_string()
}

/// Resolve a skill description, mirroring legacy precedence
/// (`skill_frontmatter_description_from_markdown` lib.rs:17106 then
/// `skill_body_description_from_markdown` lib.rs:17119):
///   1. frontmatter `description`;
///   2. frontmatter `metadata.short-description` / `metadata.short_description`;
///   3. the first non-empty, non-`#` line of the markdown body.
pub fn description_from_skill_md(contents: &str) -> Option<String> {
    if let Some(d) = frontmatter_value(contents, "description") {
        return Some(d);
    }
    if let Some(d) = frontmatter_nested_value(contents, "metadata", "short-description")
        .or_else(|| frontmatter_nested_value(contents, "metadata", "short_description"))
    {
        return Some(d);
    }
    body_description(contents)
}

/// First non-empty, non-`#` line of the markdown body (after frontmatter).
/// Legacy `skill_body_description_from_markdown` (lib.rs:17119).
fn body_description(contents: &str) -> Option<String> {
    let body = strip_frontmatter(contents);
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
}

/// Read a top-level frontmatter scalar by key.
///
/// Hand-rolled line parser mirroring legacy `skill_frontmatter_value`
/// (lib.rs:17129): the file must start with a `---` line; subsequent lines up to
/// the closing `---` are split on the first `:`; the value is trimmed and
/// surrounding `"`/`'` quotes are stripped. Returns the first non-empty match.
///
/// We deliberately do NOT use `serde_yaml` (legacy doesn't, and it is not a dep
/// of this crate); this keeps the wire shape byte-identical to legacy.
pub fn frontmatter_value(contents: &str, key: &str) -> Option<String> {
    let mut lines = contents.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        let Some((candidate, raw)) = line.split_once(':') else {
            continue;
        };
        if candidate.trim() != key {
            continue;
        }
        let value = raw.trim().trim_matches('"').trim_matches('\'').trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// Read a nested frontmatter scalar `section.key` from a simple two-level
/// `section:` / `  key: value` YAML-ish layout.
///
/// Mirrors legacy `simple_yamlish_nested_value` semantics
/// (`skill_frontmatter_nested_value` lib.rs:17158): find the `section:` line,
/// then the first more-indented `key:` line before the next top-level key.
fn frontmatter_nested_value(contents: &str, section: &str, key: &str) -> Option<String> {
    let mut lines = contents.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut in_section = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented {
            // A new top-level key. Enter the section if it matches, else leave.
            in_section = trimmed
                .split_once(':')
                .map(|(k, _)| k.trim() == section)
                .unwrap_or(false);
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, raw)) = trimmed.split_once(':') {
            if k.trim() == key {
                let value = raw.trim().trim_matches('"').trim_matches('\'').trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// Return the markdown body with any leading `---`-delimited frontmatter
/// removed. If there is no frontmatter, the input is returned unchanged.
/// Mirrors legacy `strip_markdown_frontmatter`.
pub fn strip_frontmatter(contents: &str) -> &str {
    // The first line must be exactly `---`.
    let Some(rest) = contents.strip_prefix("---") else {
        return contents;
    };
    // `rest` starts right after the opening `---`; a frontmatter block requires
    // that delimiter to be its own line (followed by a newline).
    let Some(after_open) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return contents;
    };
    // Find the closing `---` on its own line; if there is none this is not a
    // valid frontmatter block, so return the input unchanged.
    match find_closing_delimiter(after_open) {
        Some(end) => &after_open[end..],
        None => contents,
    }
}

/// Locate the byte offset just past the closing `---\n` delimiter within
/// `after_open` (the text following the opening delimiter line). Returns `None`
/// when there is no closing delimiter.
fn find_closing_delimiter(after_open: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in after_open.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']).trim() == "---" {
            return Some(offset + line.len());
        }
        offset += line.len();
    }
    None
}
