use codex_core_skills::AvailableSkills;
use codex_core_skills::SKILLS_HOW_TO_USE;
use codex_core_skills::render_available_skills_body;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_OPEN_TAG;

use super::ContextualUserFragment;

pub(crate) const SKILLS_USAGE_INSTRUCTIONS_OPEN_TAG: &str = "<skills_usage_instructions>";
const SKILLS_USAGE_INSTRUCTIONS_CLOSE_TAG: &str = "</skills_usage_instructions>";

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

    pub fn from_available_skills(_available_skills: &AvailableSkills) -> Self {
        Self {
            body: "\n## Skills\nBefore task actions, discover applicable or named skills with read_file(path=\"skill:catalog\") and read their full instructions; use provider-specific discovery for other skill sources.\n".to_string(),
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

