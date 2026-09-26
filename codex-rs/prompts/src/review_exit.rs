use codex_utils_template::Template;
use std::sync::LazyLock;

const REVIEW_EXIT_SUCCESS_TEMPLATE_TEXT: &str =
    include_str!("../templates/review/exit_success.xml");
const REVIEW_EXIT_INTERRUPTED_TEMPLATE_TEXT: &str =
    include_str!("../templates/review/exit_interrupted.xml");

static REVIEW_EXIT_SUCCESS_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    Template::parse_embedded(REVIEW_EXIT_SUCCESS_TEMPLATE_TEXT, "review/exit_success.xml")
});

pub fn render_review_exit_success(results: &str) -> String {
    // Reviewer output is raw text, not markup belonging to the user-action envelope.
    let results = codex_utils_string::xml_text(results).to_string();
    REVIEW_EXIT_SUCCESS_TEMPLATE
        .render([("results", results.as_str())])
        .unwrap_or_else(|err| panic!("review exit success template must render: {err}"))
}

pub fn render_review_exit_interrupted() -> String {
    REVIEW_EXIT_INTERRUPTED_TEMPLATE_TEXT.to_string()
}

#[cfg(test)]
#[path = "review_exit_tests.rs"]
mod review_exit_tests;
