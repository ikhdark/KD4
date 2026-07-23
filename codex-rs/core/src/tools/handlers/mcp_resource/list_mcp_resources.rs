use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::mcp_resource_spec::create_list_mcp_resources_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::protocol::McpInvocation;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use rmcp::model::PaginatedRequestParams;

use super::ListResourcesArgs;
use super::ListResourcesPayload;
use super::ensure_model_can_access_mcp_server;
use super::execute_resource_call;
use super::model_can_access_mcp_server;
use super::parse_args_with_default;
use super::parse_arguments;
use super::serialize_function_output;

pub struct ListMcpResourcesHandler;

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
                let payload = if let Some(server_name) = server.clone() {
                    ensure_model_can_access_mcp_server(turn.as_ref(), &server_name)?;
                    let params = cursor
                        .clone()
                        .map(|value| PaginatedRequestParams::default().with_cursor(Some(value)));
                    let result =
                        manager
                            .list_resources(&server_name, params)
                            .await
                            .map_err(|err| {
                                FunctionCallError::RespondToModel(format!(
                                    "resources/list failed: {err:#}"
                                ))
                            })?;
                    ListResourcesPayload::from_single_server(
                        server_name,
                        result,
                        truncation_policy,
                    )?
                } else {
                    if cursor.is_some() {
                        return Err(FunctionCallError::RespondToModel(
                            "cursor can only be used when a server is specified".to_string(),
                        ));
                    }

                    let pages = manager
                        .list_resource_pages(|server_name| {
                            model_can_access_mcp_server(turn.as_ref(), server_name)
                        })
                        .await;
                    ListResourcesPayload::from_all_servers(pages, truncation_policy)?
                };
                serialize_function_output(payload, truncation_policy)
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
