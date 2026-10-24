use codex_core_skills::render_available_skills_body;
use codex_core_skills::skill_instruction_role;
use codex_core_skills::skill_scope_label;
use codex_extension_api::ContextualUserFragment;
use codex_protocol::protocol::EXTENSION_SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::EXTENSION_SKILLS_INSTRUCTIONS_OPEN_TAG;
use codex_protocol::protocol::SkillScope;
use codex_utils_string::xml_text as escape_fragment_text;

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
        "user"
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

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(render_available_skills_body(&self.skill_lines))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillInstructions {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) contents: String,
    pub(crate) source_scope: Option<SkillScope>,
}

impl SkillInstructions {
    pub(crate) fn rendered_len(&self) -> usize {
        Self::rendered_len_parts(&self.name, &self.path, &[&self.contents], self.source_scope)
    }

    pub(crate) fn rendered_len_parts(
        name: &str,
        path: &str,
        contents: &[&str],
        scope: Option<SkillScope>,
    ) -> usize {
        struct Counter(usize);
        impl std::fmt::Write for Counter {
            fn write_str(&mut self, text: &str) -> std::fmt::Result {
                self.0 = self.0.saturating_add(text.len());
                Ok(())
            }
        }
        let mut counter = Counter("<skill></skill>".len());
        let _ = Self::write_body(&mut counter, name, path, contents, scope);
        counter.0
    }

    fn write_body(
        output: &mut impl std::fmt::Write,
        name: &str,
        path: &str,
        contents: &[&str],
        scope: Option<SkillScope>,
    ) -> std::fmt::Result {
        write!(output, "\n<name>{}</name>\n<path>{}</path>",
            escape_fragment_text(name), escape_fragment_text(path))?;
        if let Some(scope) = scope {
            write!(output, "\n<scope>{}</scope>", skill_scope_label(scope))?;
        }
        output.write_str("\n")?;
        for part in contents {
            write!(output, "{}", escape_fragment_text(part))?;
        }
        output.write_str("\n")
    }
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

    fn body(&self) -> std::borrow::Cow<'_, str> {
        let mut body = String::new();
        let _ = Self::write_body(&mut body, &self.name, &self.path, &[&self.contents], self.source_scope);
        std::borrow::Cow::Owned(body)
    }
}
