use std::time::Duration;

use super::ContextualUserFragment;
use super::escape_fragment_delimiters;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UserShellCommand {
    pub(crate) command: String,
    pub(crate) exit_code: i32,
    pub(crate) duration_seconds: f64,
    pub(crate) output: String,
}

impl UserShellCommand {
    pub(crate) fn new(
        command: impl Into<String>,
        exit_code: i32,
        duration: Duration,
        output: impl Into<String>,
    ) -> Self {
        Self {
            command: command.into(),
            exit_code,
            duration_seconds: duration.as_secs_f64(),
            output: output.into(),
        }
    }
}

impl ContextualUserFragment for UserShellCommand {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<user_shell_command>", "</user_shell_command>")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned({
            let (open, close) = Self::type_markers();
            let delimiters = [
                open,
                close,
                "<command>",
                "</command>",
                "<result>",
                "</result>",
            ];
            format!(
                "\n<command>\n{}\n</command>\n<result>\nExit code: {}\nDuration: {:.4} seconds\nOutput:\n{}\n</result>\n",
                escape_fragment_delimiters(&self.command, &delimiters),
                self.exit_code,
                self.duration_seconds,
                escape_fragment_delimiters(&self.output, &delimiters),
            )
        })
    }
}
