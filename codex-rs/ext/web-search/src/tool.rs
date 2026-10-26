use codex_api::ReqwestTransport;
use codex_api::SearchClient;
use codex_api::SearchCommands;
use codex_api::SearchQuery;
use codex_api::SearchRequest;
use codex_api::SearchSettings;
use codex_core::web_search_action_detail;
use codex_extension_api::ExtensionTurnItem;
use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_extension_api::parse_tool_input_schema_without_compaction;
use codex_extension_items::ExtensionItem;
use codex_extension_items::web_search::WebSearchAction;
use codex_extension_items::web_search::WebSearchItem;
use codex_http_client::RouteAwareClientPool;
use codex_model_provider::SharedModelProvider;
use codex_protocol::models::WebSearchAction as CoreWebSearchAction;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WebSearchBeginEvent;
use codex_protocol::protocol::WebSearchEndEvent;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolExposure;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSearchSourceInfo;
use http::HeaderMap;
use std::sync::LazyLock;
use url::Url;

use crate::history::recent_input;
use crate::output::SearchOutput;
use crate::schema::commands_schema;

pub(crate) const WEB_NAMESPACE: &str = "web";
pub(crate) const RUN_TOOL_NAME: &str = "run";
const WEB_RUN_DESCRIPTION: &str = include_str!("../web_run_description.md");
const WEB_SOURCE_NAME: &str = "Web";
/// The deferred catalog lists this source before its tools load. A bare
/// `source:web` scope did not tell a research task that it can browse.
const WEB_SOURCE_DESCRIPTION: &str = "Search the public web and open pages, PDFs and links: current facts, documentation, papers and other projects' source that the workspace does not contain.";
const WEB_SEARCH_CANCELLED_MESSAGE: &str = "web search cancelled";

pub(crate) struct WebSearchTool {
    pub(crate) session_id: String,
    pub(crate) provider: SharedModelProvider,
    pub(crate) settings: SearchSettings,
    pub(crate) http_clients: RouteAwareClientPool,
}

impl ToolExecutor<ToolCall> for WebSearchTool {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(WEB_NAMESPACE, RUN_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        static SPEC: LazyLock<ToolSpec> = LazyLock::new(|| {
            // parse schema without compaction that removes field metadata/descriptions to match hosted tool definition
            let parameters = match parse_tool_input_schema_without_compaction(&commands_schema()) {
                Ok(parameters) => parameters,
                Err(err) => panic!("search command schema should parse: {err}"),
            };

            ToolSpec::Namespace(ResponsesApiNamespace {
                name: WEB_NAMESPACE.to_string(),
                description: WEB_SOURCE_DESCRIPTION.to_string(),
                tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                    name: RUN_TOOL_NAME.to_string(),
                    description: WEB_RUN_DESCRIPTION.to_string(),
                    strict: false,
                    parameters,
                    output_schema: None,
                    defer_loading: None,
                })],
            })
        });
        SPEC.clone()
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        ToolSearchInfo::from_tool_spec(
            &self.spec(),
            Some(ToolSearchSourceInfo {
                name: WEB_SOURCE_NAME.to_string(),
                description: Some(WEB_SOURCE_DESCRIPTION.to_string()),
            }),
        )
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handles_runtime_cancellation(&self) -> bool {
        true
    }

    fn handle(&self, call: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(call))
    }
}

