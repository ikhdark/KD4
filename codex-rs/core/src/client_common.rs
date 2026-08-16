use crate::context::PromptProvenanceSidecar;
use crate::stable_context::StableContextManifest;
use crate::tool_history::ToolHistorySubstitution;
pub use codex_api::ResponseEvent;
use codex_protocol::error::Result;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::HistoryProjectionManifest;
use codex_tools::ToolSpec;
use futures::Stream;
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// API request payload for a single model turn
#[derive(Debug, Clone)]
pub struct Prompt {
    /// Conversation context input items.
    pub input: Arc<[ResponseItem]>,

    /// Complete normalized input retained for fail-open dispatch if a
    /// transport cannot establish a fresh non-inheriting provider baseline.
    pub(crate) stable_context_fallback_input: Arc<[ResponseItem]>,

    /// Same stable-context projection as `input`, but with completed-tool
    /// receipts left as their previously exposed bounded outputs.
    pub(crate) tool_history_fallback_input: Arc<[ResponseItem]>,

    /// Fully fail-open input: stable context is restored and completed-tool
    /// receipts remain unreplaced.
    pub(crate) stable_context_tool_history_fallback_input: Arc<[ResponseItem]>,

    /// Receipt substitutions applied to `input`. This metadata is never sent
    /// to a provider; it gates provider-prefix invalidation.
    pub(crate) tool_history_substitutions: Arc<[ToolHistorySubstitution]>,

    /// Receipt substitutions applied to `stable_context_fallback_input`.
    pub(crate) stable_context_fallback_tool_history_substitutions: Arc<[ToolHistorySubstitution]>,

    /// Internal-only context identity and accounting sidecar. This field is
    /// never serialized into a provider request.
    pub(crate) stable_context_manifest: StableContextManifest,

    /// Measurement-only response-item provenance. It is never serialized.
    pub(crate) prompt_provenance: PromptProvenanceSidecar,

    /// Hash-only projection provenance persisted with the sampling boundary.
    /// It is never serialized into a provider request.
    pub(crate) history_projection_manifest: Option<HistoryProjectionManifest>,

    /// Tools available to the model, including additional tools sourced from
    /// external MCP servers.
    pub(crate) tools: Vec<ToolSpec>,

    /// Whether parallel tool calls are permitted for this prompt.
    pub(crate) parallel_tool_calls: bool,

    pub base_instructions: BaseInstructions,

    /// Optional the output schema for the model's response.
    pub output_schema: Option<Value>,

    /// Whether the Responses API should strictly validate `output_schema`.
    pub output_schema_strict: bool,
}

impl Default for Prompt {
    fn default() -> Self {
        Self {
            input: Arc::from([]),
            stable_context_fallback_input: Arc::from([]),
            tool_history_fallback_input: Arc::from([]),
            stable_context_tool_history_fallback_input: Arc::from([]),
            tool_history_substitutions: Arc::from([]),
            stable_context_fallback_tool_history_substitutions: Arc::from([]),
            stable_context_manifest: StableContextManifest::default(),
            prompt_provenance: PromptProvenanceSidecar::default(),
            history_projection_manifest: None,
            tools: Vec::new(),
            parallel_tool_calls: false,
            base_instructions: BaseInstructions::default(),
            output_schema: None,
            output_schema_strict: true,
        }
    }
}

impl Prompt {
    pub(crate) fn get_formatted_input_for_request(
        &self,
        use_responses_lite: bool,
    ) -> Arc<[ResponseItem]> {
        let mut input = if crate::latency_switches::shared_prompt_input_enabled() {
            Arc::clone(&self.input)
        } else {
            Arc::from(self.input.to_vec())
        };
        if use_responses_lite && has_image_details(&input) {
            strip_image_details(Arc::make_mut(&mut input));
        }
        input
    }

    pub(crate) fn get_formatted_fallback_input_for_request(
        &self,
        use_responses_lite: bool,
    ) -> Arc<[ResponseItem]> {
        let mut input = Arc::clone(&self.stable_context_fallback_input);
        if use_responses_lite && has_image_details(&input) {
            strip_image_details(Arc::make_mut(&mut input));
        }
        input
    }

    pub(crate) fn get_formatted_tool_history_fallback_input_for_request(
        &self,
        use_responses_lite: bool,
        use_stable_context_fallback: bool,
    ) -> Arc<[ResponseItem]> {
        let mut input = if use_stable_context_fallback {
            Arc::clone(&self.stable_context_tool_history_fallback_input)
        } else {
            Arc::clone(&self.tool_history_fallback_input)
        };
        if use_responses_lite && has_image_details(&input) {
            strip_image_details(Arc::make_mut(&mut input));
        }
        input
    }

    pub(crate) fn tool_history_substitutions_for_request(
        &self,
        use_stable_context_fallback: bool,
    ) -> &[ToolHistorySubstitution] {
        if use_stable_context_fallback {
            &self.stable_context_fallback_tool_history_substitutions
        } else {
            &self.tool_history_substitutions
        }
    }
}

fn has_image_details(items: &[ResponseItem]) -> bool {
    items.iter().any(|item| match item {
        ResponseItem::Message { content, .. } => content.iter().any(|content_item| {
            matches!(
                content_item,
                ContentItem::InputImage {
                    detail: Some(_),
                    ..
                }
            )
        }),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.content_items().is_some_and(|content| {
                content.iter().any(|content_item| {
                    matches!(
                        content_item,
                        FunctionCallOutputContentItem::InputImage {
                            detail: Some(_),
                            ..
                        }
                    )
                })
            })
        }
        _ => false,
    })
}

fn strip_image_details(items: &mut [ResponseItem]) {
    for item in items {
        match item {
            ResponseItem::Message { content, .. } => {
                for content_item in content {
                    if let ContentItem::InputImage { detail, .. } = content_item {
                        *detail = None;
                    }
                }
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(content) = output.content_items_mut() {
                    for content_item in content {
                        if let FunctionCallOutputContentItem::InputImage { detail, .. } =
                            content_item
                        {
                            *detail = None;
                        }
                    }
                }
            }
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => {}
        }
    }
}

pub struct ResponseStream {
    pub(crate) rx_event: mpsc::Receiver<Result<ResponseEvent>>,
    pub(crate) attempt_identity: Option<ResponseAttemptIdentity>,
    /// Signals the mapper task that the consumer stopped polling before the
    /// provider stream reached its own terminal event.
    pub(crate) consumer_dropped: CancellationToken,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResponseAttemptIdentity {
    pub(crate) sampling_request_id: String,
    pub(crate) physical_attempt_id: String,
}

impl ResponseStream {
    pub(crate) fn attempt_identity(&self) -> Option<&ResponseAttemptIdentity> {
        self.attempt_identity.as_ref()
    }
}

impl Stream for ResponseStream {
    type Item = Result<ResponseEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx_event.poll_recv(cx)
    }
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        self.consumer_dropped.cancel();
    }
}

#[cfg(test)]
#[path = "client_common_tests.rs"]
mod tests;
