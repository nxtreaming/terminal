//! MENTION parsing + resolution (codex / legacy parity).
//!
//! Users can explicitly invoke a skill in three ways. This module extracts those
//! references from free user text and resolves them against the discovered
//! skills.
//!
//! ## Sigils / syntaxes (parity sources)
//!
//! * `$Name` — the **tool mention sigil**, codex
//!   `TOOL_MENTION_SIGIL = '$'` (`utils/plugins/src/mention_syntax.rs:4`,
//!   re-exported `core/src/mention_syntax.rs:2`). Used both as a plaintext
//!   `$Name` token and as the visible label of a linked mention
//!   `[$Name](skill://…)` (codex `mentions_tests.rs:56`). Also the legacy
//!   "Trigger rules" text: "If the user names a skill (with `$SkillName` …)"
//!   (`SKILLS_HOW_TO_USE_WITH_ABSOLUTE_PATHS`, render.rs:28).
//! * `@Name` — the **plugin plaintext mention sigil**, codex
//!   `PLUGIN_TEXT_MENTION_SIGIL = '@'`
//!   (`utils/plugins/src/mention_syntax.rs:7`). In codex `collect_explicit_*`
//!   the `@` sigil is used for the plaintext form of plugin/app links
//!   (`plugins/src/mentions.rs:86-88`). We accept `@Name` as a skill mention too
//!   (a skill contributed by a plugin is named `Plugin:skill`, so `@` and `$`
//!   both resolve against the same discovered-skill table).
//! * `skill://name` — the **structured skill reference scheme**, codex
//!   `[$label](skill://team/skill)` whose `path` is `"skill://team/skill"`
//!   (`plugins/src/mentions_tests.rs:56-65`). The path after `skill://` is the
//!   skill identifier.
//!
//! Resolution matches a discovered skill by exact `name`, with a fallback to the
//! last `/`-segment of a `skill://a/b/c` path (the leaf skill id) and to the
//! plugin-namespaced suffix after a `:` (so `@docs` resolves a `Plugin:docs`
//! skill). Unknown mentions are returned separately so callers can surface a
//! brief "skill not found" notice (legacy "Missing/blocked" rule, render.rs:29).

use std::collections::BTreeSet;

use super::discovery::Skill;

/// codex `TOOL_MENTION_SIGIL` (`utils/plugins/src/mention_syntax.rs:4`).
pub const TOOL_MENTION_SIGIL: char = '$';
/// codex `PLUGIN_TEXT_MENTION_SIGIL` (`utils/plugins/src/mention_syntax.rs:7`).
pub const PLUGIN_TEXT_MENTION_SIGIL: char = '@';
/// The structured skill-reference URI scheme (codex `skill://…`).
pub const SKILL_SCHEME: &str = "skill://";

/// A parsed mention before resolution.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mention {
    /// `$Name` — a tool-sigil plaintext mention.
    Tool(String),
    /// `@Name` — a plugin-sigil plaintext mention.
    Plugin(String),
    /// `skill://path` — a structured skill reference (the part after `skill://`).
    Uri(String),
}

impl Mention {
    /// The raw target string (the part after the sigil / scheme).
    pub fn target(&self) -> &str {
        match self {
            Mention::Tool(s) | Mention::Plugin(s) | Mention::Uri(s) => s,
        }
    }
}

/// The result of resolving the mentions in a piece of text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedMentions {
    /// Skills that a mention resolved to, deduped, in first-mention order.
    pub resolved: Vec<Skill>,
    /// Mentions that did not resolve to any discovered skill.
    pub unresolved: Vec<Mention>,
}

