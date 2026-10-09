use crate::error::ApiError;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::config_types::Verbosity as VerbosityConfig;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::ModelVerification;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnModerationMetadataEvent;
use codex_protocol::protocol::W3cTraceContext;
use futures::Stream;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use tokio::sync::mpsc;

pub const WS_REQUEST_HEADER_TRACEPARENT_CLIENT_METADATA_KEY: &str = "ws_request_header_traceparent";
pub const WS_REQUEST_HEADER_TRACESTATE_CLIENT_METADATA_KEY: &str = "ws_request_header_tracestate";

#[derive(Debug)]
pub enum ResponseEvent {
    Created,
    SafetyBuffering(SafetyBuffering),
    OutputItemDone(ResponseItem),
    OutputItemAdded(ResponseItem),
    /// Emitted when the server includes `OpenAI-Model` on the stream response.
    /// This can differ from the requested model when backend safety routing applies.
    ServerModel(String),
    /// Emitted when the server recommends additional account verification.
    ModelVerifications(Vec<ModelVerification>),
    /// Emitted when the server includes moderation metadata for first-party turn presentation.
    TurnModerationMetadata(TurnModerationMetadataEvent),
    /// Emitted when `X-Reasoning-Included: true` is present on the response,
    /// meaning the server already accounted for past reasoning tokens and the
    /// client should not re-estimate them.
    ServerReasoningIncluded(bool),
    Completed {
        response_id: String,
        token_usage: Option<TokenUsage>,
        /// Did the model affirmatively end its turn? Some providers do not set this,
        /// so we rely on fallback logic when this is `None`.
        end_turn: Option<bool>,
    },
    OutputTextDelta(String),
    ToolCallInputDelta {
        item_id: String,
        call_id: Option<String>,
        delta: String,
    },
    ReasoningSummaryDelta {
        delta: String,
        summary_index: i64,
    },
    ReasoningSummaryDone {
        item_id: String,
        text: String,
        summary_index: i64,
    },
    ReasoningContentDelta {
        delta: String,
        content_index: i64,
    },
    ReasoningSummaryPartAdded {
        summary_index: i64,
    },
    RateLimits(RateLimitSnapshot),
    ModelsEtag(String),
}

