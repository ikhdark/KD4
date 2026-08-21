use crate::MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_SUMMARY_TOKEN_LIMIT;
use crate::MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_TOKEN_LIMIT;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use codex_utils_template::Template;
use std::sync::LazyLock;
use tokio::fs;

static MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    parse_embedded_template(
        include_str!("../templates/memories/read_path.compact.md"),
        "memories/read_path.compact.md",
    )
});

fn parse_embedded_template(source: &'static str, template_name: &str) -> Template {
    match Template::parse(source) {
        Ok(template) => template,
        Err(err) => panic!("embedded template {template_name} is invalid: {err}"),
    }
}

/// Build the memory read-path prompt that is added to developer instructions.
///
/// Large `memory_summary.md` files are truncated at
/// [MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_SUMMARY_TOKEN_LIMIT], then reduced further
/// when needed to keep the complete rendered prompt within
/// [MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_TOKEN_LIMIT].
pub(crate) async fn build_memory_tool_developer_instructions(
    codex_home: &AbsolutePathBuf,
) -> Option<String> {
    let base_path = codex_home.join("memories");
    let memory_summary_path = base_path.join("memory_summary.md");
    let memory_summary = fs::read_to_string(&memory_summary_path)
        .await
        .ok()?
        .trim()
        .to_string();
    if memory_summary.is_empty() {
        return None;
    }
    let base_path = base_path.display().to_string();
    render_memory_tool_developer_instructions(base_path.as_str(), memory_summary.as_str())
}

fn render_memory_tool_developer_instructions(
    base_path: &str,
    memory_summary: &str,
) -> Option<String> {
    let memory_summary = truncate_text(
        memory_summary,
        TruncationPolicy::Tokens(MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_SUMMARY_TOKEN_LIMIT),
    );
    let fixed_instructions = MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_TEMPLATE
        .render([("base_path", base_path), ("memory_summary", "")])
        .ok()?;
    let fixed_token_count = approx_token_count(&fixed_instructions);
    let mut summary_token_limit =
        MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_TOKEN_LIMIT.checked_sub(fixed_token_count)?;
    if summary_token_limit == 0 {
        return None;
    }

    loop {
        let memory_summary =
            truncate_text_to_token_ceiling(memory_summary.as_str(), summary_token_limit);
        if memory_summary.is_empty() {
            return None;
        }
        let rendered = MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_TEMPLATE
            .render([
                ("base_path", base_path),
                ("memory_summary", memory_summary.as_str()),
            ])
            .ok()?;
        let rendered_token_count = approx_token_count(&rendered);
        if rendered_token_count <= MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_TOKEN_LIMIT {
            return Some(rendered);
        }

        // The estimator is deliberately lexical as well as byte based, so the
        // rendered template and summary are not perfectly additive. Reduce by
        // the observed overflow until the complete prompt fits the token cap.
        let token_overflow =
            rendered_token_count.saturating_sub(MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_TOKEN_LIMIT);
        let next_limit = summary_token_limit.saturating_sub(token_overflow.max(1));
        if next_limit == 0 || next_limit >= summary_token_limit {
            return None;
        }
        summary_token_limit = next_limit;
    }
}

#[cfg(test)]
#[path = "prompts_tests.rs"]
mod tests;
