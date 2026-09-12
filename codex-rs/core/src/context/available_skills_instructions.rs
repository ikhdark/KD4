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
    skill_root_lines: Vec<String>,
    skill_lines: Vec<String>,
}

impl AvailableSkillsInstructions {
    /// Creates a skills context fragment from pre-rendered catalog lines.
    pub fn from_skill_lines(skill_lines: Vec<String>) -> Self {
        Self {
            skill_root_lines: Vec::new(),
            skill_lines,
        }
    }

    pub fn from_available_skills(available_skills: &AvailableSkills) -> Self {
        Self {
            skill_root_lines: available_skills.skill_root_lines.clone(),
            skill_lines: available_skills.skill_lines.clone(),
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

    fn body(&self) -> String {
        format!("\n## How to use skills\n{SKILLS_HOW_TO_USE}\n")
    }
}

impl ContextualUserFragment for AvailableSkillsInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (SKILLS_INSTRUCTIONS_OPEN_TAG, SKILLS_INSTRUCTIONS_CLOSE_TAG)
    }

    fn body(&self) -> String {
        render_available_skills_body(&self.skill_root_lines, &self.skill_lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;

    #[test]
    fn skills_usage_fragment_renders_complete_instructions_for_developer_context() {
        // Session developer-section assembly consumes this normal render boundary.
        let rendered = SkillsUsageInstructions.render();
        let body = rendered
            .strip_prefix("<skills_usage_instructions>\n## How to use skills\n")
            .and_then(|text| text.strip_suffix("\n</skills_usage_instructions>"))
            .expect("complete skills-usage markers and heading");
        assert!(body.contains("read each selected `SKILL.md` completely"));
        assert!(body.contains("Do not delegate that reading or interpretation"));
        assert!(body.contains("Read task-required linked instructions"));
        assert!(body.contains("dedicated read-only route"));
        assert!(body.contains("orchestrator"));
        assert!(body.contains("state ordering when needed"));
        assert!(body.contains("named skill or required read is unavailable"));
        assert!(body.contains("relevant variants"));
        assert!(body.len() <= 1_000);

        // The shared fragment conversion must retain that content and its developer role.
        assert_eq!(
            ContextualUserFragment::into(SkillsUsageInstructions),
            ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText { text: rendered }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
        );
    }
}