/// Parse all `$Name`, `@Name`, and `skill://path` mentions out of `text`.
///
/// `skill://` references are recognized both bare and inside a markdown link
/// `[$label](skill://path)` (only the `skill://path` target is captured; the
/// `$label` is the visible alias and is not a separate mention). Plaintext `$`
/// and `@` tokens are captured as `name` runs of identifier characters
/// (alphanumerics, `_`, `-`, `:`, `/`), matching the codex mention tokenizer's
/// notion of a mention name.
pub fn parse_mentions(text: &str) -> Vec<Mention> {
    let mut out: Vec<Mention> = Vec::new();

    // 1) Structured skill:// references (bare or inside `(...)`).
    let mut search = text;
    while let Some(idx) = search.find(SKILL_SCHEME) {
        let after = &search[idx + SKILL_SCHEME.len()..];
        let target: String = after
            .chars()
            .take_while(|c| is_mention_path_char(*c))
            .collect();
        if !target.is_empty() {
            out.push(Mention::Uri(target.clone()));
        }
        // Advance past this occurrence.
        let consumed = idx + SKILL_SCHEME.len() + target.len();
        if consumed >= search.len() {
            break;
        }
        search = &search[consumed..];
    }

    // 2) Plaintext `$Name` / `@Name` tokens. We scan char-by-char so a sigil
    // only starts a mention at a word boundary (start of string or after
    // whitespace / common punctuation), avoiding e.g. an email `a@b` or a `$5`
    // price being mis-parsed as a skill.
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        let is_sigil = c == TOOL_MENTION_SIGIL || c == PLUGIN_TEXT_MENTION_SIGIL;
        let boundary = i == 0 || is_boundary(chars[i - 1]);
        if is_sigil && boundary {
            let mut j = i + 1;
            // The first char after the sigil must be a name start (letter/_),
            // so `$5` / `@2` are not mentions.
            let mut name = String::new();
            while j < chars.len() && is_mention_name_char(chars[j]) {
                name.push(chars[j]);
                j += 1;
            }
            if !name.is_empty() && name.chars().next().is_some_and(is_name_start) {
                let mention = if c == TOOL_MENTION_SIGIL {
                    Mention::Tool(name)
                } else {
                    Mention::Plugin(name)
                };
                out.push(mention);
            }
            i = j.max(i + 1);
            continue;
        }
        i += 1;
    }

    out
}

/// Resolve parsed mentions against the discovered `skills`.
///
/// Matching, in order of preference:
///   1. exact `skill.name` match;
///   2. for a `skill://a/b` URI: the leaf segment (`b`) matches `skill.name`;
///   3. the plugin-namespaced suffix after a `:` matches (so `@docs` resolves a
///      `Plugin:docs` skill, mirroring the `Prefix:name` legacy namespacing).
///
/// Resolved skills are deduped (by name) preserving first-mention order;
/// unresolved mentions are collected separately.
pub fn resolve_mentions(text: &str, skills: &[Skill]) -> ResolvedMentions {
    let mentions = parse_mentions(text);
    let mut resolved: Vec<Skill> = Vec::new();
    let mut unresolved: Vec<Mention> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for mention in mentions {
        match resolve_one(&mention, skills) {
            Some(skill) => {
                if seen.insert(skill.name.clone()) {
                    resolved.push(skill.clone());
                }
            }
            None => unresolved.push(mention),
        }
    }

    ResolvedMentions {
        resolved,
        unresolved,
    }
}

/// Resolve a single mention to a discovered skill, if any.
fn resolve_one<'a>(mention: &Mention, skills: &'a [Skill]) -> Option<&'a Skill> {
    let target = mention.target();
    // 1) exact name.
    if let Some(s) = skills.iter().find(|s| s.name == target) {
        return Some(s);
    }
    // 2) leaf segment of a `/`-separated target (skill:// path).
    if let Some(leaf) = target.rsplit('/').next() {
        if leaf != target {
            if let Some(s) = skills.iter().find(|s| s.name == leaf) {
                return Some(s);
            }
        }
    }
    // 3) plugin-namespaced suffix: a skill named `Prefix:target`.
    if let Some(s) = skills
        .iter()
        .find(|s| s.name.rsplit(':').next() == Some(target))
    {
        return Some(s);
    }
    None
}

/// Identifier-start char: a letter or `_`.
fn is_name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

/// Chars allowed inside a plaintext mention name.
fn is_mention_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == ':' || c == '/'
}

/// Chars allowed inside a `skill://` path target (adds `.` for path-like ids).
fn is_mention_path_char(c: char) -> bool {
    is_mention_name_char(c) || c == '.'
}

/// True iff `c` is a left word boundary before a sigil (so `$`/`@` only start a
/// mention at the start of a token, not mid-word like an email `user@host`).
fn is_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | '[' | '{' | ',' | ';' | '"' | '\'' | '`' | '*')
}
