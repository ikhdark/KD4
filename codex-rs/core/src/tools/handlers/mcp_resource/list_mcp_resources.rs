use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::mcp_resource_spec::create_list_mcp_resources_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::protocol::McpInvocation;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use rmcp::model::ListResourcesResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::Resource;

use super::ListResourcesArgs;
use super::ListResourcesPayload;
use super::McpServerCollection;
use super::canonical_all_list_resources;
use super::canonical_single_list_resources;
use super::ensure_model_can_access_mcp_server;
use super::execute_resource_call;
use super::model_can_access_mcp_server;
use super::parse_args_with_default;
use super::parse_arguments;
use super::serialize_function_output;

pub struct ListMcpResourcesHandler;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ListResourcesStrategy {
    Exhaustive,
    ExplicitPage,
}

fn list_resources_strategy(cursor: Option<&str>) -> ListResourcesStrategy {
    if cursor.is_some() {
        ListResourcesStrategy::ExplicitPage
    } else {
        ListResourcesStrategy::Exhaustive
    }
}

fn exhaustive_resource_pages(
    collection: McpServerCollection<Vec<Resource>>,
) -> McpServerCollection<ListResourcesResult> {
    McpServerCollection {
        results: collection
            .results
            .into_iter()
            .map(|(server, resources)| {
                (
                    server,
                    ListResourcesResult {
                        meta: None,
                        next_cursor: None,
                        resources,
                    },
                )
            })
            .collect(),
        errors: collection.errors,
    }
}

impl ToolExecutor<ToolInvocation> for ListMcpResourcesHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("list_mcp_resources")
    }

    fn spec(&self) -> ToolSpec {
        create_list_mcp_resources_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl ListMcpResourcesHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            cancellation_token,
            call_id,
            payload,
            ..
        } = invocation;
        let turn = std::sync::Arc::clone(&step_context.turn);
        let manager = step_context.mcp.manager();

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "list_mcp_resources handler received unsupported payload".to_string(),
                ));
            }
        };

        let arguments = parse_arguments(arguments.as_str())?;
        let args: ListResourcesArgs = parse_args_with_default(arguments.clone())?;
        let args = args.normalize()?;
        let ListResourcesArgs { server, cursor } = args;

        let invocation = McpInvocation {
            server: server.clone().unwrap_or_else(|| "codex".to_string()),
            tool: "list_mcp_resources".to_string(),
            arguments: arguments.clone(),
        };

        let truncation_policy = turn.model_info.truncation_policy.into();
        execute_resource_call(
            &session,
            turn.as_ref(),
            &call_id,
            invocation,
            cancellation_token,
            async {
                let (payload, canonical) = if let Some(server_name) = server.clone() {
                    ensure_model_can_access_mcp_server(turn.as_ref(), &server_name)?;
                    let result = match list_resources_strategy(cursor.as_deref()) {
                        ListResourcesStrategy::Exhaustive => {
                            let collection = manager
                                .list_all_resources(|candidate| candidate == server_name)
                                .await;
                            let mut pages = exhaustive_resource_pages(collection);
                            pages.results.remove(&server_name).ok_or_else(|| {
                                pages
                                    .errors
                                    .into_iter()
                                    .find(|error| error.server == server_name)
                                    .map_or_else(
                                        || {
                                            FunctionCallError::RespondToModel(format!(
                                                "resources/list failed: unknown MCP server '{server_name}'"
                                            ))
                                        },
                                        |error| {
                                            FunctionCallError::RespondToModel(error.message)
                                        },
                                    )
                            })?
                        }
                        ListResourcesStrategy::ExplicitPage => {
                            let params = cursor.clone().map(|value| {
                                PaginatedRequestParams::default().with_cursor(Some(value))
                            });
                            manager
                            .list_resources(&server_name, params)
                            .await
                            .map_err(|err| {
                                FunctionCallError::RespondToModel(format!(
                                    "resources/list failed: {err:#}"
                                ))
                            })?
                        }
                    };
                    let canonical = canonical_single_list_resources(&server_name, &result)?;
                    let payload = ListResourcesPayload::from_single_server(
                        server_name,
                        result,
                        truncation_policy,
                    )?;
                    (payload, canonical)
                } else {
                    if cursor.is_some() {
                        return Err(FunctionCallError::RespondToModel(
                            "cursor can only be used when a server is specified".to_string(),
                        ));
                    }

                    let collection = manager
                        .list_all_resources(|server_name| {
                            model_can_access_mcp_server(turn.as_ref(), server_name)
                        })
                        .await;
                    let pages = exhaustive_resource_pages(collection);
                    let canonical = canonical_all_list_resources(&pages)?;
                    let payload = ListResourcesPayload::from_all_servers(pages, truncation_policy)?;
                    (payload, canonical)
                };
                serialize_function_output(payload, canonical, truncation_policy)
            },
        )
        .await
    }
}

impl CoreToolRuntime for ListMcpResourcesHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_cursor_uses_exhaustive_owner_collection() {
        assert_eq!(
            list_resources_strategy(None),
            ListResourcesStrategy::Exhaustive
        );
        assert_eq!(
            list_resources_strategy(Some("opaque-cursor")),
            ListResourcesStrategy::ExplicitPage
        );
    }
}
