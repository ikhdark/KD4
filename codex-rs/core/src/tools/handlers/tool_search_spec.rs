use codex_tools::JsonSchema;
use codex_tools::TOOL_SEARCH_TOOL_NAME;
use codex_tools::LoadableToolSpec;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSearchSourceInfo;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub(crate) fn create_tool_search_tool(default_limit: usize) -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "query".to_string(),
            JsonSchema::string(Some(
                "Short capability terms or exact tool names. +term requires a metadata match; source:<canonical namespace> restricts membership to that namespace (scope tokens appear in the source catalog; shared namespaces may contain several connectors). At most one scope; omit it for broad discovery. Unknown snake_case names fail in name-only lookups; mixed capability queries retain identifiers as task terms, not fuzzy callable aliases. Must contain capability terms and must not exceed 4,096 UTF-8 bytes."
                    .to_string(),
            )),
        ),
        (
            "limit".to_string(),
            JsonSchema {
                minimum: Some(serde_json::Number::from(1_u64)),
                maximum: Some(serde_json::Number::from(64_u64)),
                ..JsonSchema::integer(Some(format!(
                    "Maximum number of relevant tools to return and activate. Weak matches return names only, without activation; refine the query or resolve an exact name. Must be an integer from 1 through 64. Defaults to {default_limit}, except an entire query uniquely identifying one callable defaults to that callable only. An explicit limit preserves broader requests."
                )))
            },
        ),
    ]);

    let description = format!(
        "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call. An exact deferred-tool name may be called directly; the router resolves and activates that registered capability atomically.\n\nAvailable sources are listed in the latest <tool_search_sources> context message, not in this stable contract. Use `{TOOL_SEARCH_TOOL_NAME}` to discover or disambiguate deferred capabilities. For MCP tool discovery, always use `{TOOL_SEARCH_TOOL_NAME}` instead of `list_mcp_resources` or `list_mcp_resource_templates`."
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

pub(crate) fn render_tool_search_sources(
    searchable_sources: &[ToolSearchSourceInfo],
    has_unnamed_tools: bool,
    search_infos: &[ToolSearchInfo],
) -> String {
    let mut scopes = BTreeMap::<&str, BTreeSet<&str>>::new();
    let mut owners = BTreeMap::<&str, BTreeSet<Option<&str>>>::new();
    for info in search_infos {
        let scope = canonical_source(info);
        let name = info.source_info.as_ref().map(|source| source.name.as_str());
        scopes.entry(name.unwrap_or("")).or_default().insert(scope);
        owners.entry(scope).or_default().insert(name);
    }
    let scope_label = |name: &str| {
        let Some(tokens) = scopes.get(name) else { return String::new(); };
        let mut bytes = 0;
        let labels = tokens.iter().take(8).filter_map(|scope| {
            let label = format!("source:{scope}{}", if owners[scope].len() > 1 { " (shared)" } else { "" });
            bytes += label.len();
            (bytes <= 512).then_some(label)
        }).collect::<Vec<_>>();
        let omitted = tokens.len() - labels.len();
        format!(" [{}{}]", labels.join(", "), if omitted == 0 { String::new() } else { format!("; {omitted} more scopes") })
    };
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

    if source_descriptions.is_empty() {
        if has_unnamed_tools {
            format!("- Deferred built-in or extension tools{} (named source metadata is unavailable; these deferred tools remain searchable).", scope_label(""))
        } else {
            "None currently enabled.".to_string()
        }
    } else {
        let mut source_descriptions = source_descriptions
            .into_iter()
            .map(|(name, description)| match description {
                Some(description) => format!("- {name}{}: {description}", scope_label(&name)),
                None => format!("- {name}{}", scope_label(&name)),
            })
            .collect::<Vec<_>>();
        if has_unnamed_tools {
            source_descriptions.push(format!("- Deferred built-in or extension tools{}", scope_label("")));
        }
        // Charge separators as well as prose so a large connector catalog
        // cannot grow this model-visible contribution beyond the shared cap.
        for description in &mut source_descriptions {
            description.push('\n');
        }
        crate::tools::spec_plan::apply_fair_description_budget(&mut source_descriptions);
        source_descriptions.concat().trim_end().to_string()
    }
}

pub(crate) fn canonical_source(info: &ToolSearchInfo) -> &str {
    match info.entry.output.as_ref() {
        LoadableToolSpec::Namespace(namespace) => &namespace.name,
        LoadableToolSpec::Function(tool) => &tool.name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn source_catalog_advertises_bounded_canonical_and_shared_scopes() {
        let infos = (0..12).map(|index| {
            let spec = ToolSpec::Namespace(codex_tools::ResponsesApiNamespace {
                name: if index < 2 { "shared".into() } else { format!("canonical_{index}") },
                description: String::new(),
                tools: vec![codex_tools::ResponsesApiNamespaceTool::Function(codex_tools::ResponsesApiTool {
                    name: format!("tool_{index}"), description: String::new(), strict: false, defer_loading: None,
                    parameters: JsonSchema::object(BTreeMap::new(), None, None), output_schema: None,
                })],
            });
            ToolSearchInfo::from_tool_spec(&spec, Some(ToolSearchSourceInfo {
                name: if index == 0 { "First app".into() } else { "Other app".into() }, description: None,
            })).unwrap()
        }).collect::<Vec<_>>();
        let sources = infos.iter().filter_map(|info| info.source_info.clone()).collect::<Vec<_>>();
        let catalog = render_tool_search_sources(&sources, false, &infos);
        assert!(catalog.contains("First app [source:shared (shared)]"));
        assert!(catalog.contains("source:canonical_"));
        assert!(catalog.contains("3 more scopes"));
        assert!(!catalog.contains("source:First app"));
        assert!(catalog.len() < 2048);
    }

    #[test]
    fn create_tool_search_tool_deduplicates_and_renders_enabled_sources() {
        assert_eq!(
            render_tool_search_sources(
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
                &[],
            ),
            "- Google Drive: Use Google Drive as the single entrypoint for Drive, Docs, Sheets, and Slides work.\n- docs"
        );
    }

    #[test]
    fn source_catalog_budget_preserves_small_sources_after_large_descriptions() {
        let sources = [
            ToolSearchSourceInfo {
                name: "large".to_string(),
                description: Some("界".repeat(40_000)),
            },
            ToolSearchSourceInfo {
                name: "small".to_string(),
                description: Some("Find small records.".to_string()),
            },
        ];
        let catalog = render_tool_search_sources(&sources, false, &[]);
        assert!(catalog.len() <= 40_000);
        assert!(catalog.contains("- small: Find small records."));
        assert!(catalog.contains("context truncated"));
    }

    #[test]
    fn create_tool_search_tool_describes_unnamed_deferred_tools() {
        let description = render_tool_search_sources(&[], true, &[]);

        assert!(description.contains("- Deferred built-in or extension tools"));
        assert!(description.contains("named source metadata is unavailable"));
        assert!(description.contains("these deferred tools remain searchable"));
        assert!(!description.contains("None currently enabled."));
    }

    #[test]
    fn tool_search_limit_schema_matches_runtime_range() {
        let ToolSpec::ToolSearch { parameters, .. } = create_tool_search_tool(8) else {
            panic!("expected tool search specification");
        };
        let schema = serde_json::to_value(parameters).expect("serialize tool_search schema");
        let validator = jsonschema::validator_for(&schema).expect("compile tool_search schema");

        assert!(validator.is_valid(&serde_json::json!({ "query": "q", "limit": 1 })));
        assert!(validator.is_valid(&serde_json::json!({ "query": "q", "limit": 64 })));
        assert!(!validator.is_valid(&serde_json::json!({ "query": "q", "limit": 0 })));
        assert!(!validator.is_valid(&serde_json::json!({ "query": "q", "limit": 65 })));
        assert!(!validator.is_valid(&serde_json::json!({ "query": "q", "limit": 1.5 })));
    }

    #[test]
    fn create_tool_search_tool_describes_named_and_unnamed_tools() {
        let description = render_tool_search_sources(
            &[ToolSearchSourceInfo {
                name: "Google Drive".to_string(),
                description: Some("Search Drive files.".to_string()),
            }],
            /*has_unnamed_tools*/ true,
            &[],
        );

        assert!(description.contains("- Google Drive: Search Drive files."));
        assert!(description.contains("- Deferred built-in or extension tools"));
    }
}
