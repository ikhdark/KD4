use crate::context::PromptProvenanceSidecar;
use crate::stable_context::StableContextManifest;
use crate::tool_history::ToolHistorySubstitution;
pub use codex_api::ResponseEvent;
use codex_protocol::error::Result;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_tools::ToolSpec;
use futures::Stream;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Stable identities of the three model-visible prompt domains that dominate
/// request-prefix comparison. They are computed by their owning assembly
/// stages and carried alongside the prompt; they never enter the wire payload.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PromptDigests {
    pub(crate) instructions: Option<[u8; 32]>,
    pub(crate) tools: Option<[u8; 32]>,
    pub(crate) history: Option<[u8; 32]>,
}

/// Immutable, model-visible tool schema data shared by prompt construction,
/// request serialization, accounting, and retries.
#[derive(Debug)]
pub(crate) struct ToolSchemaArtifact {
    specs: Arc<[ToolSpec]>,
    serialized: Arc<[u8]>,
    digest: [u8; 32],
    has_request_user_input: bool,
    has_wait: bool,
}

impl ToolSchemaArtifact {
    pub(crate) fn new(specs: Vec<ToolSpec>) -> Self {
        let specs: Arc<[ToolSpec]> = specs.into();
        let serialized: Arc<[u8]> = serde_json::to_vec(specs.as_ref())
            .unwrap_or_default()
            .into();
        Self {
            has_request_user_input: specs.iter().any(|spec| spec.name() == "request_user_input"),
            has_wait: specs.iter().any(|spec| spec.name() == "wait"),
            digest: Sha256::digest(serialized.as_ref()).into(),
            specs,
            serialized,
        }
    }

    pub(crate) fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    pub(crate) fn serialized(&self) -> &[u8] {
        &self.serialized
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        self.digest
    }

    pub(crate) fn has_request_user_input(&self) -> bool {
        self.has_request_user_input
    }

    pub(crate) fn has_wait(&self) -> bool {
        self.has_wait
    }
}

impl Default for ToolSchemaArtifact {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// Request-dynamic history accounting that is intentionally resolved only by
/// post-dispatch diagnostics. Clones share the same result so unchanged
/// transport retries do not repeat serialization or token scanning.
#[derive(Debug, Clone)]
pub(crate) struct DeferredDynamicHistoryMeasurement {
    stable_input_bytes: u64,
    stable_input_tokens: i64,
    measured_manifest: Arc<OnceLock<StableContextManifest>>,
    #[cfg(test)]
    measurement_count: Arc<AtomicUsize>,
}

impl DeferredDynamicHistoryMeasurement {
    pub(crate) fn new(stable_input_bytes: u64, stable_input_tokens: i64) -> Self {
        Self {
            stable_input_bytes,
            stable_input_tokens,
            measured_manifest: Arc::new(OnceLock::new()),
            #[cfg(test)]
            measurement_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn measure(
        &self,
        input: &[ResponseItem],
        manifest: &StableContextManifest,
    ) -> StableContextManifest {
        self.measured_manifest
            .get_or_init(|| {
                #[cfg(test)]
                self.measurement_count.fetch_add(1, Ordering::Relaxed);
                let input_bytes = serde_json::to_vec(input).unwrap_or_default();
                let dynamic_bytes = u64::try_from(input_bytes.len())
                    .unwrap_or(u64::MAX)
                    .saturating_sub(self.stable_input_bytes);
                let dynamic_tokens =
                    i64::try_from(codex_utils_output_truncation::approx_token_count(
                        std::str::from_utf8(&input_bytes).unwrap_or_default(),
                    ))
                    .unwrap_or(i64::MAX)
                    .saturating_sub(self.stable_input_tokens)
                    .max(0);
                manifest.add_dynamic_history(&input_bytes, dynamic_bytes, dynamic_tokens)
            })
            .clone()
    }

    #[cfg(test)]
    fn measurement_count(&self) -> usize {
        self.measurement_count.load(Ordering::Relaxed)
    }
}

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

    /// Deferred request-dynamic accounting resolved by post-dispatch
    /// diagnostics. This field is never serialized into a provider request.
    pub(crate) deferred_dynamic_history: Option<DeferredDynamicHistoryMeasurement>,

    /// Measurement-only response-item provenance. It is never serialized.
    pub(crate) prompt_provenance: PromptProvenanceSidecar,

    /// Precomputed model-visible identities reused by request serialization,
    /// prefix comparisons, and telemetry baselines.
    pub(crate) digests: PromptDigests,

    /// Tools available to the model, including additional tools sourced from
    /// external MCP servers.
    pub(crate) tools: Arc<ToolSchemaArtifact>,

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
            deferred_dynamic_history: None,
            prompt_provenance: PromptProvenanceSidecar::default(),
            digests: PromptDigests::default(),
            tools: Arc::new(ToolSchemaArtifact::default()),
            parallel_tool_calls: false,
            base_instructions: BaseInstructions::default(),
            output_schema: None,
            output_schema_strict: true,
        }
    }
}

impl Prompt {
    pub(crate) fn measured_stable_context_manifest(&self) -> StableContextManifest {
        self.deferred_dynamic_history.as_ref().map_or_else(
            || self.stable_context_manifest.clone(),
            |measurement| measurement.measure(&self.input, &self.stable_context_manifest),
        )
    }

    #[cfg(test)]
    pub(crate) fn dynamic_history_measurement_count(&self) -> usize {
        self.deferred_dynamic_history
            .as_ref()
            .map_or(0, DeferredDynamicHistoryMeasurement::measurement_count)
    }

    pub(crate) fn get_formatted_input_for_request(
        &self,
        use_responses_lite: bool,
    ) -> Arc<[ResponseItem]> {
        let mut input = Arc::clone(&self.input);
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
