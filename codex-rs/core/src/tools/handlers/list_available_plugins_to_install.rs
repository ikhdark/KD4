use codex_tools::JsonToolOutput;
use codex_tools::LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME;
use codex_tools::ListAvailablePluginsToInstallResult;
use codex_tools::RequestPluginInstallEntry;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::list_available_plugins_to_install_spec::create_list_available_plugins_to_install_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

const MAX_LIST_AVAILABLE_PLUGINS_TO_INSTALL_DESCRIPTION_CHARS: usize = 240;

pub struct ListAvailablePluginsToInstallHandler {
    tools: Vec<RequestPluginInstallEntry>,
    on_demand: bool,
}

impl ListAvailablePluginsToInstallHandler {
    pub(crate) fn new(mut tools: Vec<RequestPluginInstallEntry>) -> Self {
        tools.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.id.cmp(&right.id))
        });
        Self {
            tools,
            on_demand: false,
        }
    }

    pub(crate) fn on_demand() -> Self {
        Self {
            tools: Vec::new(),
            on_demand: true,
        }
    }

    fn result(&self) -> ListAvailablePluginsToInstallResult {
        ListAvailablePluginsToInstallResult {
            tools: self.tools.clone(),
        }
    }

    fn output(&self) -> Result<JsonToolOutput, FunctionCallError> {
        let value = serde_json::to_value(self.result()).map_err(|err| {
            FunctionCallError::Fatal(format!(
                "failed to serialize {LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME} response: {err}"
            ))
        })?;

        let mut preview = value.clone();
        if let Some(tools) = preview["tools"].as_array_mut() {
            for tool in tools {
                if let Some(description) = tool["description"].as_str() {
                    let prefix = truncate_to_char_boundary(
                        description,
                        MAX_LIST_AVAILABLE_PLUGINS_TO_INSTALL_DESCRIPTION_CHARS,
                    );
                    if prefix.len() != description.len() {
                        tool["description"] = format!(
                            "{prefix} [... recover full description with read_tool_output]"
                        )
                        .into();
                    }
                }
            }
        }
        Ok(JsonToolOutput::new(value).with_recoverable_model_value(preview))
    }
}

impl ToolExecutor<ToolInvocation> for ListAvailablePluginsToInstallHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_list_available_plugins_to_install_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl ListAvailablePluginsToInstallHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation { payload, .. } = invocation;
        match payload {
            ToolPayload::Function { .. } => {}
            _ => {
                return Err(FunctionCallError::Fatal(format!(
                    "{LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME} handler received unsupported payload"
                )));
            }
        }

        if self.on_demand {
            let tools = super::request_plugin_install::discover_plugin_install_candidates(
                invocation.session.as_ref(),
                invocation.step_context.as_ref(),
            )
            .await?;
            return Ok(boxed_tool_output(
                Self::new(codex_tools::collect_request_plugin_install_entries(&tools)).output()?,
            ));
        }
        Ok(boxed_tool_output(self.output()?))
    }
}

impl CoreToolRuntime for ListAvailablePluginsToInstallHandler {}

fn truncate_to_char_boundary(value: &str, max_chars: usize) -> &str {
    match value.char_indices().nth(max_chars) {
        Some((index, _)) => &value[..index],
        None => value,
    }
}

