//! `skills/` — SKILLS + PLUGINS subsystem (codex / legacy parity).
//!
//! Three pure cores plus a thin controller:
//! - [`discovery`] — walk skill/plugin roots in precedence order, parse
//!   `SKILL.md` (frontmatter + body), dedup same-name (higher precedence wins).
//! - [`inject`]    — build the budgeted `<skills_instructions>` developer block
//!   (~2% of the context window), degrading descriptions/skills to fit.
//! - [`mention`]   — parse + resolve `$Name`, `@Name`, and `skill://name`
//!   references in user text against the discovered skills.
//!
//! [`SkillsManager`] ties them together: discover once, then render the
//! injection block and resolve user mentions against the discovered set.
//!
//! ## Where this wires in (parity debt / future WP)
//!
//! In codex/legacy the discovery roots are derived from config + cwd + plugin
//! config (legacy `available_skill_summaries`, lib.rs:16848) and the rendered
//! block is appended to the developer-instruction sections at session start
//! (legacy lib.rs:694). This crate's session/turn-assembly layer is owned by
//! other WPs; here [`SkillsManager`] is the pure, network-free seam those layers
//! call. Specifically:
//!   * a config/cwd → `Vec<SkillRoot>` builder (config plumbing) is a later WP;
//!   * splicing the [`SkillsManager::skills_instructions`] item into the
//!     transcript via [`crate::context::inject`] happens in turn assembly;
//!   * `skill://` tool invocation / approval (codex `skills_handler`) and skill
//!     *dependencies* (codex `mcp_skill_dependencies`) are out of scope here.

pub mod discovery;
pub mod inject;
pub mod mention;

#[cfg(test)]
mod tests;

pub use discovery::{discover_skills, Skill, SkillRoot, SkillSource};
pub use inject::{
    default_skill_metadata_budget, is_skills_instructions_message, render_skills_body,
    render_skills_instructions, SkillMetadataBudget, SkillRenderReport,
    SKILLS_INSTRUCTIONS_CLOSE_TAG, SKILLS_INSTRUCTIONS_NAME, SKILLS_INSTRUCTIONS_OPEN_TAG,
};
pub use mention::{parse_mentions, resolve_mentions, Mention, ResolvedMentions};

use crate::context::Item;

/// Controller tying discovery + injection + mention resolution together.
///
/// Holds the discovered skills (precedence-ordered, deduped) and exposes the two
/// model-facing operations: rendering the budgeted `<skills_instructions>` block
/// and resolving explicit mentions in user input.
#[derive(Clone, Debug, Default)]
pub struct SkillsManager {
    skills: Vec<Skill>,
}

impl SkillsManager {
    /// An empty manager (no skills discovered).
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a manager by discovering skills under `roots` (precedence order:
    /// highest-precedence root first). Network-free; missing roots are skipped.
    pub fn discover(roots: &[SkillRoot]) -> Self {
        Self {
            skills: discover_skills(roots),
        }
    }

    /// Build a manager from an already-discovered skill list (e.g. for tests or
    /// when discovery happened elsewhere).
    pub fn from_skills(skills: Vec<Skill>) -> Self {
        Self { skills }
    }

    /// The discovered skills, in precedence order.
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// The budgeted `<skills_instructions>` context item, or `None` when there
    /// are no skills. `context_budget_tokens` is the model context window (drives
    /// the ~2% token budget); pass `None` to use the 8000-character fallback.
    pub fn skills_instructions(&self, context_budget_tokens: Option<i64>) -> Option<Item> {
        render_skills_instructions(&self.skills, context_budget_tokens)
    }

    /// Resolve the `$`/`@`/`skill://` mentions in `text` against the discovered
    /// skills.
    pub fn resolve(&self, text: &str) -> ResolvedMentions {
        resolve_mentions(text, &self.skills)
    }
}
