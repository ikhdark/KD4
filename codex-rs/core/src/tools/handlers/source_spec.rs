use codex_file_search::source_search::SOURCE_READ_MAX_LINES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_CONTEXT_LINES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_MATCHES;
use codex_file_search::task_locator::LOCATE_TASK_MAX_FILES;
use codex_file_search::task_locator::LOCATE_TASK_MAX_SOURCE_BYTES;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Number;
use std::collections::BTreeMap;

pub(crate) const SEARCH_SOURCE_TOOL_NAME: &str = "search_source";
pub(crate) const READ_FILE_SPAN_TOOL_NAME: &str = "read_file_span";
pub(crate) const LOCATE_TASK_TOOL_NAME: &str = "locate_task";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceToolOptions {
    pub(crate) include_environment_id: bool,
}

pub(crate) fn create_search_source_tool(options: SourceToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "query".to_string(),
            JsonSchema::string(Some(
                "Single-line fixed string to find in repository source files.".to_string(),
            )),
        ),
        (
            "paths".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some(
                    "Repo-relative file or directory to search.".to_string(),
                )),
                Some(
                    "Optional confined search roots. When ownership is unknown, call locate_task once and reuse its owner or closure paths here. Empty remains valid for deliberate repository-wide searches."
                        .to_string(),
                ),
            ),
        ),
        (
            "max_results".to_string(),
            bounded_integer(
                1,
                SOURCE_SEARCH_MAX_MATCHES,
                format!(
                    "Maximum matches to return; must be between 1 and {SOURCE_SEARCH_MAX_MATCHES}."
                ),
            ),
        ),
        (
            "context_lines".to_string(),
            bounded_integer(
                0,
                SOURCE_SEARCH_MAX_CONTEXT_LINES,
                format!(
                    "Context lines before and after each match; must not exceed {SOURCE_SEARCH_MAX_CONTEXT_LINES}."
                ),
            ),
        ),
        (
            "case_sensitive".to_string(),
            JsonSchema::boolean(Some("Use case-sensitive matching.".to_string())),
        ),
        (
            "include_generated".to_string(),
            JsonSchema::boolean(Some("Include generated/build-looking paths.".to_string())),
        ),
        (
            "include_vendor".to_string(),
            JsonSchema::boolean(Some("Include vendored dependency paths.".to_string())),
        ),
        (
            "include_locks".to_string(),
            JsonSchema::boolean(Some("Include lockfiles.".to_string())),
        ),
        (
            "hydrate_selected_span".to_string(),
            JsonSchema::boolean(Some(
                "When coverage and result indexing are complete, include exact bounded hydration from the same observed file bytes: the existing selected span for one match or a capped multi-match packet. Defaults to true."
                    .to_string(),
            )),
        ),
        (
            "force_fresh".to_string(),
            JsonSchema::boolean(Some(
                "Bypass exact source-search replay and execute the fresh bounded search. This does not make incomplete coverage authoritative or reopen unrelated closure."
                    .to_string(),
            )),
        ),
        ("source_question".to_string(), source_question_schema()),
    ]);
    add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: SEARCH_SOURCE_TOOL_NAME.to_string(),
        description: "Search repository source with fixed-string matching and hard scan/result limits. Prefer ownership-scoped paths: when the owner is unknown, call locate_task once and reuse its owner or closure paths. Complete results hydrate exact bounded source from the same observed bytes by default: one selected span for a unique match or a capped packet for multiple matches, with omissions and ambiguities reported explicitly. Use empty paths only for deliberate repository-wide work. This tool supports local environments only. Results include repo-relative 1-based line-span evidence citations."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["query".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub(crate) fn create_locate_task_tool(options: SourceToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "task".to_string(),
            JsonSchema::string(Some(
                "Concrete task description used for deterministic owner routing.".to_string(),
            )),
        ),
        (
            "path_anchor".to_string(),
            JsonSchema::string(Some(
                "Optional exact repository-relative file or directory anchor.".to_string(),
            )),
        ),
        (
            "symbol_anchor".to_string(),
            JsonSchema::string(Some("Optional exact source symbol anchor.".to_string())),
        ),
        (
            "max_files".to_string(),
            bounded_integer(
                1,
                LOCATE_TASK_MAX_FILES,
                format!(
                    "Maximum eligible files in the selected closure; hard-capped at {LOCATE_TASK_MAX_FILES}."
                ),
            ),
        ),
        (
            "max_source_bytes".to_string(),
            bounded_integer(
                1,
                LOCATE_TASK_MAX_SOURCE_BYTES,
                format!(
                    "Maximum aggregate captured source bytes; hard-capped at {LOCATE_TASK_MAX_SOURCE_BYTES}."
                ),
            ),
        ),
        (
            "force_fresh".to_string(),
            JsonSchema::boolean(Some(
                "Discard reusable syntax evidence for this query; defaults to false.".to_string(),
            )),
        ),
        ("source_question".to_string(), source_question_schema()),
    ]);
    add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: LOCATE_TASK_TOOL_NAME.to_string(),
        description: "Locate the owner, implementation span, source neighborhoods, conservative relationships, contracts, tests, validation, and governing instructions from one shared parser-backed source index. Call once and reuse the result for the same task and snapshot. Use read_file_span only for an exact missing detail; use narrowed search_source or an anchored locate_task for unresolved ambiguity. Local environments only; output is deterministically capped at 8 KiB."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, Some(vec!["task".to_string()]), Some(false.into())),
        output_schema: None,
    })
}

