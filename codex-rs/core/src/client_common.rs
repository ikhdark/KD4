mod repeated_bodies;

use crate::context::PromptProvenanceSidecar;
use crate::stable_context::StableContextManifest;
use crate::tool_history::ToolHistorySubstitution;
pub use codex_api::ResponseEvent;
use codex_api::response_event_channel::ResponseEventReceiver;
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
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
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
    pub(crate) fn new(mut specs: Vec<ToolSpec>) -> Self {
        // Order only the effective surface, after permission/exposure gates and
        // collision resolution. Never reorder schema arrays or promote tools.
        // Stable sorts preserve the relative order of any duplicate identities.
        specs.sort_by(|left, right| left.name().cmp(right.name()));
        for spec in &mut specs {
            if let ToolSpec::Namespace(namespace) = spec {
                namespace.tools.sort_by(|left, right| match (left, right) {
                    (
                        codex_tools::ResponsesApiNamespaceTool::Function(left),
                        codex_tools::ResponsesApiNamespaceTool::Function(right),
                    ) => left.name.cmp(&right.name),
                });
            }
        }
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
}

impl DeferredDynamicHistoryMeasurement {
    pub(crate) fn new(stable_input_bytes: u64, stable_input_tokens: i64) -> Self {
        Self {
            stable_input_bytes,
            stable_input_tokens,
            measured_manifest: Arc::new(OnceLock::new()),
        }
    }

    fn measure(
        &self,
        input: &[ResponseItem],
        manifest: &StableContextManifest,
    ) -> StableContextManifest {
        self.measured_manifest
            .get_or_init(|| {
                let input_bytes = serde_json::to_vec(input).unwrap_or_default();
                self.measure_bytes(&input_bytes, manifest)
            })
            .clone()
    }

    fn measure_serialized(
        &self,
        input_bytes: &[u8],
        manifest: &StableContextManifest,
    ) -> StableContextManifest {
        self.measured_manifest
            .get_or_init(|| self.measure_bytes(input_bytes, manifest))
            .clone()
    }

    fn measure_bytes(
        &self,
        input_bytes: &[u8],
        manifest: &StableContextManifest,
    ) -> StableContextManifest {
        let dynamic_bytes = u64::try_from(input_bytes.len())
            .unwrap_or(u64::MAX)
            .saturating_sub(self.stable_input_bytes);
        let dynamic_tokens = i64::try_from(codex_utils_output_truncation::approx_token_count(
            std::str::from_utf8(input_bytes).unwrap_or_default(),
        ))
        .unwrap_or(i64::MAX)
        .saturating_sub(self.stable_input_tokens)
        .max(0);
        manifest.add_dynamic_history(input_bytes, dynamic_bytes, dynamic_tokens)
    }

}

#[derive(Debug)]
pub(crate) struct FormattedPromptInput {
    source: Weak<[ResponseItem]>,
    use_lite: bool,
    // None records an unchanged projection without retaining its weak source.
    input: Option<Arc<[ResponseItem]>>,
}

/// API request payload for a single model turn
#[derive(Debug, Clone)]
pub struct Prompt {
    /// Conversation context input items.
    pub input: Arc<[ResponseItem]>,
    // Admission and dispatch share the selected immutable request projections.
    // Weak source identity fences later mutations without retaining old histories.
    pub(crate) formatted_inputs: Arc<Mutex<Vec<FormattedPromptInput>>>,

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

    /// Keep schemas stable while forbidding calls on a terminal continuation.
    pub(crate) tool_calls_disabled: bool,
    /// One explicitly selected soft-limit deferral; never bypasses the hard window.
    pub(crate) soft_limit_waiver: bool,

    pub base_instructions: BaseInstructions,

    /// Optional the output schema for the model's response.
    pub output_schema: Option<Value>,

    /// Whether the Responses API should strictly validate `output_schema`.
    pub output_schema_strict: bool,

    /// Longest provider silence tolerated while waiting for response progress.
    /// `None` keeps the provider's stream idle timeout. Never serialized.
    pub(crate) stream_progress_timeout: Option<Duration>,
}

impl Default for Prompt {
    fn default() -> Self {
        Self {
            input: Arc::from([]),
            formatted_inputs: Arc::new(Mutex::new(Vec::new())),
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
            tool_calls_disabled: false,
            soft_limit_waiver: false,
            base_instructions: BaseInstructions::default(),
            output_schema: None,
            output_schema_strict: true,
            stream_progress_timeout: None,
        }
    }
}

impl Prompt {
    /// One content-based recipe for admission and calibration. Images use the
    /// shared modality estimator, never the size of their wire base64 strings.
    pub(crate) fn budget_tokens_for_input(&self, input: &[ResponseItem]) -> i64 {
        let history = input
            .iter()
            .map(crate::context_manager::estimate_item_token_count)
            .fold(0i64, i64::saturating_add);
        history
            .saturating_add(
                i64::try_from(self.base_instructions.text.len().div_ceil(4)).unwrap_or(i64::MAX),
            )
            .saturating_add(
                i64::try_from(self.tools.serialized().len().div_ceil(4)).unwrap_or(i64::MAX),
            )
            .saturating_add(output_schema_tokens(
                self.output_schema.as_ref(),
                self.output_schema_strict,
            ))
    }

    pub(crate) fn calibration_basis(&self, input: Arc<[ResponseItem]>) -> PromptCalibrationBasis {
        self.calibration_basis_with_prefix(input, 0)
    }

