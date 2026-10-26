use std::borrow::Cow;

use crate::model::SkillLoadOutcome;
use crate::model::SkillMetadata;
use crate::model::skill_catalog_id;
use codex_otel::SessionTelemetry;
use codex_otel::THREAD_SKILLS_DESCRIPTION_TRUNCATED_CHARS_METRIC;
use codex_otel::THREAD_SKILLS_ENABLED_TOTAL_METRIC;
use codex_otel::THREAD_SKILLS_KEPT_TOTAL_METRIC;
use codex_otel::THREAD_SKILLS_TRUNCATED_METRIC;
use codex_protocol::protocol::SkillScope;
use codex_utils_output_truncation::approx_token_count;

const SKILL_METADATA_CONTEXT_WINDOW_PERCENT: usize = 2;
const MAX_SKILL_METADATA_TOKEN_BUDGET: usize = 2_000;
const MAX_DEFAULT_CONTEXT_SKILL_DESCRIPTION_CHARS: usize = 240;
const TRUNCATED_SKILL_DESCRIPTION_SUFFIX: &str = "...";
const SKILL_DESCRIPTION_TRUNCATION_WARNING_THRESHOLD_CHARS: usize = 100;
pub const SKILL_DESCRIPTION_TRUNCATED_WARNING: &str = "Skill descriptions were shortened to fit the skills context budget. Codex can still see every skill, but some descriptions are shorter. Disable unused skills or plugins to leave more room for the rest.";
pub const SKILL_DESCRIPTIONS_REMOVED_WARNING_PREFIX: &str =
    "Exceeded skills context budget. All skill descriptions were removed and";
pub const SKILLS_INTRO_WITH_ABSOLUTE_PATHS: &str = "Load full instructions through the skill locator. For omitted or newly relevant host skills, search read_file(path=\"skill:catalog\") before assuming a capability is unavailable. For other sources, use their provider-specific discovery routes.";
pub const SKILLS_HOW_TO_USE: &str = r###"- Use the smallest skill set named by the user or clearly matched by the task. Announce each skill on first use in the conversation, state ordering when needed, and reassess relevance on later turns without repeating the announcement.
- Before task actions, the main agent must read each selected `SKILL.md` completely. Do not delegate that reading or interpretation.
- Load `skill:` locators with `read_file`, and file, environment, orchestrator, or custom resources through their stated provider; resolve relative references from the skill source.
- Read task-required linked instructions with the same mechanism, load only relevant variants, and reuse supplied scripts, templates, and assets.
- If a named skill or required read is unavailable, state it and use the safest fallback. Retry shell invocation/transport failures through a permitted dedicated read-only route; never bypass permission denials with another tool."###;

