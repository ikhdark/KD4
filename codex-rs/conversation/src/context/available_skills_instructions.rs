use codex_core_skills::AvailableSkills;
use codex_core_skills::SKILLS_HOW_TO_USE;
use codex_core_skills::render_available_skills_body;
use codex_core_skills::rendered_skill_line_name;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_OPEN_TAG;

use super::ContextualUserFragment;

pub const SKILLS_USAGE_INSTRUCTIONS_OPEN_TAG: &str = "<skills_usage_instructions>";
const SKILLS_USAGE_INSTRUCTIONS_CLOSE_TAG: &str = "</skills_usage_instructions>";
const MAX_SKILL_INDEX_BYTES: usize = 2048;

/// Model-context fragment describing the skills available to Codex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableSkillsInstructions {
    body: String,
}

impl AvailableSkillsInstructions {
    /// Creates a skills context fragment from pre-rendered catalog lines.
    pub fn from_skill_lines(skill_lines: Vec<String>) -> Self {
        Self { body: render_available_skills_body(&skill_lines) }
    }

    /// Names only: this sits in every request's shared prefix, and a visible index answers
    /// "does a skill apply?" without a catalog probe on every task.
    pub fn from_available_skills(available_skills: &AvailableSkills) -> Self {
        let mut index = String::new();
        let mut unlisted = available_skills.report.omitted_count;
        for name in available_skills
            .skill_lines
            .iter()
            .filter_map(|line| rendered_skill_line_name(line))
        {
            if index.len() + name.len() > MAX_SKILL_INDEX_BYTES {
                unlisted += 1;
                continue;
            }
            if !index.is_empty() {
                index.push_str(", ");
            }
            index.push_str(name);
        }
        if unlisted > 0 {
            let separator = if index.is_empty() { "" } else { ", and " };
            index.push_str(&format!("{separator}{unlisted} more in skill:catalog"));
        }
        Self {
            body: format!(
                "\n## Skills\nAvailable: {index}.\nWhen the user names one or one clearly fits the requested workflow, read it with read_file(path=\"skill:<name>\") before acting. Otherwise do not look for skills; only when the user asks for a capability none of these names covers, read_file(path=\"skill:catalog\", selectors=[{{kind:\"search\", query:\"<keyword>\"}}]) searches their descriptions. Use provider-specific discovery for other skill sources.\n"
            ),
        }
    }
}

/// Singleton model-context fragment describing how to load and apply skills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillsUsageInstructions;

impl ContextualUserFragment for SkillsUsageInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            SKILLS_USAGE_INSTRUCTIONS_OPEN_TAG,
            SKILLS_USAGE_INSTRUCTIONS_CLOSE_TAG,
        )
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(format!("\n## How to use skills\n{SKILLS_HOW_TO_USE}\n"))
    }
}

impl ContextualUserFragment for AvailableSkillsInstructions {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (SKILLS_INSTRUCTIONS_OPEN_TAG, SKILLS_INSTRUCTIONS_CLOSE_TAG)
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.body)
    }
}