    pub(crate) fn calibration_basis_with_prefix(
        &self,
        input: Arc<[ResponseItem]>,
        input_start: usize,
    ) -> PromptCalibrationBasis {
        let input_start = input_start.min(input.len());
        PromptCalibrationBasis {
            local_tokens: self.budget_tokens_for_input(&input[input_start..]),
            input,
            input_start,
            tools: self.tools.digest(),
            base_instructions: self.base_instructions.clone(),
            output_schema: self.output_schema.clone(),
            strict: self.output_schema_strict,
        }
    }

    pub(crate) fn measured_stable_context_manifest(&self) -> StableContextManifest {
        self.deferred_dynamic_history.as_ref().map_or_else(
            || self.stable_context_manifest.clone(),
            |measurement| measurement.measure(&self.input, &self.stable_context_manifest),
        )
    }

    /// The caller must verify these are the complete serialized bytes of
    /// `self.input`, not an incremental transport suffix or a fallback input.
    pub(crate) fn measured_stable_context_manifest_from_serialized_input(
        &self,
        input_bytes: &[u8],
    ) -> StableContextManifest {
        self.deferred_dynamic_history.as_ref().map_or_else(
            || self.stable_context_manifest.clone(),
            |measurement| {
                measurement.measure_serialized(input_bytes, &self.stable_context_manifest)
            },
        )
    }

    pub(crate) fn get_formatted_input_for_request(
        &self,
        use_responses_lite: bool,
    ) -> Arc<[ResponseItem]> {
        self.formatted_input(&self.input, use_responses_lite)
    }

    fn formatted_input(&self, source: &Arc<[ResponseItem]>, use_lite: bool) -> Arc<[ResponseItem]> {
        let identity = Arc::downgrade(source);
        {
            let cached = self.formatted_inputs.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = cached.iter().find(|entry| {
                entry.use_lite == use_lite && Weak::ptr_eq(&entry.source, &identity)
            }) {
                return entry.input.as_ref().map_or_else(|| Arc::clone(source), Arc::clone);
            }
        }
        // Work only on the selected, complete request. Never persist references
        // based on a previous prompt: retirement/fallback may remove its owner.
        let mut input = repeated_bodies::deduplicate(source);
        if use_lite && has_image_details(&input) {
            strip_image_details(Arc::make_mut(&mut input));
        }
        let mut cached = self.formatted_inputs.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = cached.iter().find(|entry| {
            entry.use_lite == use_lite && Weak::ptr_eq(&entry.source, &identity)
        }) {
            return entry.input.as_ref().map_or_else(|| Arc::clone(source), Arc::clone);
        }
        cached.retain(|entry| entry.source.strong_count() != 0);
        if cached.len() == 8 {
            cached.remove(0);
        }
        cached.push(FormattedPromptInput {
            source: identity,
            use_lite,
            input: (!Arc::ptr_eq(&input, source)).then(|| Arc::clone(&input)),
        });
        input
    }

    /// Deduplicate immutable sources before Lite projection can clone them.
    pub(crate) fn get_formatted_admission_inputs(
        &self,
        use_responses_lite: bool,
    ) -> Vec<Arc<[ResponseItem]>> {
        let sources = [
            &self.input,
            &self.stable_context_fallback_input,
            &self.tool_history_fallback_input,
            &self.stable_context_tool_history_fallback_input,
        ];
        let mut inputs = Vec::new();
        for (index, source) in sources.iter().enumerate() {
            if sources[..index].iter().any(|previous| Arc::ptr_eq(previous, source)) {
                continue;
            }
            inputs.push(self.formatted_input(source, use_responses_lite));
        }
        inputs
    }

    pub(crate) fn get_formatted_fallback_input_for_request(
        &self,
        use_responses_lite: bool,
    ) -> Arc<[ResponseItem]> {
        self.formatted_input(&self.stable_context_fallback_input, use_responses_lite)
    }

    pub(crate) fn get_formatted_tool_history_fallback_input_for_request(
        &self,
        use_responses_lite: bool,
        use_stable_context_fallback: bool,
    ) -> Arc<[ResponseItem]> {
        let input = if use_stable_context_fallback {
            &self.stable_context_tool_history_fallback_input
        } else {
            &self.tool_history_fallback_input
        };
        self.formatted_input(input, use_responses_lite)
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

/// Final response format is prompt input, unlike internal tool output metadata.
pub(crate) fn output_schema_tokens(schema: Option<&Value>, strict: bool) -> i64 {
    let text = codex_api::create_text_param_for_request(None, &schema.cloned(), strict);
    text.and_then(|text| serde_json::to_vec(&text).ok())
        .map_or(0, |bytes| {
            i64::try_from(bytes.len().div_ceil(4)).unwrap_or(i64::MAX)
        })
}

#[derive(Clone, Debug)]
pub(crate) struct PromptCalibrationBasis {
    pub(crate) local_tokens: i64,
    input: Arc<[ResponseItem]>,
    input_start: usize,
    tools: [u8; 32],
    base_instructions: BaseInstructions,
    output_schema: Option<Value>,
    strict: bool,
}

impl PromptCalibrationBasis {
    pub(crate) fn extends(&self, previous: &Self) -> bool {
        self.tools == previous.tools
            && self.base_instructions == previous.base_instructions
            && self.output_schema == previous.output_schema
            && self.strict == previous.strict
            && self.input[self.input_start..].starts_with(&previous.input[previous.input_start..])
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
    pub(crate) rx_event: ResponseEventReceiver<codex_protocol::error::CodexErr>,
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
