//! Network-free tests for the skills/plugins subsystem (tempdir fixtures only).

use std::fs;
use std::path::Path;

use tempfile::tempdir;

use super::discovery::{
    description_from_skill_md, discover_skills, frontmatter_value, strip_frontmatter, Skill,
    SkillRoot, SkillSource,
};
use super::inject::{
    default_skill_metadata_budget, is_skills_instructions_message, render_skills_body,
    render_skills_instructions, SkillMetadataBudget, SKILLS_INSTRUCTIONS_CLOSE_TAG,
    SKILLS_INSTRUCTIONS_NAME, SKILLS_INSTRUCTIONS_OPEN_TAG,
};
use super::mention::{parse_mentions, resolve_mentions, Mention};
use super::SkillsManager;

/// Lay down a `<root>/<dir>/SKILL.md` with the given contents.
fn write_skill(root: &Path, dir: &str, contents: &str) {
    let skill_dir = root.join(dir);
    fs::create_dir_all(&skill_dir).expect("create skill dir");
    fs::write(skill_dir.join("SKILL.md"), contents).expect("write SKILL.md");
}

fn skill_md(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: \"{name}\"\ndescription: \"{description}\"\n---\n\n{body}\n")
}

fn fixture_skill(name: &str, description: Option<&str>, source: SkillSource) -> Skill {
    Skill {
        name: name.to_string(),
        description: description.map(str::to_string),
        body: format!("# {name}\nbody"),
        path: std::path::PathBuf::from(format!("/tmp/{name}/SKILL.md")),
        source,
    }
}

// --------------------------------------------------------------------------
// Frontmatter parser
// --------------------------------------------------------------------------

#[test]
fn frontmatter_parser_reads_name_and_description() {
    let contents = skill_md(
        "imagegen",
        "Generate images",
        "# Image Generation\n\nDetails.",
    );
    assert_eq!(
        frontmatter_value(&contents, "name").as_deref(),
        Some("imagegen")
    );
    assert_eq!(
        description_from_skill_md(&contents).as_deref(),
        Some("Generate images")
    );
    // Body has the frontmatter removed.
    let body = strip_frontmatter(&contents).trim();
    assert!(body.starts_with("# Image Generation"));
    assert!(!body.contains("name:"));
}

#[test]
fn frontmatter_description_falls_back_to_metadata_then_body() {
    // No top-level `description`; metadata.short-description wins next.
    let with_meta =
        "---\nname: a\nmetadata:\n  short-description: From metadata\n---\n\n# A\n\nBody line.\n";
    assert_eq!(
        description_from_skill_md(with_meta).as_deref(),
        Some("From metadata")
    );

    // No description anywhere → first non-`#`, non-empty body line.
    let body_only = "---\nname: b\n---\n\n# Heading\n\nFirst real line.\nSecond.\n";
    assert_eq!(
        description_from_skill_md(body_only).as_deref(),
        Some("First real line.")
    );
}

#[test]
fn strip_frontmatter_is_noop_without_frontmatter() {
    let plain = "# No frontmatter\n\nJust body.";
    assert_eq!(strip_frontmatter(plain), plain);
}

// --------------------------------------------------------------------------
// Discovery + precedence
// --------------------------------------------------------------------------

#[test]
fn discovery_parses_name_description_body() {
    let dir = tempdir().unwrap();
    write_skill(
        dir.path(),
        "imagegen",
        &skill_md("imagegen", "Generate images", "# Image Gen\n\nWorkflow."),
    );

    let roots = [SkillRoot::new(dir.path(), SkillSource::User)];
    let skills = discover_skills(&roots);

    assert_eq!(skills.len(), 1);
    let s = &skills[0];
    assert_eq!(s.name, "imagegen");
    assert_eq!(s.description.as_deref(), Some("Generate images"));
    assert!(s.body.contains("Workflow."));
    assert_eq!(s.source, SkillSource::User);
    assert!(s.path.ends_with("imagegen/SKILL.md"));
}

