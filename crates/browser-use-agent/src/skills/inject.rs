//! Budgeted `<skills_instructions>` injection (codex / legacy parity).
//!
//! Builds the model-facing `<skills_instructions>` developer-context block that
//! advertises the discovered skills (name + description + path), budgeted to
//! ~2% of the model context window. When the full listing would exceed the
//! budget, descriptions are dropped (lowest-priority skills first) so the names
//! always fit — matching codex's degrade-gracefully behavior.
//!
//! ## Parity sources
//!
//! * Budget: codex `core-skills/src/render.rs`:
//!   - `SKILL_METADATA_CONTEXT_WINDOW_PERCENT = 2` (render.rs:18);
//!   - `DEFAULT_SKILL_METADATA_CHAR_BUDGET = 8_000` (render.rs:17);
//!   - `default_skill_metadata_budget(window)` = `window * 2 / 100` (min 1)
//!     tokens when a positive window is known, else 8000 characters
//!     (render.rs:143-158).
//!   Legacy threads the SAME `default_skill_metadata_budget` /
//!   `SkillMetadataBudget` through `build_local_available_skills`
//!   (lib.rs:16299-16302), so the 2% / 8000 numbers are authoritative for both.
//! * Token sizing: codex sizes a token budget with `approx_token_count`
//!   (= bytes/chars div_ceil 4); we REUSE the crate-shared
//!   [`crate::context::accounting::approx_tokens_from_byte_count_i64`] (the same
//!   `(b + 3) / 4` heuristic) rather than copying it, per the WP contract.
//! * Block envelope: codex
//!   `core/src/context/available_skills_instructions.rs` wraps
//!   `render_available_skills_body` in
//!   `SKILLS_INSTRUCTIONS_OPEN_TAG`/`CLOSE_TAG` with role `developer`; legacy
//!   `render_available_skills_instructions` (lib.rs:16281) joins
//!   `<skills_instructions>` + body + `</skills_instructions>`. The tag strings
//!   are `browser-use-core/src/constants.rs:46-47`
//!   (`<skills_instructions>` / `</skills_instructions>`).
//! * Body shape: codex `render_available_skills_body` (render.rs:62) =
//!   `## Skills` / intro / `### Available skills` / one `- name: description`
//!   line per skill (`render_skill_line` render.rs:433) / `### How to use
//!   skills`. We reproduce the absolute-paths variant (no alias table) since
//!   alias compaction is a later optimization (flagged as parity debt below).

use serde_json::{json, Value};

use crate::context::accounting::approx_tokens_from_byte_count_i64;

use super::discovery::Skill;

/// codex `SKILL_METADATA_CONTEXT_WINDOW_PERCENT` (render.rs:18).
pub const SKILL_METADATA_CONTEXT_WINDOW_PERCENT: i64 = 2;

/// codex `DEFAULT_SKILL_METADATA_CHAR_BUDGET` (render.rs:17). Used when the
/// context window is unknown.
pub const DEFAULT_SKILL_METADATA_CHAR_BUDGET: i64 = 8_000;

/// Opening tag of the injected block. Legacy `SKILLS_INSTRUCTIONS_OPEN_TAG`
/// (constants.rs:46).
pub const SKILLS_INSTRUCTIONS_OPEN_TAG: &str = "<skills_instructions>";
/// Closing tag of the injected block. Legacy `SKILLS_INSTRUCTIONS_CLOSE_TAG`
/// (constants.rs:47).
pub const SKILLS_INSTRUCTIONS_CLOSE_TAG: &str = "</skills_instructions>";

/// codex `SKILLS_INTRO_WITH_ABSOLUTE_PATHS` (render.rs:25) — the intro line used
/// when skills are listed with absolute paths (no alias table).
pub const SKILLS_INTRO_WITH_ABSOLUTE_PATHS: &str = "A skill is a set of local instructions to follow that is stored in a `SKILL.md` file. Below is the list of skills that can be used. Each entry includes a name, description, and file path so you can open the source for full instructions when using a specific skill.";

/// The budget used to size the skills block. Mirrors codex `SkillMetadataBudget`
/// (render.rs:86): either a token budget (positive context window known) or a
/// character budget (window unknown).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillMetadataBudget {
    /// `limit` model tokens.
    Tokens(i64),
    /// `limit` characters.
    Characters(i64),
}

impl SkillMetadataBudget {
    /// The numeric limit (tokens or chars).
    pub fn limit(self) -> i64 {
        match self {
            Self::Tokens(n) | Self::Characters(n) => n,
        }
    }

