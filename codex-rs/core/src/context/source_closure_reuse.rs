use super::ContextualUserFragment;

pub(crate) const SOURCE_CLOSURE_REUSE_OPEN_TAG: &str = "<source_closure_reuse>";
pub(crate) const SOURCE_CLOSURE_REUSE_CLOSE_TAG: &str = "</source_closure_reuse>";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourceClosureReuseContext;

impl ContextualUserFragment for SourceClosureReuseContext {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn body(&self) -> String {
        concat!(
            "The prior turn's source closure is still current: its planning material revision ",
            "and every exact implementation-file hash match. Reuse the established owner, ",
            "dependency closure, implementation surfaces, and validation route instead of ",
            "rediscovering them. Reopen discovery if the current user request conflicts with ",
            "that closure or execution produces contrary evidence."
        )
        .to_string()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            SOURCE_CLOSURE_REUSE_OPEN_TAG,
            SOURCE_CLOSURE_REUSE_CLOSE_TAG,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_hash_guarded_reuse_directive() {
        let rendered = SourceClosureReuseContext.render();
        assert!(rendered.starts_with(SOURCE_CLOSURE_REUSE_OPEN_TAG));
        assert!(rendered.contains("every exact implementation-file hash match"));
        assert!(rendered.ends_with(SOURCE_CLOSURE_REUSE_CLOSE_TAG));
    }
}
