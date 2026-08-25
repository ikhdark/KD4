use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolSpec;
use codex_otel::MetricsClient;
use serde_json::json;

use crate::ADD_AD_HOC_NOTE_TOOL_NAME;
use crate::backend::AddAdHocMemoryNoteRequest;
use crate::backend::AddAdHocMemoryNoteResponse;
use crate::local::LocalMemoriesBackend;
use crate::metrics::record_tool_call;

use super::backend_error_to_function_call;
use super::memory_function_tool;
use super::memory_tool_name;
use super::parse_args;

#[derive(Clone)]
pub(super) struct AddAdHocNoteTool {
    pub(super) backend: LocalMemoriesBackend,
    pub(super) metrics_client: Option<MetricsClient>,
}

impl ToolExecutor<ToolCall> for AddAdHocNoteTool {
    fn tool_name(&self) -> ToolName {
        memory_tool_name(ADD_AD_HOC_NOTE_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        memory_function_tool::<AddAdHocMemoryNoteRequest, AddAdHocMemoryNoteResponse>(
            ADD_AD_HOC_NOTE_TOOL_NAME,
            "Create one append-only ad-hoc memory note after the user explicitly asks Codex to remember, forget, or update something.",
        )
    }

    fn handle(&self, call: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(call))
    }
}

impl AddAdHocNoteTool {
    async fn handle_call(
        &self,
        call: ToolCall,
    ) -> Result<Box<dyn codex_extension_api::ToolOutput>, codex_extension_api::FunctionCallError>
    {
        let request: AddAdHocMemoryNoteRequest = parse_args(&call)?;
        let response = self.backend.add_ad_hoc_note(request).await;
        record_tool_call(
            self.metrics_client.as_ref(),
            ADD_AD_HOC_NOTE_TOOL_NAME,
            "ad_hoc_notes",
            response.is_ok(),
            "not_applicable",
        );
        let response = response.map_err(backend_error_to_function_call)?;
        Ok(Box::new(JsonToolOutput::new(json!(response))))
    }
}