    /// The cost of `text` under this budget. Token budgets cost
    /// `approx_tokens_from_byte_count_i64(bytes)` (the SHARED heuristic),
    /// character budgets cost the `char` count — mirroring codex
    /// `SkillMetadataBudget::cost` (render.rs:99).
    pub fn cost(self, text: &str) -> i64 {
        match self {
            Self::Tokens(_) => approx_tokens_from_byte_count_i64(text.len() as i64),
            Self::Characters(_) => text.chars().count() as i64,
        }
    }
}

/// Compute the default skills budget from an optional context window, mirroring
/// codex `default_skill_metadata_budget` (render.rs:143): a positive window
/// yields `Tokens(window * 2 / 100)` (clamped to at least 1); otherwise
/// `Characters(8_000)`.
pub fn default_skill_metadata_budget(context_window: Option<i64>) -> SkillMetadataBudget {
    match context_window {
        Some(window) if window > 0 => {
            let tokens = window
                .saturating_mul(SKILL_METADATA_CONTEXT_WINDOW_PERCENT)
                .saturating_div(100)
                .max(1);
            SkillMetadataBudget::Tokens(tokens)
        }
        _ => SkillMetadataBudget::Characters(DEFAULT_SKILL_METADATA_CHAR_BUDGET),
    }
}

/// Outcome of rendering: how many skills were listed, how many had their
/// description dropped to fit, and how many were omitted entirely.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillRenderReport {
    /// Total skills considered.
    pub total_count: usize,
    /// Skills whose name appears in the rendered block.
    pub included_count: usize,
    /// Skills dropped entirely (not even their name fit).
    pub omitted_count: usize,
    /// Skills whose description was removed to fit the budget.
    pub truncated_description_count: usize,
}

/// Render the model-facing skills body (no envelope), budgeted, plus a report.
///
/// Skills are listed in the order given (callers list them highest-priority
/// first — discovery already returns precedence order). The full block (intro +
/// all `- name: description` lines + how-to-use footer) is emitted when it fits
/// the budget. When it does not, descriptions are removed from the END of the
/// list first (lowest priority) until the block fits; if even the bare-name
/// listing overflows, trailing skills are omitted entirely. This mirrors codex's
/// two-stage `render_skill_lines_from_lines` (render.rs:205) degrade path
/// (full-cost → minimum-cost → drop), simplified to a single deterministic pass.
pub fn render_skills_body(
    skills: &[Skill],
    budget: SkillMetadataBudget,
) -> (String, SkillRenderReport) {
    let total_count = skills.len();
    if total_count == 0 {
        return (String::new(), SkillRenderReport::default());
    }

    // `keep_description[i]` controls whether skill i renders its description.
    let mut keep_description = vec![true; total_count];
    let mut included = vec![true; total_count];

    // Stage 1: if the full block fits, we're done.
    if body_cost(skills, &keep_description, &included, budget) <= budget.limit() {
        return finalize(skills, &keep_description, &included, total_count);
    }

    // Stage 2: drop descriptions from the end (lowest priority) until it fits or
    // all descriptions are gone.
    for i in (0..total_count).rev() {
        keep_description[i] = false;
        if body_cost(skills, &keep_description, &included, budget) <= budget.limit() {
            return finalize(skills, &keep_description, &included, total_count);
        }
    }

    // Stage 3: even the bare-name listing overflows — omit trailing skills.
    for i in (0..total_count).rev() {
        included[i] = false;
        if body_cost(skills, &keep_description, &included, budget) <= budget.limit() {
            break;
        }
    }

    finalize(skills, &keep_description, &included, total_count)
}

/// Build the report + final body string from the keep/include decisions.
fn finalize(
    skills: &[Skill],
    keep_description: &[bool],
    included: &[bool],
    total_count: usize,
) -> (String, SkillRenderReport) {
    let mut included_count = 0usize;
    let mut omitted_count = 0usize;
    let mut truncated_description_count = 0usize;
    for i in 0..total_count {
        if !included[i] {
            omitted_count += 1;
            continue;
        }
        included_count += 1;
        if !keep_description[i] && skills[i].description.is_some() {
            truncated_description_count += 1;
        }
    }
    let body = build_body(skills, keep_description, included);
    let report = SkillRenderReport {
        total_count,
        included_count,
        omitted_count,
        truncated_description_count,
    };
    (body, report)
}