#[test]
fn discovery_precedence_higher_root_wins_same_name() {
    let repo = tempdir().unwrap();
    let user = tempdir().unwrap();
    // Same skill name `foo` in two roots, different descriptions.
    write_skill(
        repo.path(),
        "foo",
        &skill_md("foo", "REPO version", "# Foo"),
    );
    write_skill(
        user.path(),
        "foo",
        &skill_md("foo", "USER version", "# Foo"),
    );
    // A distinct skill only in the user root.
    write_skill(user.path(), "bar", &skill_md("bar", "User bar", "# Bar"));

    // Repo is higher precedence than User → list repo root first.
    let roots = [
        SkillRoot::new(repo.path(), SkillSource::Repo),
        SkillRoot::new(user.path(), SkillSource::User),
    ];
    let skills = discover_skills(&roots);

    // `foo` from repo wins; `bar` from user also discovered. 2 distinct skills.
    assert_eq!(skills.len(), 2);
    let foo = skills
        .iter()
        .find(|s| s.name == "foo")
        .expect("foo present");
    assert_eq!(foo.description.as_deref(), Some("REPO version"));
    assert_eq!(foo.source, SkillSource::Repo);
    assert!(skills.iter().any(|s| s.name == "bar"));
}

#[test]
fn discovery_walks_nested_dirs_and_stops_at_skill_md() {
    let dir = tempdir().unwrap();
    // A skill nested two levels deep.
    write_skill(
        dir.path(),
        "group/nested",
        &skill_md("nested", "Nested skill", "# Nested"),
    );
    let roots = [SkillRoot::new(dir.path(), SkillSource::User)];
    let skills = discover_skills(&roots);
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "nested");
}

#[test]
fn discovery_skips_missing_roots() {
    let roots = [SkillRoot::new("/nonexistent/path/xyz", SkillSource::User)];
    assert!(discover_skills(&roots).is_empty());
}

// --------------------------------------------------------------------------
// Plugin discovery (precedence layer)
// --------------------------------------------------------------------------

#[test]
fn plugin_root_contributes_namespaced_skill_at_right_precedence() {
    let user = tempdir().unwrap();
    let plugin = tempdir().unwrap();
    write_skill(
        user.path(),
        "docs",
        &skill_md("docs", "User docs", "# Docs"),
    );
    write_skill(
        plugin.path(),
        "docs",
        &skill_md("docs", "Plugin docs", "# Docs"),
    );

    // User is higher precedence than Plugin → user first.
    let roots = [
        SkillRoot::new(user.path(), SkillSource::User),
        SkillRoot::plugin(plugin.path(), "Acme"),
    ];
    let skills = discover_skills(&roots);

    // Plugin namespaces its skill name as `Acme:docs`, so it does NOT collide
    // with the user `docs` — both are present.
    assert_eq!(skills.len(), 2);
    let plugin_skill = skills
        .iter()
        .find(|s| s.name == "Acme:docs")
        .expect("namespaced plugin skill present");
    assert_eq!(plugin_skill.source, SkillSource::Plugin);
    assert_eq!(plugin_skill.description.as_deref(), Some("Plugin docs"));
    assert!(skills.iter().any(|s| s.name == "docs"));
}

// --------------------------------------------------------------------------
// Budget
// --------------------------------------------------------------------------

#[test]
fn default_budget_is_two_percent_tokens_or_8000_chars() {
    // 2% of a 100_000-token window = 2000 tokens.
    assert_eq!(
        default_skill_metadata_budget(Some(100_000)),
        SkillMetadataBudget::Tokens(2_000)
    );
    // Tiny positive window clamps to at least 1 token.
    assert_eq!(
        default_skill_metadata_budget(Some(10)),
        SkillMetadataBudget::Tokens(1)
    );
    // Unknown / non-positive window → 8000-character budget.
    assert_eq!(
        default_skill_metadata_budget(None),
        SkillMetadataBudget::Characters(8_000)
    );
    assert_eq!(
        default_skill_metadata_budget(Some(0)),
        SkillMetadataBudget::Characters(8_000)
    );
}

#[test]
fn budget_token_cost_uses_shared_div_ceil_4_heuristic() {
    // Token cost must equal context::accounting::approx_tokens_from_byte_count_i64
    // of the rendered text length (the SHARED (b+3)/4 heuristic) — not a private
    // copy. Cross-check directly.
    let budget = SkillMetadataBudget::Tokens(1_000_000);
    let text = "abcdefghij"; // 10 bytes -> (10+3)/4 = 3 tokens.
    assert_eq!(budget.cost(text), 3);
    assert_eq!(
        budget.cost(text),
        crate::context::accounting::approx_tokens_from_byte_count_i64(text.len() as i64)
    );
}

