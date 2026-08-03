use crate::MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_SUMMARY_TOKEN_LIMIT;
use crate::MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_TOKEN_LIMIT;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_bytes_for_tokens;
use codex_utils_output_truncation::truncate_text;
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
    let total_byte_limit = approx_bytes_for_tokens(MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_TOKEN_LIMIT);
    let mut summary_byte_limit = total_byte_limit.checked_sub(fixed_instructions.len())?;
    if summary_byte_limit == 0 {
        return None;
    }

    loop {
        let memory_summary = truncate_text(
            memory_summary.as_str(),
            TruncationPolicy::Bytes(summary_byte_limit),
        );
        if memory_summary.is_empty() {
            return None;
        }
        let rendered = MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_TEMPLATE
            .render([
                ("base_path", base_path),
                ("memory_summary", memory_summary.as_str()),
            ])
            .ok()?;
        if rendered.len() <= total_byte_limit {
            return Some(rendered);
        }

        // Byte truncation includes its omission marker in addition to the
        // requested budget. Account for that marker and retry until the whole
        // rendered prompt, not just the summary, fits the hard ceiling.
        let overflow = rendered.len() - total_byte_limit;
        let next_limit = summary_byte_limit.saturating_sub(overflow.max(1));
        if next_limit == 0 || next_limit >= summary_byte_limit {
            return None;
        }
        summary_byte_limit = next_limit;
    }
}

#[cfg(test)]
#[path = "prompts_tests.rs"]
mod tests;