pub(crate) fn create_read_file_span_tool(options: SourceToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "path".to_string(),
            JsonSchema::string(Some(
                "Repo-relative source path, a `skill:<opaque-id>` catalog locator, or the exact absolute SKILL.md path of a loaded skill. Other outside paths are rejected."
                    .to_string(),
            )),
        ),
        (
            "start_line".to_string(),
            integer_with_minimum(1, "First 1-based line to return.".to_string()),
        ),
        (
            "line_count".to_string(),
            bounded_integer(
                1,
                SOURCE_READ_MAX_LINES,
                format!(
                    "Number of lines to return; must be between 1 and {SOURCE_READ_MAX_LINES}."
                ),
            ),
        ),
        (
            "force_fresh".to_string(),
            JsonSchema::boolean(Some(
                "Execute the read without reusing prior immutable evidence.".to_string(),
            )),
        ),
    ]);
    add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: READ_FILE_SPAN_TOOL_NAME.to_string(),
        description: "Read a bounded file span from the current repository or a loaded skill selected by opaque catalog locator or exact path. Repository reads support local environments only. Output includes an explicit 1-based line-span evidence citation."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["path".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn add_environment_id(properties: &mut BTreeMap<String, JsonSchema>, options: SourceToolOptions) {
    if options.include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "Select a local environment id from <environment_context>; omit only when the primary environment is local."
                    .to_string(),
            )),
        );
    }
}

fn source_question_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "kind".to_string(),
                JsonSchema::string_enum(
                    [
                        "unknown_caller",
                        "unknown_contract",
                        "ambiguous_ownership",
                        "incomplete_prior_result",
                        "source_changed",
                        "validation_dependency",
                    ]
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                    Some("Concrete reason the established source closure must expand.".to_string()),
                ),
            ),
            (
                "detail".to_string(),
                JsonSchema::string(Some(
                    "Non-empty missing evidence or ownership question.".to_string(),
                )),
            ),
        ]),
        Some(vec!["kind".to_string(), "detail".to_string()]),
        Some(false.into()),
    )
}

fn bounded_integer(minimum: usize, maximum: usize, description: String) -> JsonSchema {
    JsonSchema {
        minimum: Some(Number::from(minimum as u64)),
        maximum: Some(Number::from(maximum as u64)),
        ..JsonSchema::integer(Some(description))
    }
}

fn integer_with_minimum(minimum: usize, description: String) -> JsonSchema {
    JsonSchema {
        minimum: Some(Number::from(minimum as u64)),
        ..JsonSchema::integer(Some(description))
    }
}

#[cfg(test)]
#[path = "source_spec_tests.rs"]
mod tests;
