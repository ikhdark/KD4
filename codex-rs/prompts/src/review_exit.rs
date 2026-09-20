use codex_utils_template::Template;
use std::borrow::Cow;
use std::sync::LazyLock;

const REVIEW_EXIT_SUCCESS_TEMPLATE_TEXT: &str =
    include_str!("../templates/review/exit_success.xml");
const REVIEW_EXIT_INTERRUPTED_TEMPLATE_TEXT: &str =
    include_str!("../templates/review/exit_interrupted.xml");

static REVIEW_EXIT_SUCCESS_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    let normalized = normalize_review_template_line_endings(REVIEW_EXIT_SUCCESS_TEMPLATE_TEXT);
    Template::parse(normalized.as_ref())
        .unwrap_or_else(|err| panic!("review exit success template must parse: {err}"))
});

pub fn render_review_exit_success(results: &str) -> String {
    // Reviewer output is raw text, not markup belonging to the user-action envelope.
    let results = codex_utils_string::xml_text(results).to_string();
    REVIEW_EXIT_SUCCESS_TEMPLATE
        .render([("results", results.as_str())])
        .unwrap_or_else(|err| panic!("review exit success template must render: {err}"))
}

pub fn render_review_exit_interrupted() -> String {
    normalize_review_template_line_endings(REVIEW_EXIT_INTERRUPTED_TEMPLATE_TEXT).into_owned()
}

fn normalize_review_template_line_endings(template: &str) -> Cow<'_, str> {
    codex_utils_string::normalize_newlines(template)
}

#[cfg(test)]
#[path = "review_exit_tests.rs"]
mod review_exit_tests;
