use codex_tools::JsonSchema;
use codex_tools::TOOL_SEARCH_TOOL_NAME;
use codex_tools::ToolSearchSourceInfo;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub(crate) fn create_tool_search_tool(
    searchable_sources: &[ToolSearchSourceInfo],
    has_unnamed_tools: bool,
    default_limit: usize,
) -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "query".to_string(),
            JsonSchema::string(Some(
                "Search query for deferred tools. Must contain non-whitespace text and must not exceed 4,096 UTF-8 bytes."
                    .to_string(),
            )),
        ),
        (
            "limit".to_string(),
            JsonSchema::integer(Some(format!(
                "Maximum number of tools to return. Must be an integer from 1 through 64. Defaults to {default_limit}."
            ))),
        ),
    ]);

    let mut source_descriptions = BTreeMap::new();
    for source in searchable_sources {
        source_descriptions
            .entry(source.name.clone())
            .and_modify(|existing: &mut Option<String>| {
                if existing.is_none() {
                    *existing = source.description.clone();
                }
            })
            .or_insert(source.description.clone());
    }

    let source_descriptions = if source_descriptions.is_empty() {
        if has_unnamed_tools {
            "- Deferred built-in or extension tools (named source metadata is unavailable; these deferred tools remain searchable)."
                .to_string()
        } else {
            "None currently enabled.".to_string()
        }
    } else {
        let mut source_descriptions = source_descriptions
            .into_iter()
            .map(|(name, description)| match description {
                Some(description) => format!("- {name}: {description}"),
                None => format!("- {name}"),
            })
            .collect::<Vec<_>>();
        if has_unnamed_tools {
            source_descriptions.push("- Deferred built-in or extension tools".to_string());
        }
        source_descriptions.join("\n")
    };

    let description = format!(
        "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nYou have access to tools from the following sources:\n{source_descriptions}\nSome of the tools may not have been provided to you upfront, and you should use this tool (`{TOOL_SEARCH_TOOL_NAME}`) to search for the required tools. For MCP tool discovery, always use `{TOOL_SEARCH_TOOL_NAME}` instead of `list_mcp_resources` or `list_mcp_resource_templates`."
    );

    ToolSpec::ToolSearch {
        execution: "client".to_string(),
        description,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["query".to_string()]),
            Some(false.into()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_tools::JsonSchema;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeMap;

    #[test]
    fn create_tool_search_tool_deduplicates_and_renders_enabled_sources() {
        assert_eq!(
            create_tool_search_tool(
                &[
                    ToolSearchSourceInfo {
                        name: "Google Drive".to_string(),
                        description: Some(
                            "Use Google Drive as the single entrypoint for Drive, Docs, Sheets, and Slides work."
                                .to_string(),
                        ),
                    },
                    ToolSearchSourceInfo {
                        name: "Google Drive".to_string(),
                        description: None,
                    },
                    ToolSearchSourceInfo {
                        name: "docs".to_string(),
                        description: None,
                    },
                ],
                /*has_unnamed_tools*/ false,
                /*default_limit*/ 8,
            ),
            ToolSpec::ToolSearch {
                execution: "client".to_string(),
                description: "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nYou have access to tools from the following sources:\n- Google Drive: Use Google Drive as the single entrypoint for Drive, Docs, Sheets, and Slides work.\n- docs\nSome of the tools may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search` instead of `list_mcp_resources` or `list_mcp_resource_templates`.".to_string(),
                parameters: JsonSchema::object(BTreeMap::from([
                        (
                            "limit".to_string(),
                            JsonSchema::integer(Some(
                                    "Maximum number of tools to return. Must be an integer from 1 through 64. Defaults to 8."
                                        .to_string(),
                                ),),
                        ),
                        (
                            "query".to_string(),
                            JsonSchema::string(Some(
                                    "Search query for deferred tools. Must contain non-whitespace text and must not exceed 4,096 UTF-8 bytes."
                                        .to_string(),
                                ),),
                        ),
                    ]), Some(vec!["query".to_string()]), Some(false.into())),
            }
        );
    }

    #[test]
    fn create_tool_search_tool_describes_unnamed_deferred_tools() {
        let ToolSpec::ToolSearch { description, .. } =
            create_tool_search_tool(&[], /*has_unnamed_tools*/ true, 8)
        else {
            panic!("expected tool search specification");
        };

        assert!(description.contains("- Deferred built-in or extension tools"));
        assert!(description.contains("named source metadata is unavailable"));
        assert!(description.contains("these deferred tools remain searchable"));
        assert!(!description.contains("None currently enabled."));
    }

    #[test]
    fn create_tool_search_tool_describes_named_and_unnamed_tools() {
        let ToolSpec::ToolSearch { description, .. } = create_tool_search_tool(
            &[ToolSearchSourceInfo {
                name: "Google Drive".to_string(),
                description: Some("Search Drive files.".to_string()),
            }],
            /*has_unnamed_tools*/ true,
            8,
        ) else {
            panic!("expected tool search specification");
        };

        assert!(description.contains("- Google Drive: Search Drive files."));
        assert!(description.contains("- Deferred built-in or extension tools"));
    }
}
