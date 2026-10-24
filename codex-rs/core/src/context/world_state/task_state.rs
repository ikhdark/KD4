//! Recognize historical task-state fragments without producing new ones.
use crate::context::ContextualUserFragment;

pub(crate) struct TaskState;

impl ContextualUserFragment for TaskState {
    fn role(&self) -> &'static str { "user" }
    fn markers(&self) -> (&'static str, &'static str) { Self::type_markers() }
    fn type_markers() -> (&'static str, &'static str) {
        ("<codex_task_state>", "</codex_task_state>")
    }
    fn body(&self) -> std::borrow::Cow<'_, str> { "".into() }
}
