use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::mcp_resource_spec::create_list_mcp_resources_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::protocol::McpInvocation;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::Value;

use super::ListMcpResourceTemplatesHandler;
use super::ListResourcesArgs;
use super::ListResourcesPayload;
use super::canonical_all_list_resources;
use super::canonical_single_list_resources;
use super::ensure_cursor_has_server;
use super::ensure_model_can_access_mcp_server;
use super::execute_resource_call;
use super::list_mcp_server_page;
use super::model_can_access_mcp_server;
use super::parse_args_with_default;
use super::parse_arguments;
use super::serialize_function_output;
use super::take_server_result;

pub struct ListMcpResourcesHandler;

#[derive(Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ResourceListKind {
    #[default]
    Resources,
    Templates,
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
        let arguments = match &invocation.payload {
            ToolPayload::Function { arguments } => parse_arguments(arguments)?,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "list_mcp_resources handler received unsupported payload".to_string(),
                ));
            }
        };
        // Keep the original arguments for lifecycle evidence; only the selector
        // decoder omits the canonical front door's operation discriminator.
        let mut selectors = arguments.clone();
        let kind: ResourceListKind = selectors
            .as_mut()
            .and_then(Value::as_object_mut)
            .and_then(|object| object.remove("kind"))
            .map(serde_json::from_value)
            .transpose()
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to parse function arguments: {err}"
                ))
            })?
            .unwrap_or_default();
        let args: ListResourcesArgs = parse_args_with_default(selectors)?;
        let args = args.normalize()?;
        ensure_cursor_has_server(args.server.as_deref(), args.cursor.as_deref())?;
        if kind == ResourceListKind::Templates {
            return ListMcpResourceTemplatesHandler
                .handle_parsed_call(invocation, args, arguments)
                .await;
        }

        let ToolInvocation {
            session,
            step_context,
            cancellation_token,
            call_id,
            ..
        } = invocation;
        let turn = std::sync::Arc::clone(&step_context.turn);
        let manager = step_context.mcp.manager();

        let ListResourcesArgs { server, cursor } = args;

        let invocation = McpInvocation {
            server: server.clone().unwrap_or_else(|| "codex".to_string()),
            tool: "list_mcp_resources".to_string(),
            arguments: arguments.clone(),
        };

        let truncation_policy = turn.model_info.truncation_policy.into();
        execute_resource_call(
            &session,
            &turn,
            &call_id,
            invocation,
            cancellation_token,
            async {
                let (payload, canonical) = if let Some(server_name) = server.clone() {
                    ensure_model_can_access_mcp_server(turn.as_ref(), &server_name)?;
                    let result = list_mcp_server_page(
                        cursor.clone(),
                        || async {
                            let collection = manager
                                .list_all_resources(|candidate| candidate == server_name)
                                .await;
                            take_server_result(
                                collection,
                                &server_name,
                                "resources/list",
                            )
                        },
                        |params| async {
                            manager
                                .list_resources(&server_name, params)
                                .await
                                .map_err(|err| {
                                    FunctionCallError::RespondToModel(format!(
                                        "resources/list failed: {err:#}"
                                    ))
                                })
                        },
                    )
                    .await?;
                    let canonical = canonical_single_list_resources(&server_name, &result)?;
                    let payload = ListResourcesPayload::from_single_server(
                        server_name,
                        result,
                        truncation_policy,
                    )?;
                    (payload, canonical)
                } else {
                    let collection = manager
                        .list_all_resources(|server_name| {
                            model_can_access_mcp_server(turn.as_ref(), server_name)
                        })
                        .await;
                    let canonical = canonical_all_list_resources(&collection)?;
                    let payload =
                        ListResourcesPayload::from_all_servers(collection, truncation_policy)?;
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