impl ResponseEvent {
    pub(crate) fn advances_model_response(&self) -> bool {
        match self {
            Self::OutputTextDelta(delta)
            | Self::ToolCallInputDelta { delta, .. }
            | Self::ReasoningSummaryDelta { delta, .. }
            | Self::ReasoningContentDelta { delta, .. } => !delta.is_empty(),
            Self::OutputItemAdded(_) | Self::OutputItemDone(_)
            | Self::ReasoningSummaryDone { .. } | Self::Completed { .. } => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SafetyBuffering {
    pub use_cases: Vec<String>,
    pub reasons: Vec<String>,
    #[serde(skip)]
    pub show_buffering_ui: bool,
    #[serde(rename = "retry_model")]
    pub faster_model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SafetyBufferingTreatment {
    pub faster_model: Option<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningContext {
    Auto,
    CurrentTurn,
    AllTurns,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Reasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffortConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReasoningSummaryConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ReasoningContext>,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummaryDelivery {
    SequentialCutoff,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct StreamOptions {
    pub reasoning_summary_delivery: ReasoningSummaryDelivery,
}

#[derive(Debug, Serialize, Default, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TextFormatType {
    #[default]
    JsonSchema,
}

#[derive(Debug, Serialize, Default, Clone, PartialEq)]
pub struct TextFormat {
    /// Format type used by the OpenAI text controls.
    pub r#type: TextFormatType,
    /// When true, the server is expected to strictly validate responses.
    pub strict: bool,
    /// JSON schema for the desired output.
    pub schema: Value,
    /// Friendly name for the format, used in telemetry/debugging.
    pub name: String,
}

/// Controls the `text` field for the Responses API, combining verbosity and
/// optional JSON schema output formatting.
#[derive(Debug, Serialize, Default, Clone, PartialEq)]
pub struct TextControls {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<OpenAiVerbosity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<TextFormat>,
}

#[derive(Debug, Serialize, Default, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OpenAiVerbosity {
    Low,
    #[default]
    Medium,
    High,
}

impl From<VerbosityConfig> for OpenAiVerbosity {
    fn from(v: VerbosityConfig) -> Self {
        match v {
            VerbosityConfig::Low => OpenAiVerbosity::Low,
            VerbosityConfig::Medium => OpenAiVerbosity::Medium,
            VerbosityConfig::High => OpenAiVerbosity::High,
        }
    }
}

/// Request input can prepend the Responses Lite envelope without copying its
/// shared history. Contiguous access remains available for legacy consumers,
/// but serialization and ordinary iteration never materialize that copy.
#[derive(Debug, Clone, Default)]
pub struct ResponsesInput {
    prefix: Arc<[ResponseItem]>,
    shared: Arc<[ResponseItem]>,
    contiguous: std::sync::OnceLock<Arc<[ResponseItem]>>,
}

impl ResponsesInput {
    pub fn with_prefix(prefix: Vec<ResponseItem>, shared: Arc<[ResponseItem]>) -> Self {
        Self { prefix: prefix.into(), shared, contiguous: std::sync::OnceLock::new() }
    }

    pub fn iter(&self) -> impl Iterator<Item = &ResponseItem> + DoubleEndedIterator {
        self.prefix.iter().chain(self.shared.iter())
    }

    pub fn len(&self) -> usize { self.prefix.len() + self.shared.len() }
    pub fn is_empty(&self) -> bool { self.prefix.is_empty() && self.shared.is_empty() }
    pub fn first(&self) -> Option<&ResponseItem> { self.iter().next() }

    /// Indexed inspection must not materialize the contiguous compatibility copy.
    pub fn get_item(&self, index: usize) -> Option<&ResponseItem> {
        if index < self.prefix.len() {
            self.prefix.get(index)
        } else {
            self.shared.get(index - self.prefix.len())
        }
    }

    pub fn update_segments(&mut self, mut update: impl FnMut(&mut Arc<[ResponseItem]>)) {
        update(&mut self.prefix);
        update(&mut self.shared);
        self.contiguous = std::sync::OnceLock::new();
    }
}

impl From<Arc<[ResponseItem]>> for ResponsesInput {
    fn from(shared: Arc<[ResponseItem]>) -> Self { Self::with_prefix(Vec::new(), shared) }
}

impl From<Vec<ResponseItem>> for ResponsesInput {
    fn from(items: Vec<ResponseItem>) -> Self { Self::from(Arc::<[ResponseItem]>::from(items)) }
}

impl std::ops::Deref for ResponsesInput {
    type Target = Arc<[ResponseItem]>;
    fn deref(&self) -> &Self::Target {
        if self.prefix.is_empty() { return &self.shared; }
        self.contiguous.get_or_init(|| self.iter().cloned().collect::<Vec<_>>().into())
    }
}

impl std::ops::DerefMut for ResponsesInput {
    fn deref_mut(&mut self) -> &mut Self::Target {
        if !self.prefix.is_empty() {
            self.shared = self.contiguous.take()
                .unwrap_or_else(|| self.iter().cloned().collect::<Vec<_>>().into());
            self.prefix = Arc::from([]);
        }
        &mut self.shared
    }
}

impl PartialEq for ResponsesInput {
    fn eq(&self, other: &Self) -> bool { self.iter().eq(other.iter()) }
}

impl Serialize for ResponsesInput {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(self.len()))?;
        for item in self.iter() { sequence.serialize_element(item)?; }
        sequence.end()
    }
}

#[cfg(test)]
mod responses_input_tests {
    use super::*;

    #[test]
    fn prefixed_serialization_keeps_exact_bytes_without_copying_shared_history() {
        let item = |text: &str| ResponseItem::Message {
            id: None, role: "developer".into(),
            content: vec![codex_protocol::models::ContentItem::InputText { text: text.into() }],
            phase: None, internal_chat_message_metadata_passthrough: None,
        };
        let history: Arc<[ResponseItem]> = Arc::from([item("shared history")]);
        for prefix in [Vec::new(), vec![item("fixed prefix")]] {
            let expected = prefix.iter().chain(history.iter()).cloned().collect::<Vec<_>>();
            let input = ResponsesInput::with_prefix(prefix, Arc::clone(&history));
            for (index, item) in input.iter().enumerate() {
                assert!(std::ptr::eq(input.get_item(index).unwrap(), item));
            }
            assert!(input.get_item(input.len()).is_none());
            assert!(input.get_item(usize::MAX).is_none());
            for _ in 0..2 {
                assert_eq!(serde_json::to_vec(&input).unwrap(), serde_json::to_vec(&expected).unwrap());
                assert!(input.contiguous.get().is_none());
                assert!(Arc::ptr_eq(&input.shared, &history));
            }
            let mut modified = input.clone();
            Arc::make_mut(&mut modified)[0] = item("rewritten");
            let mut expected_modified = expected.clone();
            expected_modified[0] = item("rewritten");
            assert_eq!(serde_json::to_vec(&modified).unwrap(), serde_json::to_vec(&expected_modified).unwrap());
            assert_eq!(serde_json::to_vec(&input).unwrap(), serde_json::to_vec(&expected).unwrap());
        }
    }
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct ResponsesApiRequest {
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub instructions: String,
    pub input: ResponsesInput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Arc<[Value]>>,
    pub tool_choice: String,
    pub parallel_tool_calls: bool,
    pub reasoning: Option<Reasoning>,
    pub store: bool,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    pub include: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextControls>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_metadata: Option<HashMap<String, String>>,
}

impl From<&ResponsesApiRequest> for ResponseCreateWsRequest {
    fn from(request: &ResponsesApiRequest) -> Self {
        Self {
            model: request.model.clone(),
            instructions: request.instructions.clone(),
            previous_response_id: None,
            input: request.input.clone(),
            tools: request.tools.clone(),
            tool_choice: request.tool_choice.clone(),
            parallel_tool_calls: request.parallel_tool_calls,
            reasoning: request.reasoning.clone(),
            store: request.store,
            stream: request.stream,
            stream_options: request.stream_options.clone(),
            include: request.include.clone(),
            service_tier: request.service_tier.clone(),
            prompt_cache_key: request.prompt_cache_key.clone(),
            text: request.text.clone(),
            generate: None,
            client_metadata: request.client_metadata.clone(),
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct ResponseCreateWsRequest {
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub instructions: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    pub input: ResponsesInput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Arc<[Value]>>,
    pub tool_choice: String,
    pub parallel_tool_calls: bool,
    pub reasoning: Option<Reasoning>,
    pub store: bool,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    pub include: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextControls>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_metadata: Option<HashMap<String, String>>,
}

pub fn response_create_client_metadata(
    client_metadata: Option<HashMap<String, String>>,
    trace: Option<&W3cTraceContext>,
) -> Option<HashMap<String, String>> {
    let mut client_metadata = client_metadata.unwrap_or_default();

    if let Some(traceparent) = trace.and_then(|trace| trace.traceparent.as_deref()) {
        client_metadata.insert(
            WS_REQUEST_HEADER_TRACEPARENT_CLIENT_METADATA_KEY.to_string(),
            traceparent.to_string(),
        );
    }
    if let Some(tracestate) = trace.and_then(|trace| trace.tracestate.as_deref()) {
        client_metadata.insert(
            WS_REQUEST_HEADER_TRACESTATE_CLIENT_METADATA_KEY.to_string(),
            tracestate.to_string(),
        );
    }

    (!client_metadata.is_empty()).then_some(client_metadata)
}

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)]
pub enum ResponsesWsRequest {
    #[serde(rename = "response.create")]
    ResponseCreate(ResponseCreateWsRequest),
}

pub fn create_text_param_for_request(
    verbosity: Option<VerbosityConfig>,
    output_schema: &Option<Value>,
    output_schema_strict: bool,
) -> Option<TextControls> {
    if verbosity.is_none() && output_schema.is_none() {
        return None;
    }

    Some(TextControls {
        verbosity: verbosity.map(std::convert::Into::into),
        format: output_schema.as_ref().map(|schema| TextFormat {
            r#type: TextFormatType::JsonSchema,
            strict: output_schema_strict,
            schema: schema.clone(),
            name: "codex_output_schema".to_string(),
        }),
    })
}

pub struct ResponseStream {
    pub rx_event: mpsc::Receiver<Result<ResponseEvent, ApiError>>,
    /// Server-assigned `x-request-id` response header, when present.
    pub upstream_request_id: Option<String>,
}

impl Stream for ResponseStream {
    type Item = Result<ResponseEvent, ApiError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx_event.poll_recv(cx)
    }
}