#[test]
fn under_budget_includes_all_skills_with_descriptions() {
    let skills = vec![
        fixture_skill("alpha", Some("first skill"), SkillSource::User),
        fixture_skill("beta", Some("second skill"), SkillSource::User),
    ];
    // Generous character budget.
    let (body, report) = render_skills_body(&skills, SkillMetadataBudget::Characters(10_000));
    assert_eq!(report.total_count, 2);
    assert_eq!(report.included_count, 2);
    assert_eq!(report.omitted_count, 0);
    assert_eq!(report.truncated_description_count, 0);
    assert!(body.contains("- alpha: first skill"));
    assert!(body.contains("- beta: second skill"));
}

#[test]
fn over_budget_drops_descriptions_lowest_priority_first() {
    // Two skills, descriptions long enough that both descriptions can't fit but
    // the bare names + boilerplate can.
    let long = "x".repeat(400);
    let skills = vec![
        fixture_skill("alpha", Some(&long), SkillSource::User),
        fixture_skill("beta", Some(&long), SkillSource::User),
    ];
    // Budget large enough for the boilerplate (~713 chars) + both names +
    // exactly ONE 400-char description: full block ~1575, one-desc ~1173, so a
    // 1200-char budget keeps both names but drops the lowest-priority desc.
    let budget = SkillMetadataBudget::Characters(1_200);
    let (body, report) = render_skills_body(&skills, budget);

    assert_eq!(report.included_count, 2, "both names should still fit");
    assert!(
        report.truncated_description_count >= 1,
        "at least one description dropped to fit"
    );
    // The LAST (lowest-priority) skill loses its description first → beta's
    // description is gone, alpha keeps a `:` description line.
    assert!(body.contains("- beta ("), "beta rendered name-only");
    // The whole body fits the budget.
    assert!(budget.cost(&body) <= budget.limit());
}

#[test]
fn extreme_over_budget_omits_trailing_skills() {
    let long = "y".repeat(2_000);
    let skills = vec![
        fixture_skill("alpha", Some(&long), SkillSource::User),
        fixture_skill("beta", Some(&long), SkillSource::User),
        fixture_skill("gamma", Some(&long), SkillSource::User),
    ];
    // Tight budget: boilerplate (~713) + all three bare names (~801) overflows,
    // but boilerplate + two bare names (~771) fits, so the lowest-priority skill
    // (gamma) is omitted entirely while alpha/beta keep name-only lines.
    let budget = SkillMetadataBudget::Characters(780);
    let (body, report) = render_skills_body(&skills, budget);
    assert!(
        report.omitted_count >= 1,
        "some skills omitted under tight budget"
    );
    assert!(budget.cost(&body) <= budget.limit());
    // alpha (highest priority) is the last to be dropped.
    assert!(body.contains("- alpha"));
}

// --------------------------------------------------------------------------
// Injection shape
// --------------------------------------------------------------------------

#[test]
fn injection_wraps_body_in_skills_instructions_tag_and_envelope() {
    let skills = vec![fixture_skill("alpha", Some("desc"), SkillSource::User)];
    let item = render_skills_instructions(&skills, Some(100_000)).expect("some item");

    // Developer-role, name-tagged envelope matching the other context messages.
    assert_eq!(item.get("role").and_then(|v| v.as_str()), Some("developer"));
    assert_eq!(
        item.get("name").and_then(|v| v.as_str()),
        Some(SKILLS_INSTRUCTIONS_NAME)
    );
    assert!(is_skills_instructions_message(&item));

    let text = item
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())
        .expect("input_text content");
    assert!(text.starts_with(SKILLS_INSTRUCTIONS_OPEN_TAG));
    assert!(text.trim_end().ends_with(SKILLS_INSTRUCTIONS_CLOSE_TAG));
    assert!(text.contains("## Skills"));
    assert!(text.contains("- alpha: desc"));
}

#[test]
fn injection_none_when_no_skills() {
    assert!(render_skills_instructions(&[], Some(100_000)).is_none());
}

// --------------------------------------------------------------------------
// Mentions
// --------------------------------------------------------------------------