/// The cost of the body under the keep/include decisions.
fn body_cost(
    skills: &[Skill],
    keep_description: &[bool],
    included: &[bool],
    budget: SkillMetadataBudget,
) -> i64 {
    budget.cost(&build_body(skills, keep_description, included))
}

/// Assemble the body text (codex `render_available_skills_body`, absolute-paths
/// variant): `## Skills`, intro, `### Available skills`, the skill lines, and the
/// `### How to use skills` footer. Each skill line is
/// `- {name}: {description}` (or `- {name}` when the description is dropped /
/// absent), mirroring codex `render_skill_line` (render.rs:433). The path is
/// appended in parentheses so the model can open the source, matching the intro
/// promise of a "file path".
fn build_body(skills: &[Skill], keep_description: &[bool], included: &[bool]) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("## Skills".to_string());
    lines.push(SKILLS_INTRO_WITH_ABSOLUTE_PATHS.to_string());
    lines.push("### Available skills".to_string());
    for (i, skill) in skills.iter().enumerate() {
        if !included[i] {
            continue;
        }
        lines.push(skill_line(skill, keep_description[i]));
    }
    lines.push("### How to use skills".to_string());
    lines.push(SKILLS_HOW_TO_USE.to_string());
    format!("\n{}\n", lines.join("\n"))
}

/// One skill line: `- name: description (path)` (codex `render_skill_line`,
/// render.rs:433), or `- name (path)` when the description is dropped/absent.
fn skill_line(skill: &Skill, keep_description: bool) -> String {
    let path = skill.path.display();
    match skill.description.as_deref().filter(|_| keep_description) {
        Some(desc) if !desc.trim().is_empty() => {
            format!("- {}: {} ({})", skill.name, desc.trim(), path)
        }
        _ => format!("- {} ({})", skill.name, path),
    }
}

/// Condensed how-to-use footer. A faithful-in-spirit, shortened form of codex
/// `SKILLS_HOW_TO_USE_WITH_ABSOLUTE_PATHS` (render.rs:27): the long codex text is
/// product copy, not load-bearing wire shape; the trigger/`$Name` rule is the
/// load-bearing part and is preserved. (Flagged as parity debt: the full codex
/// footer text is not reproduced verbatim.)
const SKILLS_HOW_TO_USE: &str = "- If the user names a skill (with `$SkillName` or plain text) or the task clearly matches a skill's description above, use that skill for that turn by opening its `SKILL.md` at the listed path. Multiple mentions mean use them all; do not carry skills across turns unless re-mentioned. If a named skill isn't listed or its path can't be read, say so briefly and continue with the best fallback.";

/// Render the full `<skills_instructions>` developer-context message, or `None`
/// when there are no skills.
///
/// The returned [`Item`](crate::context::Item) is the
/// `{role:"developer", name:"skills_instructions", content:[{input_text}]}`
/// envelope used by the other context-message builders in
/// [`crate::context::inject`], with the body wrapped in the
/// `<skills_instructions>` … `</skills_instructions>` tags, matching legacy
/// `render_available_skills_instructions` (lib.rs:16303). The `name` tag value
/// (`"skills_instructions"`, derived from the open tag) lets downstream
/// accounting/rollback recognize the block.
pub fn render_skills_instructions(
    skills: &[Skill],
    context_budget_tokens: Option<i64>,
) -> Option<crate::context::Item> {
    if skills.is_empty() {
        return None;
    }
    let budget = default_skill_metadata_budget(context_budget_tokens);
    let (body, report) = render_skills_body(skills, budget);
    if report.included_count == 0 {
        return None;
    }
    let content =
        format!("{SKILLS_INSTRUCTIONS_OPEN_TAG}\n{body}\n{SKILLS_INSTRUCTIONS_CLOSE_TAG}");
    Some(json!({
        "role": "developer",
        "name": SKILLS_INSTRUCTIONS_NAME,
        "content": [{
            "type": "input_text",
            "text": content,
        }],
    }))
}

/// The `name` tag carried by the injected skills-instructions message
/// (`"skills_instructions"`, the open tag stripped of its angle brackets).
pub const SKILLS_INSTRUCTIONS_NAME: &str = "skills_instructions";

/// True iff `item` is the injected `<skills_instructions>` context message.
pub fn is_skills_instructions_message(item: &Value) -> bool {
    item.get("name").and_then(Value::as_str) == Some(SKILLS_INSTRUCTIONS_NAME)
}
