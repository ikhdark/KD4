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
                        tool["description"] = format!("{prefix} [... recover full description with read_tool_output]").into();
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_tools::DiscoverableToolType;
    use pretty_assertions::assert_eq;

    #[test]
    fn list_tool_does_not_support_parallel_calls() {
        assert!(
            !ListAvailablePluginsToInstallHandler::new(Vec::new()).supports_parallel_tool_calls()
        );
    }

    #[test]
    fn code_mode_result_is_a_structured_tools_object() {
        let output = ListAvailablePluginsToInstallHandler::new(Vec::new())
            .output()
            .expect("serialize result");
        let result = codex_tools::ToolOutput::code_mode_result(
            &output,
            &ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        );

        assert_eq!(result, serde_json::json!({ "tools": [] }));
    }

    #[test]
    fn result_preserves_candidate_descriptions() {
        let handler = ListAvailablePluginsToInstallHandler::new(vec![
            RequestPluginInstallEntry {
                id: "sample@openai-curated".to_string(),
                name: "Sample Plugin".to_string(),
                description: Some(
                    "x".repeat(MAX_LIST_AVAILABLE_PLUGINS_TO_INSTALL_DESCRIPTION_CHARS + 1),
                ),
                tool_type: DiscoverableToolType::Plugin,
                has_skills: true,
                mcp_server_names: vec!["sample-mcp".to_string()],
                app_connector_ids: vec!["connector-sample".to_string()],
            },
            RequestPluginInstallEntry {
                id: "calendar@openai-curated".to_string(),
                name: "Calendar".to_string(),
                description: Some("calendar".to_string()),
                tool_type: DiscoverableToolType::Plugin,
                has_skills: false,
                mcp_server_names: Vec::new(),
                app_connector_ids: Vec::new(),
            },
        ]);

        assert_eq!(
            handler.result(),
            ListAvailablePluginsToInstallResult {
                tools: vec![
                    RequestPluginInstallEntry {
                        id: "calendar@openai-curated".to_string(),
                        name: "Calendar".to_string(),
                        description: Some("calendar".to_string()),
                        tool_type: DiscoverableToolType::Plugin,
                        has_skills: false,
                        mcp_server_names: Vec::new(),
                        app_connector_ids: Vec::new(),
                    },
                    RequestPluginInstallEntry {
                        id: "sample@openai-curated".to_string(),
                        name: "Sample Plugin".to_string(),
                        description: Some(
                            "x".repeat(MAX_LIST_AVAILABLE_PLUGINS_TO_INSTALL_DESCRIPTION_CHARS + 1)
                        ),
                        tool_type: DiscoverableToolType::Plugin,
                        has_skills: true,
                        mcp_server_names: vec!["sample-mcp".to_string()],
                        app_connector_ids: vec!["connector-sample".to_string()],
                    },
                ],
            }
        );
    }

    #[test]
    fn preview_suffixes_remain_distinguishable_in_canonical_recovery() {
        use codex_tools::ToolOutput;
        let prefix = "界".repeat(MAX_LIST_AVAILABLE_PLUGINS_TO_INSTALL_DESCRIPTION_CHARS);
        let candidates = ["calendar export", "document export"].into_iter().map(|suffix| RequestPluginInstallEntry {
            id: suffix.into(), name: suffix.into(), description: Some(format!("{prefix}{suffix}")),
            tool_type: DiscoverableToolType::Plugin, has_skills: false,
            mcp_server_names: Vec::new(), app_connector_ids: Vec::new(),
        }).collect::<Vec<_>>();
        let output = ListAvailablePluginsToInstallHandler::new(candidates.clone()).output().unwrap();
        assert!(output.requires_canonical_artifact());
        let payload = ToolPayload::Function { arguments: "{}".into() };
        let raw = output.code_mode_result(&payload);
        for (index, candidate) in candidates.iter().enumerate() {
            assert_eq!(raw["tools"][index]["description"], candidate.description.as_ref().unwrap().as_str());
        }
        let preview: serde_json::Value = serde_json::from_str(&output.projection_metadata().unwrap().spillable_text[0]).unwrap();
        assert_eq!(preview["tools"][0]["description"], preview["tools"][1]["description"]);
        assert_eq!(output.canonical_result(&payload), JsonToolOutput::new(raw).canonical_result(&payload));
        assert!(!ListAvailablePluginsToInstallHandler::new(Vec::new()).output().unwrap().requires_canonical_artifact());
    }
}
