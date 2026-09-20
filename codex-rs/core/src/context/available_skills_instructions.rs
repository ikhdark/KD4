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
        let mut skill_lines = available_skills.skill_lines.clone();
        if available_skills.report.omitted_count > 0 {
            skill_lines.push(format!(
                "Catalog incomplete: {} additional skills omitted to fit the context budget. An unlisted skill may still be available; use supplied instructions or a relevant discovery route when needed for the task.",
                available_skills.report.omitted_count
            ));
        }
        Self {
            skill_root_lines: available_skills.skill_root_lines.clone(),
            skill_lines,
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

    #[tokio::test]
    async fn bounded_catalog_reports_omissions_in_model_context() {
        use codex_core_skills::SkillMetadataBudget;
        use codex_core_skills::build_available_skills;
        use codex_core_skills::loader::SkillRoot;
        use codex_core_skills::loader::load_skills_from_roots;
        use codex_core_skills::render::SkillRenderSideEffects;
        use codex_utils_absolute_path::test_support::PathExt;
        use std::sync::Arc;

        let root = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta"] {
            let dir = root.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: Test skill.\n---\nInstructions.\n"),
            )
            .unwrap();
        }
        let outcome = load_skills_from_roots(
            vec![SkillRoot {
                path: root.path().abs(),
                scope: codex_protocol::protocol::SkillScope::Repo,
                file_system: Arc::clone(&codex_exec_server::LOCAL_FS),
                plugin_id: None,
                plugin_namespace: None,
                plugin_root: None,
            }],
            None,
        )
        .await;
        assert_eq!(outcome.skills.len(), 2);
        for (budget, omitted) in [(1, 2), (10_000, 0)] {
            let available = build_available_skills(
                &outcome,
                SkillMetadataBudget::Characters(budget),
                SkillRenderSideEffects::None,
            )
            .unwrap();
            assert_eq!(available.report.omitted_count, omitted);
            assert_eq!(available.skill_lines.len(), 2 - omitted);
            let rendered = AvailableSkillsInstructions::from_available_skills(&available).render();
            if omitted > 0 {
                assert!(
                    rendered.contains("Catalog incomplete: 2 additional skills omitted"),
                    "{rendered}"
                );
                assert!(rendered.contains("An unlisted skill may still be available"));
            } else {
                assert!(!rendered.contains("Catalog incomplete:"));
                assert!(rendered.contains("alpha") && rendered.contains("beta"));
            }
        }
    }

    #[test]
    fn skills_usage_fragment_renders_complete_instructions_for_developer_context() {
        // Session developer-section assembly consumes this normal render boundary.
        let rendered = SkillsUsageInstructions.render();
        let body = rendered
            .strip_prefix("<skills_usage_instructions>\n## How to use skills\n")
            .and_then(|text| text.strip_suffix("\n</skills_usage_instructions>"))
            .expect("complete skills-usage markers and heading");
        assert_eq!(body, SKILLS_HOW_TO_USE);
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
