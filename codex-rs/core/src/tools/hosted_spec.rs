use codex_protocol::config_types::WebSearchConfig;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::openai_models::WebSearchToolType;
use codex_tools::ToolSpec;

const WEB_SEARCH_TEXT_AND_IMAGE_CONTENT_TYPES: [&str; 2] = ["text", "image"];

pub struct WebSearchToolOptions<'a> {
    pub web_search_mode: Option<WebSearchMode>,
    pub force_cached_access: bool,
    pub web_search_config: Option<&'a WebSearchConfig>,
    pub web_search_tool_type: WebSearchToolType,
}

pub fn create_web_search_tool(options: WebSearchToolOptions<'_>) -> Option<ToolSpec> {
    let (mut external_web_access, mut indexed_web_access) = match options.web_search_mode {
        Some(WebSearchMode::Cached) => (false, None),
        Some(WebSearchMode::Indexed) => (true, Some(true)),
        Some(WebSearchMode::Live) => (true, None),
        Some(WebSearchMode::Disabled) | None => return None,
    };
    if options.force_cached_access {
        // Bedrock live fetching requires a separate authorization policy. A
        // general search mode must not opt into crossing its AWS boundary.
        external_web_access = false;
        indexed_web_access = None;
    }

    let search_content_types = match options.web_search_tool_type {
        WebSearchToolType::Text => None,
        WebSearchToolType::TextAndImage => Some(
            WEB_SEARCH_TEXT_AND_IMAGE_CONTENT_TYPES
                .into_iter()
                .map(str::to_string)
                .collect(),
        ),
    };

    Some(ToolSpec::WebSearch {
        external_web_access: Some(external_web_access),
        indexed_web_access,
        filters: options
            .web_search_config
            .and_then(|config| config.filters.clone().map(Into::into)),
        user_location: options
            .web_search_config
            .and_then(|config| config.user_location.clone().map(Into::into)),
        search_context_size: options
            .web_search_config
            .and_then(|config| config.search_context_size),
        search_content_types,
    })
}

