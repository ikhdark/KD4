use codex_core_skills::render_available_skills_body;
use codex_core_skills::skill_instruction_role;
use codex_core_skills::skill_scope_label;
use codex_extension_api::ContextualUserFragment;
use codex_protocol::protocol::EXTENSION_SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::EXTENSION_SKILLS_INSTRUCTIONS_OPEN_TAG;
use codex_protocol::protocol::SkillScope;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AvailableSkillsInstructions {
    skill_lines: Vec<String>,
}

impl AvailableSkillsInstructions {
    pub(crate) fn from_skill_lines(skill_lines: Vec<String>) -> Self {
        Self { skill_lines }
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
        (
            EXTENSION_SKILLS_INSTRUCTIONS_OPEN_TAG,
            EXTENSION_SKILLS_INSTRUCTIONS_CLOSE_TAG,
        )
    }

    fn body(&self) -> String {
        render_available_skills_body(&[], &self.skill_lines)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillInstructions {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) contents: String,
    pub(crate) source_scope: Option<SkillScope>,
}

impl ContextualUserFragment for SkillInstructions {
    fn role(&self) -> &'static str {
        self.source_scope
            .map(skill_instruction_role)
            .unwrap_or("user")
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<skill>", "</skill>")
    }

    fn body(&self) -> String {
        let name = &self.name;
        let path = &self.path;
        let contents = &self.contents;
        let scope = self
            .source_scope
            .map(|scope| format!("\n<scope>{}</scope>", skill_scope_label(scope)))
            .unwrap_or_default();
        format!("\n<name>{name}</name>\n<path>{path}</path>{scope}\n{contents}\n")
    }
}