impl WebSearchTool {
    async fn handle_call(&self, call: ToolCall) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let commands = parse_commands(&call)?;
        validate_commands(&commands)?;
        let command_action = command_action(&commands);
        let Some(client) = call
            .cancellation_token
            .run_until_cancelled(async {
                let (provider, auth) = self
                    .provider
                    .resolve_unscoped_client_setup()
                    .await
                    .map_err(|_| setup_error("provider configuration"))?;
                let endpoint = provider.url_for_path("alpha/search");
                self.http_clients
                    .client_for_url(&endpoint)
                    .await
                    .map_err(|_| setup_error("HTTP client configuration"))?;
                Ok::<_, FunctionCallError>(SearchClient::new(
                    ReqwestTransport::from_client_pool(self.http_clients.clone()),
                    provider,
                    auth,
                ))
            })
            .await
        else {
            return Err(cancelled_error());
        };
        let client = client?;
        let request = SearchRequest {
            id: self.session_id.clone(),
            model: call.model.clone(),
            reasoning: None,
            input: recent_input(call.conversation_history.items()),
            commands: Some(commands),
            settings: Some(self.settings.clone()),
            max_output_tokens: Some(
                u64::try_from(call.truncation_policy.token_budget()).unwrap_or(u64::MAX),
            ),
        };
        call.turn_item_emitter
            .emit_started(extension_turn_item(
                WebSearchItem {
                    id: call.call_id.clone(),
                    query: String::new(),
                    action: None,
                    status: Some("inProgress".to_string()),
                    error: None,
                },
                EventMsg::WebSearchBegin(WebSearchBeginEvent {
                    call_id: call.call_id.clone(),
                }),
            ))
            .await;
        let search_result = call
            .cancellation_token
            .run_until_cancelled(client.search(&request, HeaderMap::new()))
            .await;
        match search_result {
            None => {
                let terminal_item = completed_turn_item(
                    &call.call_id,
                    command_action,
                    "cancelled",
                    Some(WEB_SEARCH_CANCELLED_MESSAGE.to_string()),
                );
                call.turn_item_emitter.emit_completed(terminal_item).await;
                Err(cancelled_error())
            }
            Some(Err(err)) => {
                let error = search_error(err);
                let terminal_item = completed_turn_item(
                    &call.call_id,
                    command_action,
                    "failed",
                    Some(error.to_string()),
                );
                call.turn_item_emitter.emit_completed(terminal_item).await;
                Err(error)
            }
            Some(Ok(response)) => {
                let terminal_item =
                    completed_turn_item(&call.call_id, command_action, "completed", None);
                call.turn_item_emitter.emit_completed(terminal_item).await;
                Ok(Box::new(SearchOutput::new(response.output)))
            }
        }
    }
}

fn cancelled_error() -> FunctionCallError {
    FunctionCallError::RespondToModel(WEB_SEARCH_CANCELLED_MESSAGE.to_string())
}

fn setup_error(stage: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "Web search is unavailable because {stage} failed. User configuration or authentication must be repaired before retrying."
    ))
}

fn search_error(error: codex_api::ApiError) -> FunctionCallError {
    use codex_api::ApiError;
    let message = match error {
        ApiError::Api { status, .. }
        | ApiError::Transport(codex_api::TransportError::Http { status, .. })
            if status == http::StatusCode::UNAUTHORIZED
                || status == http::StatusCode::FORBIDDEN =>
        {
            "Web search access was denied. User authentication or permissions must be repaired before retrying."
        }
        ApiError::Api { status, .. }
        | ApiError::Transport(codex_api::TransportError::Http { status, .. })
            if status == http::StatusCode::BAD_REQUEST
                || status == http::StatusCode::UNPROCESSABLE_ENTITY =>
        {
            "Web search rejected the request. Correct the commands or reduce the request before retrying."
        }
        ApiError::Api { status, .. }
        | ApiError::Transport(codex_api::TransportError::Http { status, .. })
            if status == http::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() =>
        {
            "Web search is temporarily unavailable or rate limited. Retry later if the evidence is still needed."
        }
        ApiError::InvalidRequest { .. } | ApiError::ContextWindowExceeded => {
            "Web search rejected the request. Correct the commands or reduce the request before retrying."
        }
        ApiError::QuotaExceeded | ApiError::UsageNotIncluded => {
            "Web search is unavailable for this account's current quota or plan. User action is required before retrying."
        }
        ApiError::CyberPolicy { .. } => {
            "Web search was blocked by policy. Do not retry the same operation."
        }
        ApiError::RateLimit(_) | ApiError::ServerOverloaded { .. } | ApiError::Retryable { .. } => {
            "Web search is temporarily unavailable or rate limited. Retry later if the evidence is still needed."
        }
        _ => {
            "Web search failed to obtain a valid service response. No evidence was retrieved; retry only if the evidence is still needed."
        }
    };
    FunctionCallError::RespondToModel(message.to_string())
}

