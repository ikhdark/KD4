use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::mcp_resource_spec::create_list_mcp_resource_templates_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::protocol::McpInvocation;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use rmcp::model::ListResourceTemplatesResult;
use rmcp::model::ResourceTemplate;

use super::ListResourceTemplatesArgs;
use super::ListResourceTemplatesPayload;
use super::McpServerCollection;
use super::canonical_all_list_resource_templates;
use super::canonical_single_list_resource_templates;
use super::ensure_cursor_has_server;
use super::ensure_model_can_access_mcp_server;
use super::execute_resource_call;
use super::list_mcp_server_page;
use super::model_can_access_mcp_server;
use super::parse_args_with_default;
use super::parse_arguments;
use super::serialize_function_output;
use super::take_server_result;

pub struct ListMcpResourceTemplatesHandler;

fn exhaustive_resource_template_pages(
    collection: McpServerCollection<Vec<ResourceTemplate>>,
) -> McpServerCollection<ListResourceTemplatesResult> {
    McpServerCollection {
        results: collection
            .results
            .into_iter()
            .map(|(server, resource_templates)| {
                (
                    server,
                    ListResourceTemplatesResult {
                        meta: None,
                        next_cursor: None,
                        resource_templates,
                    },
                )
            })
            .collect(),
        errors: collection.errors,
    }
}

impl ToolExecutor<ToolInvocation> for ListMcpResourceTemplatesHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("list_mcp_resource_templates")
    }

    fn spec(&self) -> ToolSpec {
        create_list_mcp_resource_templates_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl ListMcpResourceTemplatesHandler {
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
                    "list_mcp_resource_templates handler received unsupported payload".to_string(),
                ));
            }
        };

        let arguments = parse_arguments(arguments.as_str())?;
        let args: ListResourceTemplatesArgs = parse_args_with_default(arguments.clone())?;
        let args = args.normalize()?;
        let ListResourceTemplatesArgs { server, cursor } = args;
        ensure_cursor_has_server(server.as_deref(), cursor.as_deref())?;

        let invocation = McpInvocation {
            server: server.clone().unwrap_or_else(|| "codex".to_string()),
            tool: "list_mcp_resource_templates".to_string(),
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
                    let result = list_mcp_server_page(
                        cursor.clone(),
                        || async {
                            let collection = manager
                                .list_all_resource_templates(|candidate| candidate == server_name)
                                .await;
                            take_server_result(
                                exhaustive_resource_template_pages(collection),
                                &server_name,
                                "resources/templates/list",
                            )
                        },
                        |params| async {
                            manager
                                .list_resource_templates(&server_name, params)
                                .await
                                .map_err(|err| {
                                    FunctionCallError::RespondToModel(format!(
                                        "resources/templates/list failed: {err:#}"
                                    ))
                                })
                        },
                    )
                    .await?;
                    let canonical =
                        canonical_single_list_resource_templates(&server_name, &result)?;
                    let payload = ListResourceTemplatesPayload::from_single_server(
                        server_name,
                        result,
                        truncation_policy,
                    )?;
                    (payload, canonical)
                } else {
                    let collection = manager
                        .list_all_resource_templates(|server_name| {
                            model_can_access_mcp_server(turn.as_ref(), server_name)
                        })
                        .await;
                    let pages = exhaustive_resource_template_pages(collection);
                    let canonical = canonical_all_list_resource_templates(&pages)?;
                    let payload =
                        ListResourceTemplatesPayload::from_all_servers(pages, truncation_policy)?;
                    (payload, canonical)
                };
                serialize_function_output(payload, canonical, truncation_policy)
            },
        )
        .await
    }
}

impl CoreToolRuntime for ListMcpResourceTemplatesHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}