pub fn render_available_skills_body(skill_lines: &[String]) -> String {
    let capacity = "\n## Skills\n".len()
        + SKILLS_INTRO_WITH_ABSOLUTE_PATHS.len()
        + 1
        + "### Available skills\n".len()
        + skill_lines.iter().map(|line| line.len() + 1).sum::<usize>();
    let mut rendered = String::with_capacity(capacity);
    rendered.push_str("\n## Skills\n");
    rendered.push_str(SKILLS_INTRO_WITH_ABSOLUTE_PATHS);
    rendered.push('\n');
    rendered.push_str("### Available skills\n");
    for line in skill_lines {
        rendered.push_str(line);
        rendered.push('\n');
    }
    rendered
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillMetadataBudget {
    Tokens(usize),
    Characters(usize),
}

impl SkillMetadataBudget {
    fn limit(self) -> usize {
        match self {
            Self::Tokens(limit) | Self::Characters(limit) => limit,
        }
    }

    fn cost(self, text: &str) -> usize {
        match self {
            Self::Tokens(_) => approx_token_count(text),
            Self::Characters(_) => text.chars().count(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRenderReport {
    pub total_count: usize,
    pub included_count: usize,
    pub omitted_count: usize,
    pub truncated_description_chars: usize,
    pub truncated_description_count: usize,
}

#[derive(Clone, Copy)]
pub enum SkillRenderSideEffects<'a> {
    None,
    ThreadStart {
        session_telemetry: &'a SessionTelemetry,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableSkills {
    pub skill_lines: Vec<String>,
    pub report: SkillRenderReport,
    pub warning_message: Option<String>,
}

pub fn default_skill_metadata_budget(context_window: Option<i64>) -> SkillMetadataBudget {
    context_window
        .and_then(|window| usize::try_from(window).ok())
        .filter(|window| *window > 0)
        .map(|window| {
            SkillMetadataBudget::Tokens(
                window
                    .saturating_mul(SKILL_METADATA_CONTEXT_WINDOW_PERCENT)
                    .saturating_div(100)
                    .clamp(1, MAX_SKILL_METADATA_TOKEN_BUDGET),
            )
        })
        .unwrap_or(SkillMetadataBudget::Tokens(MAX_SKILL_METADATA_TOKEN_BUDGET))
}

pub fn build_available_skills(
    outcome: &SkillLoadOutcome,
    budget: SkillMetadataBudget,
    side_effects: SkillRenderSideEffects<'_>,
) -> Option<AvailableSkills> {
    let (selected, empty) = prepare_available_skills(outcome, budget);
    record_prepared_skills_side_effects(selected.as_ref(), empty, budget, side_effects);
    selected
}

pub(crate) type AvailableSkillsCache =
    std::sync::Mutex<Option<(SkillMetadataBudget, Option<AvailableSkills>, bool)>>;

impl crate::model::HostSkillsSnapshot {
    /// Reuse only rendering derived from this immutable snapshot and exact
    /// budget. Live warnings/telemetry still occur for each caller as before.
    pub fn available_skills(
        &self,
        budget: SkillMetadataBudget,
        side_effects: SkillRenderSideEffects<'_>,
    ) -> Option<AvailableSkills> {
        let cached = self.rendered_available_skills.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref().filter(|(cached_budget, _, _)| *cached_budget == budget)
            .map(|(_, available, empty)| (available.clone(), *empty));
        let (selected, empty) = cached.unwrap_or_else(|| {
            // Do not hold the cache lock while formatting. Concurrent cold
            // callers may do duplicate bounded work, never serialize polling.
            let prepared = prepare_available_skills(self.outcome(), budget);
            *self.rendered_available_skills.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((budget, prepared.0.clone(), prepared.1));
            prepared
        });
        record_prepared_skills_side_effects(selected.as_ref(), empty, budget, side_effects);
        selected
    }
}

fn prepare_available_skills(
    outcome: &SkillLoadOutcome,
    budget: SkillMetadataBudget,
) -> (Option<AvailableSkills>, bool) {
    let skills = outcome.allowed_skills_for_implicit_invocation();
    if skills.is_empty() {
        return (None, true);
    }
    let skill_lines = ordered_catalog_skill_lines(&skills);
    (build_available_skills_from_lines(skill_lines, skills.len(), budget), false)
}

fn record_prepared_skills_side_effects(
    selected: Option<&AvailableSkills>,
    empty: bool,
    budget: SkillMetadataBudget,
    side_effects: SkillRenderSideEffects<'_>,
) {
    if let Some(selected) = selected {
        record_available_skills_side_effects(selected, budget, side_effects);
    } else if empty {
        record_skill_render_side_effects(
            side_effects,
            /*total_count*/ 0,
            /*included_count*/ 0,
            /*omitted_count*/ 0,
            /*truncated_description_chars*/ 0,
        );
    }
}

fn build_available_skills_from_lines(
    skill_lines: Vec<SkillLine<'_>>,
    total_count: usize,
    budget: SkillMetadataBudget,
) -> Option<AvailableSkills> {
    if total_count == 0 {
        return None;
    }

    let (skill_lines, report) = render_skill_lines_from_lines(skill_lines, total_count, budget);
    let warning_message = if report.omitted_count > 0 {
        let skill_word = if report.omitted_count == 1 {
            "skill"
        } else {
            "skills"
        };
        let verb = if report.omitted_count == 1 {
            "was"
        } else {
            "were"
        };
        Some(format!(
            "{} {} additional {} {} not included in the model-visible skills list.",
            SKILL_DESCRIPTIONS_REMOVED_WARNING_PREFIX, report.omitted_count, skill_word, verb
        ))
    } else if report.average_truncated_description_chars()
        > SKILL_DESCRIPTION_TRUNCATION_WARNING_THRESHOLD_CHARS
    {
        Some(SKILL_DESCRIPTION_TRUNCATED_WARNING.to_string())
    } else {
        None
    };
    let available = AvailableSkills {
        skill_lines,
        report,
        warning_message,
    };
    Some(available)
}

fn record_available_skills_side_effects(
    available: &AvailableSkills,
    budget: SkillMetadataBudget,
    side_effects: SkillRenderSideEffects<'_>,
) {
    record_skill_render_side_effects(
        side_effects,
        available.report.total_count,
        available.report.included_count,
        available.report.omitted_count,
        available.report.truncated_description_chars,
    );
    if available.report.omitted_count > 0 || available.report.truncated_description_chars > 0 {
        tracing::info!(
            budget_limit = budget.limit(),
            total_skills = available.report.total_count,
            included_skills = available.report.included_count,
            omitted_skills = available.report.omitted_count,
            truncated_description_chars_per_skill =
                available.report.average_truncated_description_chars(),
            truncated_skill_descriptions = available.report.truncated_description_count,
            "truncated skill metadata to fit skills context budget"
        );
    }
}

fn record_skill_render_side_effects(
    side_effects: SkillRenderSideEffects<'_>,
    total_count: usize,
    included_count: usize,
    omitted_count: usize,
    truncated_description_chars: usize,
) {
    match side_effects {
        SkillRenderSideEffects::None => {}
        SkillRenderSideEffects::ThreadStart { session_telemetry } => {
            session_telemetry.histogram(
                THREAD_SKILLS_ENABLED_TOTAL_METRIC,
                i64::try_from(total_count).unwrap_or(i64::MAX),
                &[],
            );
            session_telemetry.histogram(
                THREAD_SKILLS_KEPT_TOTAL_METRIC,
                i64::try_from(included_count).unwrap_or(i64::MAX),
                &[],
            );
            session_telemetry.histogram(
                THREAD_SKILLS_TRUNCATED_METRIC,
                if omitted_count > 0 { 1 } else { 0 },
                &[],
            );
            session_telemetry.histogram(
                THREAD_SKILLS_DESCRIPTION_TRUNCATED_CHARS_METRIC,
                i64::try_from(truncated_description_chars).unwrap_or(i64::MAX),
                &[],
            );
        }
    }
}

fn render_skill_lines_from_lines(
    skill_lines: Vec<SkillLine<'_>>,
    total_count: usize,
    budget: SkillMetadataBudget,
) -> (Vec<String>, SkillRenderReport) {
    let full_cost = skill_lines.iter().fold(0usize, |used, line| {
        used.saturating_add(line.full_cost(budget))
    });
    if full_cost <= budget.limit() {
        let included = skill_lines
            .iter()
            .map(SkillLine::render_full)
            .collect::<Vec<_>>();

        return (
            included,
            skill_render_report(
                total_count,
                /*included_count*/ skill_lines.len(),
                /*omitted_count*/ 0,
                skill_lines
                    .iter()
                    .map(|line| line.truncated_chars(line.description_char_count()))
                    .sum(),
                skill_lines
                    .iter()
                    .filter(|line| line.truncated_chars(line.description_char_count()) > 0)
                    .count(),
            ),
        );
    }

    let minimum_cost = skill_lines.iter().fold(0usize, |used, line| {
        used.saturating_add(line.minimum_cost(budget))
    });
    if minimum_cost <= budget.limit() {
        let rendered = render_lines_with_description_budget(
            budget,
            &skill_lines,
            budget.limit().saturating_sub(minimum_cost),
        );
        let (truncated_description_chars, truncated_description_count) =
            sum_description_truncation(&rendered);
        let included = rendered
            .into_iter()
            .map(|rendered| rendered.line)
            .collect::<Vec<_>>();

        return (
            included,
            skill_render_report(
                total_count,
                /*included_count*/ skill_lines.len(),
                /*omitted_count*/ 0,
                truncated_description_chars,
                truncated_description_count,
            ),
        );
    }

    render_minimum_skill_lines_until_budget(budget, skill_lines, total_count)
}

fn render_minimum_skill_lines_until_budget(
    budget: SkillMetadataBudget,
    skill_lines: Vec<SkillLine<'_>>,
    total_count: usize,
) -> (Vec<String>, SkillRenderReport) {
    let mut included = Vec::new();
    let mut used = 0usize;
    let mut omitted_count = 0usize;
    let mut truncated_description_chars = 0usize;
    let mut truncated_description_count = 0usize;
    for line in skill_lines {
        let line_cost = line.minimum_cost(budget);
        let description_char_count = line.original_description_chars;
        if used.saturating_add(line_cost) <= budget.limit() {
            used = used.saturating_add(line_cost);
            included.push(line.render_minimum());
        } else {
            omitted_count = omitted_count.saturating_add(1);
        }

        truncated_description_chars =
            truncated_description_chars.saturating_add(description_char_count);
        if description_char_count > 0 {
            truncated_description_count = truncated_description_count.saturating_add(1);
        }
    }

    let report = skill_render_report(
        total_count,
        included.len(),
        omitted_count,
        truncated_description_chars,
        truncated_description_count,
    );

    (included, report)
}

fn skill_render_report(
    total_count: usize,
    included_count: usize,
    omitted_count: usize,
    truncated_description_chars: usize,
    truncated_description_count: usize,
) -> SkillRenderReport {
    SkillRenderReport {
        total_count,
        included_count,
        omitted_count,
        truncated_description_chars,
        truncated_description_count,
    }
}

impl SkillRenderReport {
    fn average_truncated_description_chars(&self) -> usize {
        if self.total_count == 0 || self.truncated_description_chars == 0 {
            return 0;
        }

        self.truncated_description_chars
            .saturating_add(self.total_count.saturating_sub(1))
            / self.total_count
    }
}

struct SkillLine<'a> {
    name: &'a str,
    scope: &'static str,
    description: Cow<'a, str>,
    original_description_chars: usize,
    retained_source_chars: usize,
    path: String,
}

struct RenderedSkillLine {
    line: String,
    truncated_chars: usize,
}

struct DescriptionBudgetLine<'a> {
    line: &'a SkillLine<'a>,
    description_char_count: usize,
    extra_costs: Vec<usize>,
}

fn sum_description_truncation(rendered: &[RenderedSkillLine]) -> (usize, usize) {
    rendered
        .iter()
        .fold((0usize, 0usize), |(chars, count), line| {
            if line.truncated_chars == 0 {
                (chars, count)
            } else {
                (
                    chars.saturating_add(line.truncated_chars),
                    count.saturating_add(1),
                )
            }
        })
}

impl<'a> SkillLine<'a> {
    fn catalog(skill: &'a SkillMetadata) -> Self {
        let summary = skill
            .short_description
            .as_deref()
            .or_else(|| {
                skill
                    .interface
                    .as_ref()
                    .and_then(|interface| interface.short_description.as_deref())
            })
            .unwrap_or(skill.description.as_str());
        let description = truncate_default_context_skill_description(summary);
        let original_description_chars = summary.chars().count();
        let retained_source_chars = if matches!(&description, Cow::Owned(_)) {
            description
                .chars()
                .count()
                .saturating_sub(TRUNCATED_SKILL_DESCRIPTION_SUFFIX.chars().count())
        } else {
            original_description_chars
        };
        Self {
            name: skill.name.as_str(),
            scope: crate::skill_scope_label(skill.scope),
            description,
            original_description_chars,
            retained_source_chars,
            path: format!("{SKILL_CATALOG_LOCATOR_PREFIX}{}", skill_catalog_id(skill)),
        }
    }

    fn full_cost(&self, budget: SkillMetadataBudget) -> usize {
        line_cost(budget, &self.render_full())
    }

    fn minimum_cost(&self, budget: SkillMetadataBudget) -> usize {
        line_cost(budget, &self.render_minimum())
    }

    fn description_char_count(&self) -> usize {
        self.description.chars().count()
    }

    fn truncated_chars(&self, rendered_chars: usize) -> usize {
        self.original_description_chars
            .saturating_sub(rendered_chars.min(self.retained_source_chars))
    }

    fn render_full(&self) -> String {
        self.render_with_description(self.description.as_ref())
    }

    fn render_minimum(&self) -> String {
        self.render_with_description("")
    }

    fn rendered_description_prefix_len(&self, description_chars: usize) -> usize {
        self.description
            .char_indices()
            .nth(description_chars)
            .map_or(self.description.len(), |(idx, _)| idx)
    }

    fn render_with_description_chars(&self, description_chars: usize) -> String {
        let end = self.rendered_description_prefix_len(description_chars);
        let description = &self.description.as_ref()[..end];
        self.render_with_description(description)
    }

    fn render_with_description(&self, description: &str) -> String {
        let empty = description.is_empty();
        let name = codex_utils_string::xml_text(self.name);
        let description = codex_utils_string::xml_text(description);
        let scope = self.scope;
        if empty {
            format!("- {name} — {} (scope: {scope})", self.path)
        } else {
            format!("- {name} — {description} — {} (scope: {scope})", self.path)
        }
    }
}

fn truncate_default_context_skill_description(description: &str) -> Cow<'_, str> {
    if description
        .char_indices()
        .nth(MAX_DEFAULT_CONTEXT_SKILL_DESCRIPTION_CHARS)
        .is_none()
    {
        return Cow::Borrowed(description);
    }

    let prefix_chars = MAX_DEFAULT_CONTEXT_SKILL_DESCRIPTION_CHARS
        .saturating_sub(TRUNCATED_SKILL_DESCRIPTION_SUFFIX.chars().count());
    let hard_prefix_end = description
        .char_indices()
        .nth(prefix_chars)
        .map_or(description.len(), |(index, _)| index);
    let hard_prefix = &description[..hard_prefix_end];
    let prefix_end = hard_prefix
        .char_indices()
        .rev()
        .find_map(|(index, character)| character.is_whitespace().then_some(index))
        .filter(|index| *index > 0)
        .unwrap_or(hard_prefix_end);
    let mut truncated = description[..prefix_end].trim_end().to_string();
    truncated.push_str(TRUNCATED_SKILL_DESCRIPTION_SUFFIX);
    Cow::Owned(truncated)
}

impl<'a> DescriptionBudgetLine<'a> {
    fn new(line: &'a SkillLine<'a>, budget: SkillMetadataBudget) -> Self {
        let description_char_count = line.description_char_count();
        if description_char_count == 0 {
            return Self { line, description_char_count, extra_costs: vec![0] };
        }
        let minimum_line = line.render_minimum();
        let minimum_cost = line_cost(budget, &minimum_line);
        // Replacing one ASCII placeholder with the counted description and
        // adding the final newline cancels exactly in bytes and characters.
        let nonempty_line = line.render_with_description("x");
        let base_bytes = nonempty_line.len();
        let base_chars = nonempty_line.chars().count();

        #[derive(Default)]
        struct EncodedLength {
            bytes: usize,
            chars: usize,
        }
        impl std::fmt::Write for EncodedLength {
            fn write_str(&mut self, text: &str) -> std::fmt::Result {
                self.bytes = self.bytes.saturating_add(text.len());
                self.chars = self.chars.saturating_add(text.chars().count());
                Ok(())
            }
        }
        use std::fmt::Write as _;
        let mut encoded = EncodedLength::default();

        let mut extra_costs = Vec::with_capacity(description_char_count.saturating_add(1));
        extra_costs.push(0);

        for ch in line.description.chars() {
            // Count the final encoding: one source character can expand to an
            // entity, and neither character nor token budgets may ignore it.
            let mut bytes = [0; 4];
            let _ = write!(&mut encoded, "{}", codex_utils_string::xml_text(ch.encode_utf8(&mut bytes)));
            let rendered_cost = match budget {
                SkillMetadataBudget::Tokens(_) => codex_utils_string::TokenCountEstimate::from_byte_count(
                    base_bytes.saturating_add(encoded.bytes),
                ).tokens(),
                SkillMetadataBudget::Characters(_) => base_chars.saturating_add(encoded.chars),
            };
            let cost = rendered_cost.saturating_sub(minimum_cost);
            extra_costs.push(cost);
        }

        Self {
            line,
            description_char_count,
            extra_costs,
        }
    }
}

fn line_cost(budget: SkillMetadataBudget, line: &str) -> usize {
    budget.cost(&format!("{line}\n"))
}

fn render_lines_with_description_budget(
    budget: SkillMetadataBudget,
    skill_lines: &[SkillLine<'_>],
    limit: usize,
) -> Vec<RenderedSkillLine> {
    let budget_lines = skill_lines
        .iter()
        .map(|line| DescriptionBudgetLine::new(line, budget))
        .collect::<Vec<_>>();
    let mut char_allocations = vec![0usize; budget_lines.len()];
    let mut current_extra_costs = vec![0usize; budget_lines.len()];
    let mut remaining = limit;

    // Distribute description space one character at a time across skills.
    // Short descriptions naturally drop out, so their unused share can go to
    // longer descriptions instead of being stranded in a fixed per-skill quota.
    loop {
        let mut changed = false;
        for (index, line) in budget_lines.iter().enumerate() {
            if char_allocations[index] >= line.description_char_count {
                continue;
            }

            let current_cost = current_extra_costs[index];
            let next_chars = char_allocations[index].saturating_add(1);
            let next_cost = line.extra_costs[next_chars];
            let delta = next_cost.saturating_sub(current_cost);
            if delta <= remaining {
                char_allocations[index] = next_chars;
                current_extra_costs[index] = next_cost;
                remaining = remaining.saturating_sub(delta);
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    budget_lines
        .iter()
        .zip(char_allocations)
        .map(|(line, description_chars)| {
            let truncated_chars = line.line.truncated_chars(description_chars);
            RenderedSkillLine {
                line: line.line.render_with_description_chars(description_chars),
                truncated_chars,
            }
        })
        .collect()
}

fn ordered_catalog_skill_lines(skills: &[SkillMetadata]) -> Vec<SkillLine<'_>> {
    ordered_skills_for_budget(skills)
        .into_iter()
        .map(SkillLine::catalog)
        .collect()
}

fn ordered_skills_for_budget(skills: &[SkillMetadata]) -> Vec<&SkillMetadata> {
    let mut ordered = skills.iter().collect::<Vec<_>>();
    ordered.sort_by(|a, b| {
        prompt_scope_rank(a.scope)
            .cmp(&prompt_scope_rank(b.scope))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path_to_skills_md.cmp(&b.path_to_skills_md))
    });
    ordered
}

fn prompt_scope_rank(scope: SkillScope) -> u8 {
    match scope {
        SkillScope::System => 0,
        SkillScope::Admin => 1,
        SkillScope::Repo => 2,
        SkillScope::User => 3,
    }
}
