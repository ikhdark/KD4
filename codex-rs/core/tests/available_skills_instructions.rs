use codex_core::context::AvailableSkillsInstructions;
use codex_core::context::ContextualUserFragment;
use codex_core_skills::AvailableSkills;
use codex_core_skills::SkillRenderReport;

fn available_skills(skill_root_lines: Vec<String>) -> AvailableSkills {
    AvailableSkills {
        skill_root_lines,
        skill_lines: vec!["- demo: example skill".to_string()],
        report: SkillRenderReport {
            total_count: 1,
            included_count: 1,
            omitted_count: 0,
            truncated_description_chars: 0,
            truncated_description_count: 0,
        },
        warning_message: None,
    }
}

#[test]
fn rendered_skill_catalog_does_not_repeat_shared_usage_guidance() {
    for skill_root_lines in [Vec::new(), vec!["- r0: C:\\workspace\\skills".to_string()]] {
        let rendered =
            AvailableSkillsInstructions::from_available_skills(&available_skills(skill_root_lines))
                .render();

        assert!(rendered.starts_with("<skills_instructions>"));
        assert!(rendered.ends_with("</skills_instructions>"));
        assert!(!rendered.contains("How to use skills"));
        assert!(!rendered.contains("read the selected `SKILL.md` completely"));
    }
}