#[test]
fn mentions_parse_all_three_sigils() {
    let text = "use $alpha and @beta and [$x](skill://team/gamma) please";
    let mentions = parse_mentions(text);
    assert!(mentions.contains(&Mention::Tool("alpha".to_string())));
    assert!(mentions.contains(&Mention::Plugin("beta".to_string())));
    // The visible `$x` label inside the link is also captured as a Tool mention;
    // the structured target is the skill:// URI.
    assert!(mentions.contains(&Mention::Uri("team/gamma".to_string())));
}

#[test]
fn mentions_resolve_to_correct_skill_per_sigil() {
    let skills = vec![
        fixture_skill("alpha", Some("a"), SkillSource::User),
        fixture_skill("beta", Some("b"), SkillSource::User),
        fixture_skill("gamma", Some("g"), SkillSource::User),
    ];

    // $alpha
    let r = resolve_mentions("run $alpha now", &skills);
    assert_eq!(r.resolved.len(), 1);
    assert_eq!(r.resolved[0].name, "alpha");
    assert!(r.unresolved.is_empty());

    // @beta
    let r = resolve_mentions("run @beta now", &skills);
    assert_eq!(r.resolved.len(), 1);
    assert_eq!(r.resolved[0].name, "beta");

    // skill://team/gamma resolves by leaf segment.
    let r = resolve_mentions("see [$g](skill://team/gamma)", &skills);
    assert!(r.resolved.iter().any(|s| s.name == "gamma"));
}

#[test]
fn unknown_mention_is_reported_unresolved() {
    let skills = vec![fixture_skill("alpha", Some("a"), SkillSource::User)];
    let r = resolve_mentions("use $nope and skill://missing", &skills);
    assert!(r.resolved.is_empty());
    assert_eq!(r.unresolved.len(), 2);
    assert!(r.unresolved.contains(&Mention::Tool("nope".to_string())));
    assert!(r.unresolved.contains(&Mention::Uri("missing".to_string())));
}

#[test]
fn at_sigil_resolves_plugin_namespaced_skill_by_suffix() {
    let skills = vec![fixture_skill("Acme:docs", Some("d"), SkillSource::Plugin)];
    // `@docs` resolves the `Acme:docs` skill via the `:`-suffix rule.
    let r = resolve_mentions("open @docs", &skills);
    assert_eq!(r.resolved.len(), 1);
    assert_eq!(r.resolved[0].name, "Acme:docs");
}

#[test]
fn sigil_not_a_mention_mid_word_or_before_digit() {
    // `$5` is a price, `a@b.com` is an email — neither is a mention.
    let mentions = parse_mentions("pay $5 to a@b.com");
    assert!(mentions.is_empty(), "got {mentions:?}");
}

#[test]
fn mentions_dedupe_same_skill() {
    let skills = vec![fixture_skill("alpha", Some("a"), SkillSource::User)];
    let r = resolve_mentions("$alpha and @alpha and skill://alpha", &skills);
    assert_eq!(r.resolved.len(), 1, "same skill resolved once");
}

// --------------------------------------------------------------------------
// SkillsManager controller
// --------------------------------------------------------------------------

#[test]
fn manager_discovers_renders_and_resolves() {
    let repo = tempdir().unwrap();
    let user = tempdir().unwrap();
    write_skill(repo.path(), "foo", &skill_md("foo", "Repo foo", "# Foo"));
    write_skill(user.path(), "bar", &skill_md("bar", "User bar", "# Bar"));

    let roots = [
        SkillRoot::new(repo.path(), SkillSource::Repo),
        SkillRoot::new(user.path(), SkillSource::User),
    ];
    let mgr = SkillsManager::discover(&roots);
    assert_eq!(mgr.skills().len(), 2);

    // Render the block.
    let item = mgr.skills_instructions(Some(100_000)).expect("block");
    assert!(is_skills_instructions_message(&item));

    // Resolve a mention.
    let r = mgr.resolve("please $foo");
    assert_eq!(r.resolved.len(), 1);
    assert_eq!(r.resolved[0].name, "foo");
    assert_eq!(r.resolved[0].source, SkillSource::Repo);
}

#[test]
fn manager_empty_has_no_block() {
    let mgr = SkillsManager::new();
    assert!(mgr.skills_instructions(Some(100_000)).is_none());
    assert!(mgr.resolve("$anything").resolved.is_empty());
}