fn validate_commands(commands: &SearchCommands) -> Result<(), FunctionCallError> {
    let invalid = |message: &str| FunctionCallError::RespondToModel(message.to_string());
    let counts = [
        commands.search_query.as_ref().map_or(0, Vec::len),
        commands.image_query.as_ref().map_or(0, Vec::len),
        commands.open.as_ref().map_or(0, Vec::len),
        commands.click.as_ref().map_or(0, Vec::len),
        commands.find.as_ref().map_or(0, Vec::len),
        commands.screenshot.as_ref().map_or(0, Vec::len),
        commands.finance.as_ref().map_or(0, Vec::len),
        commands.weather.as_ref().map_or(0, Vec::len),
        commands.sports.as_ref().map_or(0, Vec::len),
        commands.time.as_ref().map_or(0, Vec::len),
    ];
    if counts.iter().all(|count| *count == 0) {
        return Err(invalid(
            "web.run requires at least one operation; response_length alone is not an operation",
        ));
    }
    if counts[0] > 4 {
        return Err(invalid("search_query accepts at most four queries"));
    }
    if counts[0] == 4
        && !matches!(
            commands.response_length,
            Some(codex_api::SearchResponseLength::Medium | codex_api::SearchResponseLength::Long)
        )
    {
        return Err(invalid(
            "four search queries require response_length medium or long",
        ));
    }
    // Empty search queries remain supported by the documented accidental-call
    // escape hatch. Opaque references are valid, but blank references are not.
    let references = commands
        .open
        .iter()
        .flatten()
        .map(|op| op.ref_id.as_str())
        .chain(commands.click.iter().flatten().map(|op| op.ref_id.as_str()))
        .chain(commands.find.iter().flatten().map(|op| op.ref_id.as_str()))
        .chain(
            commands
                .screenshot
                .iter()
                .flatten()
                .map(|op| op.ref_id.as_str()),
        );
    if references
        .into_iter()
        .any(|reference| reference.trim().is_empty())
    {
        return Err(invalid(
            "page operations require a nonblank ref_id (a returned reference or URL)",
        ));
    }
    Ok(())
}

fn completed_turn_item(
    call_id: &str,
    action: WebSearchAction,
    status: &str,
    error: Option<String>,
) -> ExtensionTurnItem {
    let legacy_action = match &action {
        WebSearchAction::Search { query, queries } => CoreWebSearchAction::Search {
            query: query.clone(),
            queries: queries.clone(),
        },
        WebSearchAction::OpenPage { url } => CoreWebSearchAction::OpenPage { url: url.clone() },
        WebSearchAction::FindInPage { url, pattern } => CoreWebSearchAction::FindInPage {
            url: url.clone(),
            pattern: pattern.clone(),
        },
        WebSearchAction::Other => CoreWebSearchAction::Other,
    };
    let query = web_search_action_detail(&legacy_action);
    extension_turn_item(
        WebSearchItem {
            id: call_id.to_string(),
            query: query.clone(),
            action: Some(action),
            status: Some(status.to_string()),
            error,
        },
        EventMsg::WebSearchEnd(WebSearchEndEvent {
            call_id: call_id.to_string(),
            query,
            action: legacy_action,
        }),
    )
}

fn parse_commands(call: &ToolCall) -> Result<SearchCommands, FunctionCallError> {
    let arguments = call.function_arguments()?;
    if arguments.trim().is_empty() {
        return Ok(SearchCommands::default());
    }

    serde_json::from_str(arguments)
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))
}

fn command_action(commands: &SearchCommands) -> WebSearchAction {
    commands
        .search_query
        .as_deref()
        .and_then(query_action)
        .or_else(|| commands.image_query.as_deref().and_then(query_action))
        .or_else(|| {
            commands
                .open
                .as_deref()
                .and_then(|operations| operations.first())
                .map(|operation| WebSearchAction::OpenPage {
                    url: literal_url(&operation.ref_id),
                })
        })
        .or_else(|| {
            commands
                .find
                .as_deref()
                .and_then(|operations| operations.first())
                .map(|operation| WebSearchAction::FindInPage {
                    url: literal_url(&operation.ref_id),
                    pattern: Some(operation.pattern.clone()),
                })
        })
        .unwrap_or(WebSearchAction::Other)
}

fn query_action(queries: &[SearchQuery]) -> Option<WebSearchAction> {
    match queries {
        [] => None,
        [query] => Some(WebSearchAction::Search {
            query: Some(query.q.clone()),
            queries: None,
        }),
        queries => Some(WebSearchAction::Search {
            query: None,
            queries: Some(queries.iter().map(|query| query.q.clone()).collect()),
        }),
    }
}

fn literal_url(ref_id: &str) -> Option<String> {
    Url::parse(ref_id).is_ok().then(|| ref_id.to_string())
}

fn extension_turn_item(item: WebSearchItem, legacy_event: EventMsg) -> ExtensionTurnItem {
    // The legacy end event has no failure representation. Publishing it for a
    // failed request would overwrite the honest canonical item during replay.
    let legacy_events = if matches!(item.status.as_deref(), Some("failed" | "cancelled")) {
        Vec::new()
    } else {
        vec![legacy_event]
    };
    ExtensionTurnItem {
        item: ExtensionItem::WebSearch(item),
        legacy_events,
    }
}
