use codex_file_search::source_search::SOURCE_READ_MAX_LINES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_CONTEXT_LINES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_MATCHES;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub(crate) const SEARCH_SOURCE_TOOL_NAME: &str = "search_source";
pub(crate) const READ_FILE_SPAN_TOOL_NAME: &str = "read_file_span";

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
                    "Optional confined search roots. Empty searches the repository root."
                        .to_string(),
                ),
            ),
        ),
        (
            "max_results".to_string(),
            JsonSchema::integer(Some(format!(
                "Maximum matches to return; must be between 1 and {SOURCE_SEARCH_MAX_MATCHES}."
            ))),
        ),
        (
            "context_lines".to_string(),
            JsonSchema::integer(Some(format!(
                "Context lines before and after each match; must not exceed {SOURCE_SEARCH_MAX_CONTEXT_LINES}."
            ))),
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
    ]);
    add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: SEARCH_SOURCE_TOOL_NAME.to_string(),
        description: "Search repository source with fixed-string matching and hard scan/result limits. This tool supports local environments only. Results include repo-relative 1-based line-span evidence citations."
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

pub(crate) fn create_read_file_span_tool(options: SourceToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "path".to_string(),
            JsonSchema::string(Some(
                "Repo-relative source file path. Paths outside the repository are rejected."
                    .to_string(),
            )),
        ),
        (
            "start_line".to_string(),
            JsonSchema::integer(Some("First 1-based line to return.".to_string())),
        ),
        (
            "line_count".to_string(),
            JsonSchema::integer(Some(format!(
                "Number of lines to return; must be between 1 and {SOURCE_READ_MAX_LINES}."
            ))),
        ),
    ]);
    add_environment_id(&mut properties, options);

    ToolSpec::Function(ResponsesApiTool {
        name: READ_FILE_SPAN_TOOL_NAME.to_string(),
        description: "Read a bounded source-file span confined to the current repository. This tool supports local environments only. Output includes an explicit repo-relative 1-based line-span evidence citation."
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

#[cfg(test)]
#[path = "source_spec_tests.rs"]
mod tests;
