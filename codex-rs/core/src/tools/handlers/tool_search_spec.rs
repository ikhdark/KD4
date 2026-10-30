use codex_tools::JsonSchema;
use codex_tools::LoadableToolSpec;
use codex_tools::TOOL_SEARCH_TOOL_NAME;
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
                "Short capability terms or exact tool names. +term requires a metadata match; source:<canonical namespace> (also +source:) restricts membership to that namespace (scope tokens appear in the source catalog; shared namespaces may contain several connectors). At most one scope; omit it for broad discovery. Name-only batches return available exact tools and report unknown snake_case names; wholly unknown name lookups fail. Mixed capability queries retain identifiers as task terms, not fuzzy callable aliases. Must not exceed 4,096 UTF-8 bytes."
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
        "# Tool discovery\n\nSearches deferred tool metadata with BM25. Direct results expose contracts on the next model call; inside exec, matches can be resolved and invoked in the same cell when their arguments are already known. An exact deferred-tool name may be called directly; the router resolves and activates that registered capability atomically.\n\nRepeated discovery is allowed. Reuse a previous enumeration only when it was complete and the catalog version, query (including limit), scope, and permissions match. Failed or incomplete discovery remains retryable; refine the query or increase the limit for omitted matches, and resolve exact names for omitted contracts.\n\nAvailable sources are listed in the latest <tool_search_sources> context message, not in this stable contract. Use `{TOOL_SEARCH_TOOL_NAME}` to discover or disambiguate deferred capabilities. For MCP tool discovery, always use `{TOOL_SEARCH_TOOL_NAME}` instead of `list_mcp_resources` or `list_mcp_resource_templates`."
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

pub(crate) fn render_tool_search_sources<'a>(
    searchable_sources: impl IntoIterator<Item = &'a ToolSearchSourceInfo>,
    has_unnamed_tools: bool,
    search_infos: impl IntoIterator<Item = &'a ToolSearchInfo>,
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
        let Some(tokens) = scopes.get(name) else {
            return String::new();
        };
        let mut bytes = 0;
        let labels = tokens
            .iter()
            .take(8)
            .filter_map(|scope| {
                let label = format!(
                    "source:{scope}{}",
                    if owners[scope].len() > 1 {
                        " (shared)"
                    } else {
                        ""
                    }
                );
                bytes += label.len();
                (bytes <= 512).then_some(label)
            })
            .collect::<Vec<_>>();
        let omitted = tokens.len() - labels.len();
        format!(
            " [{}{}]",
            labels.join(", "),
            if omitted == 0 {
                String::new()
            } else {
                format!("; {omitted} more scopes")
            }
        )
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
            format!(
                "- Deferred built-in or extension tools{} (named source metadata is unavailable; these deferred tools remain searchable).",
                scope_label("")
            )
        } else {
            "None currently enabled.".to_string()
        }
    } else {
        let mut source_descriptions = source_descriptions
            .into_iter()
            .map(|(name, description)| match description {
                Some(description) => format!(
                    "- {name}{}: {}",
                    scope_label(&name),
                    catalog_summary(&description)
                ),
                None => format!("- {name}{}", scope_label(&name)),
            })
            .collect::<Vec<_>>();
        if has_unnamed_tools {
            source_descriptions.push(format!(
                "- Deferred built-in or extension tools{}",
                scope_label("")
            ));
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

/// The catalog only advertises that a source exists. Its complete description
/// stays on the loadable namespace instead of recurring in every context window.
fn catalog_summary(description: &str) -> std::borrow::Cow<'_, str> {
    const MAX_BYTES: usize = 256;
    let description = description.trim();
    let first = description
        .split("\n\n")
        .next()
        .unwrap_or_default()
        .trim_end();
    let first = &first[..first.floor_char_boundary(MAX_BYTES.min(first.len()))];
    if first.len() == description.len() {
        return description.into();
    }
    format!("{first} [abridged; the source's full description loads with its tools]").into()
}

pub(crate) fn canonical_source(info: &ToolSearchInfo) -> &str {
    match info.entry.output.as_ref() {
        LoadableToolSpec::Namespace(namespace) => &namespace.name,
        LoadableToolSpec::Function(tool) => &tool.name,
    }
}

