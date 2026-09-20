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

/// The catalog advertises `skill:` locators, so the guidance has to name the
/// tool that resolves them. Saying "their stated provider" without naming one
/// left the model with locators and no way to load them.
#[test]
fn skill_usage_guidance_names_the_tool_that_loads_skill_locators() {
    let guidance = codex_core_skills::SKILLS_HOW_TO_USE;
    assert!(
        guidance.contains("`skill:` locators with `read_file`"),
        "skill locators must name read_file as their provider: {guidance}"
    );
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
