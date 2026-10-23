use crate::common::ResponseEvent;
use crate::common::SafetyBuffering;
use crate::common::SafetyBufferingTreatment;
use crate::error::ApiError;
use crate::rate_limits::RateLimitEventBody;
use crate::rate_limits::parse_all_rate_limits;
use crate::rate_limits::rate_limit_snapshot_from_event;
use crate::safety_buffering::X_CODEX_SAFETY_BUFFERING_ENABLED_HEADER;
use crate::safety_buffering::X_CODEX_SAFETY_BUFFERING_FASTER_MODEL_HEADER;
use crate::safety_buffering::treatment_from_headers;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ModelVerification;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnModerationMetadataEvent;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use serde::Deserialize;
use serde_json::Value;
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tracing::debug;
use tracing::trace;

pub(crate) const X_REASONING_INCLUDED_HEADER: &str = "x-reasoning-included";
pub(crate) const X_MODELS_ETAG_HEADER: &str = "x-models-etag";
pub const X_CODEX_TURN_STATE_HEADER: &str = "x-codex-turn-state";
pub(crate) const OPENAI_MODEL_HEADER: &str = "openai-model";
pub(crate) const REQUEST_ID_HEADER: &str = "x-request-id";
const TRUSTED_ACCESS_FOR_CYBER_VERIFICATION: &str = "trusted_access_for_cyber";

#[path = "response_lifecycle.rs"]
mod response_lifecycle;
pub(crate) use response_lifecycle::ConnectionResponseHistory;

/// Transport-independent metadata returned when a Responses stream is established.
#[derive(Debug, Clone)]
pub(crate) struct ResponsesStreamMetadata {
    pub(crate) ollama_single_summary: bool,
    rate_limit_snapshots: Vec<RateLimitSnapshot>,
    models_etag: Option<String>,
    server_model: Option<String>,
    reasoning_included: bool,
    upstream_request_id: Option<String>,
    safety_buffering_treatment: SafetyBufferingTreatment,
    turn_state: Option<String>,
}

impl Default for ResponsesStreamMetadata {
    fn default() -> Self {
        Self::from_headers(&HeaderMap::new())
    }
}

impl ResponsesStreamMetadata {
    pub(crate) fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            ollama_single_summary: false,
            rate_limit_snapshots: parse_all_rate_limits(headers),
            models_etag: header_string(headers, X_MODELS_ETAG_HEADER),
            server_model: header_string(headers, OPENAI_MODEL_HEADER),
            reasoning_included: headers
                .get(X_REASONING_INCLUDED_HEADER)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("true")),
            upstream_request_id: header_string(headers, REQUEST_ID_HEADER),
            safety_buffering_treatment: treatment_from_headers(headers).unwrap_or_default(),
            turn_state: header_string(headers, X_CODEX_TURN_STATE_HEADER),
        }
    }

    pub(crate) fn initial_events(&self) -> Vec<ResponseEvent> {
        let mut events = Vec::new();
        if let Some(model) = self.server_model.clone() {
            events.push(ResponseEvent::ServerModel(model));
        }
        events.extend(
            self.rate_limit_snapshots
                .iter()
                .cloned()
                .map(ResponseEvent::RateLimits),
        );
        if let Some(etag) = self.models_etag.clone() {
            events.push(ResponseEvent::ModelsEtag(etag));
        }
        if self.reasoning_included {
            events.push(ResponseEvent::ServerReasoningIncluded(true));
        }
        events
    }

    pub(crate) fn apply_turn_state(&self, turn_state: Option<&OnceLock<String>>) {
        if let Some(turn_state) = turn_state
            && let Some(response_turn_state) = self.turn_state.clone()
        {
            let _ = turn_state.set(response_turn_state);
        }
    }

    pub(crate) fn upstream_request_id(&self) -> Option<&str> {
        self.upstream_request_id.as_deref()
    }

    pub(crate) fn reasoning_included(&self) -> bool {
        self.reasoning_included
    }

    pub(crate) fn models_etag_present(&self) -> bool {
        self.models_etag.is_some()
    }

    pub(crate) fn server_model_present(&self) -> bool {
        self.server_model.is_some()
    }
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Stateful, transport-independent interpreter for Responses stream payloads.
pub(crate) struct ResponsesEventInterpreter {
    ollama_single_summary: bool,
    last_server_model: Option<String>,
    safety_buffering_treatment: SafetyBufferingTreatment,
    turn_state: Option<Arc<OnceLock<String>>>,
    events: Vec<ResponseEvent>,
    lifecycle: response_lifecycle::ResponseLifecycle,
}

impl ResponsesEventInterpreter {
    pub(crate) fn with_connection_history(mut self, history: ConnectionResponseHistory) -> Self {
        self.lifecycle = response_lifecycle::ResponseLifecycle::with_connection_history(history);
        self
    }

    pub(crate) fn into_connection_history(self) -> ConnectionResponseHistory {
        self.lifecycle.into_connection_history()
    }

    pub(crate) fn new(
        metadata: &ResponsesStreamMetadata,
        turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Self {
        Self {
            last_server_model: metadata.server_model.clone(),
            ollama_single_summary: metadata.ollama_single_summary,
            safety_buffering_treatment: metadata.safety_buffering_treatment.clone(),
            turn_state,
            events: Vec::new(),
            lifecycle: response_lifecycle::ResponseLifecycle::default(),
        }
    }

    pub(crate) fn process_payload(
        &mut self,
        payload: &str,
    ) -> Result<std::vec::Drain<'_, ResponseEvent>, ResponsesEventError> {
        self.process_payload_with_error_mapper(payload, || None)
    }

    /// Let a transport interpret its error envelope without reparsing ordinary events.
    /// Also try the mapper on deserialization failures: transport errors may use
    /// fields that are incompatible with the Responses event schema.
    pub(crate) fn process_payload_with_error_mapper(
        &mut self,
        payload: &str,
        map_error: impl FnOnce() -> Option<ApiError>,
    ) -> Result<std::vec::Drain<'_, ResponseEvent>, ResponsesEventError> {
        // Retain the allocation across frames, including when a previous frame
        // failed after interpreting metadata. Both transports drain in order.
        self.events.clear();
        // Ordinary frames take one JSON pass and never use flattened-field buffering.
        // A rate-limit frame may contain fields incompatible with the ordinary shape,
        // so only failed deserialization needs a discriminator-only fallback.
        #[derive(Deserialize)]
        struct EventKindProbe<'a> {
            #[serde(rename = "type", borrow, default)]
            kind: Option<std::borrow::Cow<'a, str>>,
        }

        let event = serde_json::from_str::<ResponsesStreamEvent>(payload);
        if match &event {
            Ok(event) => event.kind == "error",
            Err(_) => true,
        } && let Some(error) = map_error()
        {
            return Err(ResponsesEventError::Api(error));
        }
        let kind = match &event {
            Ok(event) => Some(event.kind.clone()),
            Err(_) => serde_json::from_str::<EventKindProbe<'_>>(payload)
                .ok()
                .and_then(|probe| probe.kind),
        };
        if kind.as_deref() == Some("error") {
            let value: Value = serde_json::from_str(payload)?;
            let error = value.get("error").unwrap_or(&value);
            return Err(ResponsesEventError::Api(provider_error(error)));
        }
        // Unknown extensions may reuse field names with new types. Known events
        // remain strict, especially output items containing executable actions.
        if event.is_err() && kind.as_deref().is_some_and(|kind| !is_known_event(kind)) {
            return Ok(self.events.drain(..));
        }
        let is_rate_limit = kind.as_deref() == Some("codex.rate_limits");
        if is_rate_limit {
            let event: RateLimitStreamEvent = serde_json::from_str(payload)?;
            self.events.extend(
                rate_limit_snapshot_from_event(event.rate_limit).map(ResponseEvent::RateLimits),
            );
            return Ok(self.events.drain(..));
        }

        let mut event = event?;
        // Ollama's admitted single-summary dialect omits this index. Keep this
        // opt-in below trusted provider selection; generic Responses stays strict.
        if self.ollama_single_summary
            && event.summary_index.is_none()
            && matches!(
                event.kind.as_ref(),
                "response.reasoning_summary_text.delta" | "response.reasoning_summary_text.done"
            )
        {
            event.summary_index = Some(0);
        }

        if let Some(response_turn_state) = event.turn_state()
            && let Some(turn_state) = self.turn_state.as_deref()
        {
            let _ = turn_state.set(response_turn_state);
        }

        if let Some(headers) = event.headers.as_ref().and_then(Value::as_object)
            && let Some(updated_treatment) = treatment_from_headers(&json_headers_to_http_headers(
                headers.iter().filter(|(name, _)| {
                    name.eq_ignore_ascii_case(X_CODEX_SAFETY_BUFFERING_ENABLED_HEADER)
                        || name.eq_ignore_ascii_case(X_CODEX_SAFETY_BUFFERING_FASTER_MODEL_HEADER)
                }),
            ))
        {
            self.safety_buffering_treatment = updated_treatment;
        }

        if let Some(response) = event.response.as_ref()
            && let Some(reported_model) = response.model.as_ref().and_then(Value::as_str)
        {
            let header_model = response.headers.as_ref().and_then(header_openai_model_value_from_json)
                .or_else(|| event.headers.as_ref().and_then(header_openai_model_value_from_json));
            tracing::info!(event.name = "codex.provider_reported_model",
                response_id = response.id.as_ref().and_then(|value| value.as_str()).unwrap_or_default(),
                reported_model, provenance = "response.model", header_model,
                "provider-reported identity; not an inferred reroute reason");
        }
        if let Some(model) = event.response_model()
            && self.last_server_model.as_deref() != Some(model)
        {
            self.last_server_model = Some(model.to_owned());
            self.events
                .push(ResponseEvent::ServerModel(model.to_owned()));
        }
        if let Some(verifications) = event.model_verifications() {
            self.events
                .push(ResponseEvent::ModelVerifications(verifications));
        }
        if let Some(metadata) = event.turn_moderation_metadata() {
            self.events
                .push(ResponseEvent::TurnModerationMetadata(metadata));
        }
        if let Some(buffering) = event.safety_buffering(&self.safety_buffering_treatment) {
            self.events.push(ResponseEvent::SafetyBuffering(buffering));
        }
        let completion_usage = if event.kind == "response.completed" {
            parse_usage(event.response.as_mut().and_then(|response| response.usage.take()))
        } else {
            None
        };
        let rejected_completion_usage = if event.kind == "response.completed" {
            event.response.as_ref().and_then(|response| {
                let response_id = response.id.as_ref().and_then(Value::as_str);
                if !self.lifecycle.owns_terminal_observation(response_id) {
                    return None;
                }
                completion_usage.clone()
                    .map(|usage| (response_id.map(str::to_owned), usage))
            })
        } else {
            None
        };
        let interpreted = self
            .lifecycle
            .process(event, payload.len(), completion_usage)
            .map_err(|error| match (error, rejected_completion_usage) {
                (ResponsesEventError::Api(source), Some((response_id, usage))) => {
                    ResponsesEventError::Api(ApiError::TerminalResponse {
                        source: Box::new(source),
                        response_id,
                        token_usage: Some(usage),
                    })
                }
                (error, _) => error,
            })?;
        if let Some(event) = interpreted {
            self.events.push(event);
        }
        Ok(self.events.drain(..))
    }
}

#[derive(Debug)]
pub(crate) enum ResponsesEventError {
    Parse(serde_json::Error),
    Api(ApiError),
}

impl From<serde_json::Error> for ResponsesEventError {
    fn from(error: serde_json::Error) -> Self {
        Self::Parse(error)
    }
}

#[derive(Debug, Deserialize)]
struct Error {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct ResponsesStreamEvent<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    headers: Option<Value>,
    metadata: Option<Value>,
    response: Option<ResponseEnvelope>,
    response_id: Option<String>,
    item: Option<Value>,
    item_id: Option<String>,
    call_id: Option<String>,
    delta: Option<String>,
    text: Option<String>,
    summary_index: Option<i64>,
    content_index: Option<i64>,
    output_index: Option<u64>,
    safety_buffering: Option<Value>,
}

// Terminal snapshots, when supplied, must agree with admitted ordered items.
#[derive(Debug, Deserialize)]
struct ResponseEnvelope {
    id: Option<Value>,
    model: Option<Value>,
    usage: Option<Value>,
    end_turn: Option<Value>,
    error: Option<Value>,
    incomplete_details: Option<Value>,
    output: Option<Value>,
    headers: Option<Value>,
}

fn is_known_event(kind: &str) -> bool {
    matches!(
        kind,
        "error"
            | "codex.rate_limits"
            | "response.metadata"
            | "response.created"
            | "response.in_progress"
            | "response.failed"
            | "response.incomplete"
            | "response.completed"
            | "response.output_item.done"
            | "response.output_item.added"
            | "response.output_text.delta"
            | "response.custom_tool_call_input.delta"
            | "response.function_call_arguments.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_summary_part.added"
    )
}

pub(crate) fn decode_diagnostic(context: &str, error: &serde_json::Error) -> String {
    format!(
        "{context}: {:?} at line {} column {}",
        error.classify(),
        error.line(),
        error.column()
    )
}

#[cfg(test)]
thread_local! {
    static USAGE_PAYLOAD_PARSE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn parse_usage(value: Option<Value>) -> Option<TokenUsage> {
    use codex_protocol::protocol::TokenUsageCoverage;
    let value = value?;
    #[cfg(test)]
    USAGE_PAYLOAD_PARSE_COUNT.with(|count| count.set(count.get() + 1));
    if value.is_null() {
        return None;
    }
    let Some(object) = value.as_object() else {
        return Some(TokenUsage {
            coverage: Some(TokenUsageCoverage {
                invalid_receipts: 1,
                reported_receipts: Some(1),
                ..TokenUsage::missing_receipt().coverage.expect("missing receipt coverage")
            }),
            ..Default::default()
        });
    };
    let mut invalid = false;
    let mut count = |value: Option<&Value>| {
        let parsed = value.and_then(Value::as_i64).filter(|value| *value >= 0);
        invalid |= value.is_some_and(|value| !value.is_null()) && parsed.is_none();
        parsed
    };
    let input = count(object.get("input_tokens"));
    let output = count(object.get("output_tokens"));
    let total = count(object.get("total_tokens"));
    let cached = count(
        object
            .get("input_tokens_details")
            .and_then(|v| v.get("cached_tokens")),
    );
    let cache_write = count(
        object
            .get("input_tokens_details")
            .and_then(|v| v.get("cache_write_tokens")),
    );
    let reasoning = count(
        object
            .get("output_tokens_details")
            .and_then(|v| v.get("reasoning_tokens")),
    );
    invalid |= ["input_tokens_details", "output_tokens_details"]
        .into_iter()
        .any(|key| {
            object
                .get(key)
                .is_some_and(|value| !value.is_null() && !value.is_object())
        });
    invalid |= input
        .zip(cached)
        .is_some_and(|(input, cached)| cached > input);
    invalid |= output
        .zip(reasoning)
        .is_some_and(|(output, reasoning)| reasoning > output);
    invalid |= input
        .zip(cached)
        .zip(cache_write)
        .is_some_and(|((input, cached), write)| {
            cached.checked_add(write).is_none_or(|sum| sum > input)
        });
    Some(TokenUsage {
        input_tokens: input.unwrap_or(0),
        cached_input_tokens: cached.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
        reasoning_output_tokens: reasoning.unwrap_or(0),
        total_tokens: total.unwrap_or(0),
        coverage: Some(TokenUsageCoverage {
            receipts: 1,
            input_tokens: u64::from(input.is_some()),
            reported_receipts: Some(1),
            cache_write_input_tokens: cache_write.unwrap_or(0),
            cache_write_receipts: u64::from(cache_write.is_some()),
            cached_input_tokens: u64::from(cached.is_some()),
            output_tokens: u64::from(output.is_some()),
            reasoning_output_tokens: u64::from(reasoning.is_some()),
            total_tokens: u64::from(total.is_some()),
            invalid_receipts: u64::from(invalid),
            saturated: false,
        }),
    })
}

/// Stable error fields are independent of optional provider metadata. HTTP
/// adapters keep their status-based fallback when the provider code is unknown.
pub(crate) fn classify_provider_error(value: &Value) -> Option<ApiError> {
    let code = value
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| value.get("type").and_then(Value::as_str));
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Some(match code? {
        "context_length_exceeded" => ApiError::ContextWindowExceeded,
        "insufficient_quota" | "quota_exceeded" | "billing_hard_limit_reached" => {
            ApiError::QuotaExceeded
        }
        "usage_not_included" => ApiError::UsageNotIncluded,
        "cyber_policy" => ApiError::CyberPolicy {
            message: cyber_policy_message(Some(message)),
        },
        "server_is_overloaded" | "slow_down" => ApiError::ServerOverloaded { retry_after: None },
        "server_error" | "internal_server_error" | "rate_limit_exceeded" => {
            let delay = try_parse_retry_after(&Error {
                code: code.map(str::to_owned),
                message: Some(message.clone()),
            });
            ApiError::Retryable {
                message,
                delay: delay.and_then(codex_http_client::RetryAfter::from_delay),
            }
        }
        "invalid_request_error"
        | "invalid_prompt"
        | "bio_policy"
        | "content_policy_violation"
        | "invalid_image"
        | "invalid_image_format"
        | "invalid_base64_image"
        | "invalid_image_url"
        | "image_too_large"
        | "image_too_small"
        | "image_parse_error"
        | "image_content_policy_violation"
        | "invalid_image_mode"
        | "image_file_too_large"
        | "unsupported_image_media_type"
        | "empty_image_file"
        | "image_file_not_found" => ApiError::InvalidRequest { message },
        _ => return None,
    })
}

fn provider_error(value: &Value) -> ApiError {
    classify_provider_error(value).unwrap_or_else(|| ApiError::ProviderFailure {
        code: value.get("code").and_then(Value::as_str).map(str::to_owned),
        message: value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Unrecognized provider failure")
            .to_owned(),
    })
}

/// Rate-limit frames only.
///
/// `#[serde(flatten)]` forces serde to buffer every unmatched key of the enclosing object
/// into owned `Content` values. Keeping it on this dedicated struct means ordinary stream
/// frames, which vastly outnumber rate-limit frames, deserialize through the direct struct
/// visitor instead of paying that buffering per delta.
#[derive(Deserialize, Debug)]
pub(crate) struct RateLimitStreamEvent {
    #[serde(flatten)]
    rate_limit: RateLimitEventBody,
}

impl ResponsesStreamEvent<'_> {
    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }

    pub(crate) fn response_model(&self) -> Option<&str> {
        self.response
            .as_ref()
            .and_then(|response| response.headers.as_ref())
            .and_then(header_openai_model_value_from_json)
            .or_else(|| {
                self.headers
                    .as_ref()
                    .and_then(header_openai_model_value_from_json)
            })
    }

    fn turn_state(&self) -> Option<String> {
        (self.kind() == "response.metadata")
            .then(|| {
                self.headers
                    .as_ref()
                    .and_then(header_turn_state_value_from_json)
            })
            .flatten()
    }

    pub(crate) fn model_verifications(&self) -> Option<Vec<ModelVerification>> {
        (self.kind() == "response.metadata")
            .then(|| {
                self.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("openai_verification_recommendation"))
                    .and_then(model_verifications_from_json_value)
            })
            .flatten()
    }

    fn turn_moderation_metadata(&self) -> Option<TurnModerationMetadataEvent> {
        (self.kind() == "response.metadata")
            .then(|| {
                self.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("openai_chatgpt_moderation_metadata"))
                    .cloned()
                    .map(|metadata| TurnModerationMetadataEvent { metadata })
            })
            .flatten()
    }

    pub(crate) fn safety_buffering(
        &self,
        treatment: &SafetyBufferingTreatment,
    ) -> Option<SafetyBuffering> {
        let value = self.safety_buffering.as_ref()?;
        let retry_model_present = value.as_object()?.contains_key("retry_model");
        let mut buffering = SafetyBuffering::deserialize(value).ok()?;
        buffering.show_buffering_ui = true;
        if !retry_model_present {
            buffering.faster_model.clone_from(&treatment.faster_model);
        }
        Some(buffering)
    }
}

fn header_openai_model_value_from_json(value: &Value) -> Option<&str> {
    value.as_object()?.iter().find_map(|(name, value)| {
        (name.eq_ignore_ascii_case(OPENAI_MODEL_HEADER)
            || name.eq_ignore_ascii_case("x-openai-model"))
        .then(|| json_value_as_string(value))
        .flatten()
    })
}

fn header_turn_state_value_from_json(value: &Value) -> Option<String> {
    value.as_object()?.iter().find_map(|(name, value)| {
        name.eq_ignore_ascii_case(X_CODEX_TURN_STATE_HEADER)
            .then(|| json_value_as_string(value).map(str::to_owned))
            .flatten()
    })
}

fn model_verifications_from_json_value(value: &Value) -> Option<Vec<ModelVerification>> {
    let verifications = value
        .as_array()
        .map(|items| {
            let mut verifications = Vec::new();
            for verification in items
                .iter()
                .filter_map(Value::as_str)
                .filter_map(parse_model_verification)
            {
                if !verifications.contains(&verification) {
                    verifications.push(verification);
                }
            }
            verifications
        })
        .unwrap_or_default();
    (!verifications.is_empty()).then_some(verifications)
}

fn parse_model_verification(value: &str) -> Option<ModelVerification> {
    match value {
        TRUSTED_ACCESS_FOR_CYBER_VERIFICATION => Some(ModelVerification::TrustedAccessForCyber),
        _ => None,
    }
}

fn json_value_as_string(value: &Value) -> Option<&str> {
    match value {
        Value::String(value) => Some(value.as_str()),
        Value::Array(items) => items.first().and_then(json_value_as_string),
        _ => None,
    }
}

fn process_responses_event(
    event: ResponsesStreamEvent<'_>,
    completion_usage: Option<TokenUsage>,
) -> Result<Option<ResponseEvent>, ResponsesEventError> {
    match event.kind.as_ref() {
        "response.output_item.done" => {
            let item = parse_required_response_item("response.output_item.done", event.item)?;
            return Ok(Some(ResponseEvent::OutputItemDone(item)));
        }
        "response.output_text.delta" => {
            if let Some(delta) = event.delta {
                return Ok(Some(ResponseEvent::OutputTextDelta(delta)));
            }
        }
        "response.custom_tool_call_input.delta" | "response.function_call_arguments.delta" => {
            if let (Some(delta), Some(item_id)) =
                (event.delta, event.item_id.or_else(|| event.call_id.clone()))
            {
                return Ok(Some(ResponseEvent::ToolCallInputDelta {
                    item_id,
                    call_id: event.call_id,
                    delta,
                }));
            }
        }
        "response.reasoning_summary_text.delta" => {
            if let (Some(delta), Some(summary_index)) = (event.delta, event.summary_index) {
                return Ok(Some(ResponseEvent::ReasoningSummaryDelta {
                    delta,
                    summary_index,
                }));
            }
        }
        "response.reasoning_summary_text.done" => {
            if let (Some(item_id), Some(text), Some(summary_index)) =
                (event.item_id, event.text, event.summary_index)
            {
                return Ok(Some(ResponseEvent::ReasoningSummaryDone {
                    item_id,
                    text,
                    summary_index,
                }));
            }
        }
        "response.reasoning_text.delta" => {
            if let (Some(delta), Some(content_index)) = (event.delta, event.content_index) {
                return Ok(Some(ResponseEvent::ReasoningContentDelta {
                    delta,
                    content_index,
                }));
            }
        }
        "response.created" => {
            if event.response.is_some() {
                return Ok(Some(ResponseEvent::Created {}));
            }
        }
        "response.failed" => {
            let response = event.response;
            let response_id = response
                .as_ref()
                .and_then(|response| response.id.as_ref())
                .and_then(Value::as_str)
                .map(str::to_owned);
            let error = provider_error(
                response
                    .as_ref()
                    .and_then(|response| response.error.as_ref())
                    .unwrap_or(&Value::Null),
            );
            let token_usage = parse_usage(response.and_then(|response| response.usage));
            return Err(ResponsesEventError::Api(
                if response_id.is_some() || token_usage.is_some() {
                    ApiError::TerminalResponse {
                        source: Box::new(error),
                        response_id,
                        token_usage,
                    }
                } else {
                    error
                },
            ));
        }
        "response.incomplete" => {
            let response = event.response;
            let reason = response
                .as_ref()
                .and_then(|response| response.incomplete_details.as_ref())
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            let response_id = response
                .as_ref()
                .and_then(|response| response.id.as_ref())
                .and_then(Value::as_str)
                .map(str::to_owned);
            let token_usage = parse_usage(response.and_then(|response| response.usage));
            return Err(ResponsesEventError::Api(ApiError::IncompleteResponse(
                Box::new(codex_protocol::error::IncompleteResponse {
                    response_id,
                    reason,
                    token_usage,
                }),
            )));
        }
        "response.completed" => {
            let response = event.response.ok_or_else(|| {
                ResponsesEventError::Api(ApiError::invalid_response(
                    "response.completed event missing response",
                ))
            })?;
            // Decode only the required fields. Rebuilding a JSON object here
            // clones optional usage trees and the response id on every completion.
            let (response_id, end_turn) = (|| -> serde_json::Result<(String, Option<bool>)> {
                Ok((
                    serde_json::from_value(response.id.unwrap_or(Value::Null))?,
                    serde_json::from_value(response.end_turn.unwrap_or(Value::Null))?,
                ))
            })()
            .map_err(|error| {
                ResponsesEventError::Api(ApiError::invalid_response(decode_diagnostic(
                    "failed to parse ResponseCompleted",
                    &error,
                )))
            })?;
            return Ok(Some(ResponseEvent::Completed {
                response_id,
                token_usage: completion_usage.or_else(|| parse_usage(response.usage)),
                end_turn,
            }));
        }
        "response.output_item.added" => {
            let item = parse_required_response_item("response.output_item.added", event.item)?;
            return Ok(Some(ResponseEvent::OutputItemAdded(item)));
        }
        "response.reasoning_summary_part.added" => {
            if let Some(summary_index) = event.summary_index {
                return Ok(Some(ResponseEvent::ReasoningSummaryPartAdded {
                    summary_index,
                }));
            }
        }
        _ => trace!("unhandled responses event: {}", event.kind),
    }
    Ok(None)
}

fn parse_required_response_item(
    event_kind: &str,
    item: Option<Value>,
) -> Result<ResponseItem, ResponsesEventError> {
    let item = item.ok_or_else(|| {
        let message = format!("{event_kind} event missing item");
        debug!("{message}");
        ResponsesEventError::Api(ApiError::invalid_response(message))
    })?;
    // The event name is not sufficient authority to execute a tool. Check
    // explicit provider finality before deserialization can discard status
    // (notably for legacy FunctionCall). Omitted/null status remains compatible.
    if event_kind == "response.output_item.done"
        && matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call" | "custom_tool_call" | "tool_search_call" | "local_shell_call")
        )
        && let Some(status) = item.get("status").filter(|status| !status.is_null())
        && status.as_str() != Some("completed")
    {
        return Err(ResponsesEventError::Api(ApiError::invalid_response(
            "executable output item has a non-completed status",
        )));
    }
    let mut item: ResponseItem = serde_json::from_value(item).map_err(|error| {
        let message = decode_diagnostic(
            &format!("failed to parse ResponseItem from {event_kind}"),
            &error,
        );
        debug!("{message}");
        ResponsesEventError::Api(ApiError::invalid_response(message))
    })?;
    // ResponseItem is also the durable host-input schema. Deserializing it
    // does not grant a provider authority to create local policy or mail.
    let admitted = match &item {
        ResponseItem::Message { role, content, .. } => {
            role == "assistant"
                && content.iter().all(|part| {
                    matches!(part, codex_protocol::models::ContentItem::OutputText { .. })
                })
        }
        ResponseItem::Reasoning { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. } => true,
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::Other => false,
    };
    if !admitted {
        return Err(ResponsesEventError::Api(ApiError::invalid_response(
            format!("{event_kind} contains a host-only or invalid provider output item"),
        )));
    }
    item.clear_internal_chat_message_metadata_passthrough();
    Ok(item)
}

pub(crate) fn json_headers_to_http_headers<'a>(
    headers: impl IntoIterator<Item = (&'a String, &'a Value)>,
) -> HeaderMap {
    let mut mapped = HeaderMap::new();
    for (name, value) in headers {
        let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Some(header_value) = json_header_value(value) else {
            continue;
        };
        mapped.insert(header_name, header_value);
    }
    mapped
}

fn json_header_value(value: &Value) -> Option<HeaderValue> {
    let value = match value {
        Value::String(value) => Cow::Borrowed(value.as_str()),
        Value::Number(value) => Cow::Owned(value.to_string()),
        Value::Bool(value) => Cow::Owned(value.to_string()),
        _ => return None,
    };
    HeaderValue::from_str(&value).ok()
}

fn try_parse_retry_after(error: &Error) -> Option<Duration> {
    if error.code.as_deref() != Some("rate_limit_exceeded") {
        return None;
    }
    let captures = rate_limit_regex().captures(error.message.as_ref()?)?;
    // Long waits arrive as compound durations such as `1m30s` or `1h2m3.5s`;
    // an unparseable or overflowing component discards the whole advice.
    let mut delay = Duration::ZERO;
    for component in retry_delay_component_regex().captures_iter(captures.get(1)?.as_str()) {
        let value = component.get(1)?.as_str().parse::<f64>().ok()?;
        let seconds = match component.get(2)?.as_str().to_ascii_lowercase().as_str() {
            "ms" | "millisecond" | "milliseconds" => value / 1000.0,
            "s" | "sec" | "secs" | "second" | "seconds" => value,
            "m" | "min" | "mins" | "minute" | "minutes" => value * 60.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => value * 3600.0,
            _ => return None,
        };
        delay = delay.checked_add(Duration::try_from_secs_f64(seconds).ok()?)?;
    }
    Some(delay)
}

fn cyber_policy_message(message: Option<String>) -> String {
    message
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| "This request has been flagged for possible cybersecurity risk.".into())
}

fn rate_limit_regex() -> &'static regex_lite::Regex {
    static RE: OnceLock<regex_lite::Regex> = OnceLock::new();
    #[expect(clippy::unwrap_used)]
    RE.get_or_init(|| {
        regex_lite::Regex::new(&format!(r"(?i)try again in\s*((?:{RETRY_DELAY_COMPONENT}\s*)+)\b"))
            .unwrap()
    })
}

/// One `<number><unit>` part of a retry delay. Alternation is ordered, so
/// longer units precede their prefixes (`ms` before `m`, `mins` before `m`).
const RETRY_DELAY_COMPONENT: &str =
    r"(\d+(?:\.\d+)?)\s*(ms|milliseconds?|seconds?|secs?|s|minutes?|mins?|m|hours?|hrs?|h)";

fn retry_delay_component_regex() -> &'static regex_lite::Regex {
    static RE: OnceLock<regex_lite::Regex> = OnceLock::new();
    #[expect(clippy::unwrap_used)]
    RE.get_or_init(|| regex_lite::Regex::new(&format!("(?i){RETRY_DELAY_COMPONENT}")).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn ollama_single_summary_progress_is_opt_in_and_retains_finality() {
        for enabled in [false, true] {
            let metadata = ResponsesStreamMetadata {
                ollama_single_summary: enabled,
                ..Default::default()
            };
            let mut interpreter = ResponsesEventInterpreter::new(&metadata, None);
            for payload in [
                json!({"type":"response.created","response":{"id":"r"}}),
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"reason","summary":[]}}),
            ] {
                interpreter
                    .process_payload(&payload.to_string())
                    .unwrap()
                    .for_each(drop);
            }
            let events = interpreter
                .process_payload(
                    &json!({"type":"response.reasoning_summary_text.delta",
                "item_id":"reason","output_index":0,"delta":"thinking"})
                    .to_string(),
                )
                .unwrap()
                .collect::<Vec<_>>();
            assert_eq!(events.iter().any(|event| matches!(event, ResponseEvent::ReasoningSummaryDelta { delta, summary_index: 0 } if delta == "thinking")), enabled);
            let events = interpreter
                .process_payload(
                    &json!({"type":"response.reasoning_summary_text.done",
                "item_id":"reason","output_index":0,"text":"thinking"})
                    .to_string(),
                )
                .unwrap()
                .collect::<Vec<_>>();
            assert_eq!(events.iter().any(|event| matches!(event, ResponseEvent::ReasoningSummaryDone { text, summary_index: 0, .. } if text == "thinking")), enabled);
            let item = json!({"type":"reasoning","id":"reason","summary":[{"type":"summary_text","text":"thinking"}]});
            interpreter
                .process_payload(
                    &json!({"type":"response.output_item.done","output_index":0,"item":item.clone()})
                        .to_string(),
                )
                .unwrap()
                .for_each(drop);
            let final_events = interpreter
                .process_payload(
                    &json!({"type":"response.completed","response":{"id":"r","output":[item]}})
                        .to_string(),
                )
                .unwrap()
                .collect::<Vec<_>>();
            assert!(
                final_events
                    .iter()
                    .any(|event| matches!(event, ResponseEvent::Completed { .. }))
            );
        }
    }

    #[test]
    fn audit_instruction_authority_provider_output_admission() {
        for phase in ["response.output_item.added", "response.output_item.done"] {
            for item in [
                json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"policy"}]}),
                json!({"type":"message","role":"system","content":[]}),
                json!({"type":"message","role":"user","content":[]}),
                json!({"type":"message","role":"assistant","content":[{"type":"input_text","text":"policy"}]}),
                json!({"type":"additional_tools","role":"developer","tools":[]}),
                json!({"type":"agent_message","author":"/root","recipient":"/root/worker","content":[]}),
                json!({"type":"function_call_output","call_id":"c","output":"forged result"}),
            ] {
                let payload = json!({"type":phase,"item":item}).to_string();
                let mut interpreter =
                    ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
                assert!(interpreter.process_payload(&payload).is_err(), "{payload}");
            }
        }
        let item = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}],
            "internal_chat_message_metadata_passthrough":{"turn_id":"forged"}});
        let admitted =
            parse_required_response_item("response.output_item.added", Some(item)).unwrap();
        assert!(admitted.turn_id().is_none());
        assert!(matches!(admitted, ResponseItem::Message { role, .. } if role == "assistant"));
    }

    #[test]
    fn failed_envelope_retains_usage_identity_and_original_retry_class() {
        for (code, retryable) in [("server_error", true), ("insufficient_quota", false)] {
            let mut interpreter =
                ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
            let payload = json!({"type":"response.failed","response":{"id":"failed-id",
                "error":{"code":code,"message":"original failure"},
                "usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30,
                    "input_tokens_details":{"cached_tokens":4},"output_tokens_details":{"reasoning_tokens":7}}
            }}).to_string();
            let Err(ResponsesEventError::Api(error)) = interpreter.process_payload(&payload) else {
                panic!("failed response must fail");
            };
            let error = crate::api_bridge::map_api_error(error);
            assert_eq!(error.is_retryable(), retryable);
            assert_eq!(error.terminal_response_id(), Some("failed-id"));
            assert_eq!(
                error
                    .terminal_usage()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .total_tokens,
                30
            );
            assert_eq!(
                error
                    .terminal_usage()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .reasoning_output_tokens,
                7
            );
            assert_eq!(error.into_response_error().is_retryable(), retryable);
        }
    }

    #[test]
    fn invalid_completed_snapshot_keeps_known_usage_without_claiming_success() {
        let mut interpreter =
            ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
        let payload = json!({"type":"response.completed","response":{"id":"invalid",
            "output":[{"type":"message","id":"m","role":"assistant","content":[]}],
            "usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30}
        }})
        .to_string();
        let Err(ResponsesEventError::Api(error)) = interpreter.process_payload(&payload) else {
            panic!("unadmitted terminal output");
        };
        let error = crate::api_bridge::map_api_error(error);
        assert!(!error.is_retryable());
        assert_eq!(error.terminal_response_id(), Some("invalid"));
        assert_eq!(
            error
                .terminal_usage()
                .unwrap()
                .as_ref()
                .unwrap()
                .total_tokens,
            30
        );
    }

    #[test]
    fn executable_done_requires_completed_or_legacy_missing_status() {
        for kind in ["function_call", "custom_tool_call", "tool_search_call"] {
            for status in ["incomplete", "in_progress", "failed", "unknown"] {
                let payload = json!({
                    "type": "response.output_item.done",
                    "item": {"type": kind, "status": status, "name": "marker",
                        "call_id": "call", "arguments": "{}", "input": "marker",
                        "execution": "client"}
                })
                .to_string();
                let mut interpreter =
                    ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
                assert!(
                    matches!(
                        interpreter.process_payload(&payload),
                        Err(ResponsesEventError::Api(_))
                    ),
                    "{kind}: {status}"
                );
            }
        }
        for status in [Value::Null, json!("completed")] {
            let payload = json!({"type": "response.output_item.done", "item": {
                "type": "function_call", "name": "marker", "call_id": "call",
                "arguments": "{}", "status": status
            }})
            .to_string();
            let mut interpreter =
                ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
            assert!(matches!(
                interpreter.process_payload(&payload).unwrap().next(),
                Some(ResponseEvent::OutputItemDone(
                    ResponseItem::FunctionCall { .. }
                ))
            ));
        }
    }

    fn admit_test_item(interpreter: &mut ResponsesEventInterpreter, item: Value) {
        interpreter
            .process_payload(&json!({"type":"response.output_item.added","item":item}).to_string())
            .unwrap()
            .for_each(drop);
    }

    #[test]
    fn function_argument_fragments_are_productive_but_empty_fragments_are_not() {
        for delta in ["", "{\"code\":\"", "partial code"] {
            let payload = json!({"type": "response.function_call_arguments.delta",
                "item_id": "fc_1", "delta": delta})
            .to_string();
            let mut interpreter =
                ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
            admit_test_item(
                &mut interpreter,
                json!({"type":"function_call","id":"fc_1","call_id":"call","name":"marker","arguments":""}),
            );
            let event = interpreter
                .process_payload(&payload)
                .unwrap()
                .next()
                .unwrap();
            assert_eq!(event.advances_model_response(), !delta.is_empty());
            assert!(
                matches!(event, ResponseEvent::ToolCallInputDelta { item_id, delta: actual, .. }
                if item_id == "fc_1" && actual == delta)
            );
        }
    }

    #[test]
    fn transport_audit_initial_model_is_not_reemitted() {
        let metadata = ResponsesStreamMetadata::from_headers(&HeaderMap::from_iter([(
            HeaderName::from_static(OPENAI_MODEL_HEADER),
            HeaderValue::from_static("initial"),
        )]));
        let mut interpreter = ResponsesEventInterpreter::new(&metadata, None);
        assert_eq!(
            interpreter
                .process_payload(
                    r#"{"type":"response.metadata","headers":{"openai-model":"initial"}}"#
                )
                .unwrap()
                .count(),
            0
        );
        let events = interpreter
            .process_payload(r#"{"type":"response.metadata","headers":{"openai-model":"changed"}}"#)
            .unwrap()
            .collect::<Vec<_>>();
        assert!(
            matches!(events.as_slice(), [ResponseEvent::ServerModel(model)] if model == "changed")
        );
    }

    #[test]
    fn transport_audit_item_fields_and_completion_moves_identity() {
        let payload = r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"test","call_id":"call","arguments":"{\"text\":\"hello\"}"}}"#;
        let event: ResponsesStreamEvent<'_> = serde_json::from_str(payload).unwrap();
        assert!(matches!(process_responses_event(event, None).unwrap(),
            Some(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { name, arguments, .. }))
            if name == "test" && arguments == r#"{"text":"hello"}"#));

        let event: ResponsesStreamEvent<'_> = serde_json::from_str(
            r#"{"type":"response.completed","response":{"id":"owned-identity","end_turn":true}}"#,
        )
        .unwrap();
        let id_allocation = event
            .response
            .as_ref()
            .unwrap()
            .id
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap()
            .as_ptr();
        let Some(ResponseEvent::Completed {
            response_id,
            end_turn,
            ..
        }) = process_responses_event(event, None).unwrap()
        else {
            panic!("completion");
        };
        assert_eq!(response_id.as_ptr(), id_allocation);
        assert_eq!(end_turn, Some(true));
    }

    #[test]
    fn tool_input_delta_moves_owned_item_id() {
        for call_id in [None, Some("call_1")] {
            let payload = json!({
                "type": "response.custom_tool_call_input.delta",
                "item_id": "ctc_1",
                "call_id": call_id,
                "delta": "*** Begin",
            })
            .to_string();
            let event: ResponsesStreamEvent<'_> = serde_json::from_str(&payload).unwrap();
            let item_allocation = event.item_id.as_ref().unwrap().as_ptr();
            let call_allocation = event.call_id.as_ref().map(|id| id.as_ptr());

            let Some(ResponseEvent::ToolCallInputDelta {
                item_id,
                call_id: actual_call_id,
                delta,
            }) = process_responses_event(event, None).unwrap()
            else {
                panic!("expected tool input delta");
            };
            assert_eq!(item_id, "ctc_1");
            assert_eq!(item_id.as_ptr(), item_allocation);
            assert_eq!(actual_call_id.as_deref(), call_id);
            assert_eq!(
                actual_call_id.as_ref().map(|id| id.as_ptr()),
                call_allocation
            );
            assert_eq!(delta, "*** Begin");
        }
    }

    #[test]
    fn tool_input_delta_preserves_fallback_and_missing_fields() {
        for (item_id, call_id, delta) in [
            (Some("ctc_1"), Some("call_1"), Some("input")),
            (Some("ctc_1"), None, Some("input")),
            (None, Some("call_1"), Some("input")),
            (Some(""), Some("call_1"), Some("")),
            (None, None, Some("input")),
            (Some("ctc_1"), Some("call_1"), None),
        ] {
            let payload = json!({
                "type": "response.custom_tool_call_input.delta",
                "item_id": item_id,
                "call_id": call_id,
                "delta": delta,
            })
            .to_string();
            let mut interpreter =
                ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
            admit_test_item(
                &mut interpreter,
                json!({"type":"custom_tool_call","id":item_id.unwrap_or("fixture-item"),"call_id":call_id.unwrap_or("fixture-call"),"name":"marker","input":""}),
            );
            let events = interpreter
                .process_payload(&payload)
                .unwrap()
                .collect::<Vec<_>>();
            if let (Some(expected_id), Some(expected_delta)) = (item_id.or(call_id), delta) {
                assert!(
                    matches!(events.as_slice(), [ResponseEvent::ToolCallInputDelta {
                    item_id, call_id: actual_call_id, delta,
                }] if item_id == expected_id
                    && actual_call_id.as_deref() == call_id
                    && delta == expected_delta)
                );
            } else {
                assert!(events.is_empty());
            }
        }
    }

    #[test]
    fn audit_optional_usage_does_not_destroy_completion() {
        for usage in [
            Value::Null,
            json!({}),
            json!({"input_tokens": "bad"}),
            json!({"input_tokens": 1, "output_tokens": 2, "total_tokens": 3, "output_tokens_details": "bad"}),
        ] {
            let mut interpreter =
                ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
            let payload = json!({"type":"response.completed","response":{"id":"done","end_turn":true,"usage":usage,
                "output":[]}}).to_string();
            let events = interpreter
                .process_payload(&payload)
                .unwrap()
                .collect::<Vec<_>>();
            let [
                ResponseEvent::Completed {
                    response_id,
                    token_usage,
                    end_turn,
                },
            ] = events.as_slice()
            else {
                panic!("optional usage must not destroy completion")
            };
            assert_eq!(response_id, "done");
            assert_eq!(*end_turn, Some(true));
            if usage.is_null() {
                assert!(token_usage.is_none());
            } else {
                let tokens = token_usage
                    .as_ref()
                    .expect("retain independently valid components");
                assert_eq!(tokens.known_cached_input(), None);
                assert_eq!(tokens.known_reasoning_output(), None);
                if usage["input_tokens"] == 1 {
                    assert_eq!(tokens.known_input(), Some(1));
                    assert_eq!(tokens.known_output(), Some(2));
                    assert_eq!(tokens.known_total(), Some(3));
                } else {
                    assert_eq!(tokens.known_input(), None);
                    assert_eq!(tokens.known_total(), None);
                }
            }
        }
        // Control: a well-formed usage object is still reported, including its details.
        let mut interpreter =
            ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
        let payload = json!({"type":"response.completed","response":{"id":"done","usage":{
            "input_tokens": 10, "input_tokens_details": {"cached_tokens": 4},
            "output_tokens": 20, "output_tokens_details": {"reasoning_tokens": 7},
            "total_tokens": 30}}})
        .to_string();
        let events = interpreter
            .process_payload(&payload)
            .unwrap()
            .collect::<Vec<_>>();
        let expected_usage = TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 4,
            output_tokens: 20,
            reasoning_output_tokens: 7,
            total_tokens: 30,
            coverage: Some(codex_protocol::protocol::TokenUsageCoverage::fully_reported()),
        };
        assert!(matches!(events.as_slice(), [ResponseEvent::Completed {
            response_id, token_usage: Some(usage), end_turn: None }]
            if response_id == "done" && *usage == expected_usage));
        for response in [
            json!({"end_turn":true}),
            json!({"id":"done","end_turn":"true"}),
        ] {
            let mut interpreter =
                ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
            let payload = json!({"type":"response.completed","response":response}).to_string();
            // Required fields stay strict; the rejection is a non-replayable protocol error.
            let Err(ResponsesEventError::Api(ApiError::ProviderFailure { code, message })) =
                interpreter.process_payload(&payload)
            else {
                panic!("completion without a valid id and end_turn must be rejected");
            };
            assert_eq!(code.as_deref(), Some("invalid_response"));
            assert!(
                message.starts_with("failed to parse ResponseCompleted"),
                "{message}"
            );
        }
    }

    #[test]
    fn audit_unknown_events_tolerate_colliding_fields_but_known_events_are_strict() {
        let mut interpreter =
            ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
        assert_eq!(
            interpreter
                .process_payload(r#"{"type":"future.event","delta":{},"summary_index":"new"}"#)
                .unwrap()
                .count(),
            0
        );
        assert!(matches!(
            interpreter.process_payload(r#"{"type":"response.output_text.delta","delta":{}}"#),
            Err(ResponsesEventError::Parse(_))
        ));
    }

    #[test]
    fn audit_reasoning_header_requires_affirmative_value() {
        for (value, expected) in [
            ("true", true),
            (" TRUE ", true),
            ("false", false),
            ("", false),
            ("1", false),
            ("invalid", false),
        ] {
            let headers = HeaderMap::from_iter([(
                HeaderName::from_static(X_REASONING_INCLUDED_HEADER),
                value.parse().unwrap(),
            )]);
            let metadata = ResponsesStreamMetadata::from_headers(&headers);
            assert_eq!(metadata.reasoning_included(), expected);
            assert_eq!(
                metadata
                    .initial_events()
                    .iter()
                    .any(|event| matches!(event, ResponseEvent::ServerReasoningIncluded(true))),
                expected
            );
        }
        let headers = HeaderMap::from_iter([(
            HeaderName::from_static(X_REASONING_INCLUDED_HEADER),
            HeaderValue::from_bytes(&[0xff]).unwrap(),
        )]);
        assert!(!ResponsesStreamMetadata::from_headers(&headers).reasoning_included());
        assert!(!ResponsesStreamMetadata::default().reasoning_included());
    }

    #[test]
    fn audit_error_envelopes_share_classification_and_never_disappear() {
        for code in [
            "invalid_image",
            "invalid_base64_image",
            "insufficient_quota",
            "context_length_exceeded",
            "server_error",
            "cyber_policy",
            "future_failure",
        ] {
            let error =
                json!({"code":code,"message":"same message", "resets_at":{},"plan_type":[]});
            for envelope in [
                json!({"type":"error", "code":code,"message":"same message"}),
                json!({"type":"error","error":error}),
                json!({"type":"response.failed","response":{"error":error}}),
            ] {
                let mut interpreter =
                    ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
                let Err(ResponsesEventError::Api(error)) =
                    interpreter.process_payload(&envelope.to_string())
                else {
                    panic!("error must be delivered");
                };
                let mapped = crate::api_bridge::map_api_error(error);
                assert_eq!(
                    mapped.is_retryable(),
                    code == "server_error",
                    "{code}: {mapped:?}"
                );
                use codex_protocol::error::CodexErr;
                match (code, mapped) {
                    (
                        "invalid_image" | "invalid_base64_image",
                        CodexErr::InvalidRequest(message),
                    )
                    | ("cyber_policy", CodexErr::CyberPolicy { message })
                    | ("server_error", CodexErr::Stream(message, None)) => {
                        assert_eq!(message, "same message");
                    }
                    ("insufficient_quota", CodexErr::QuotaExceeded)
                    | ("context_length_exceeded", CodexErr::ContextWindowExceeded) => {}
                    ("future_failure", CodexErr::ProviderFailure { code, message }) => {
                        assert_eq!(code.as_deref(), Some("future_failure"));
                        assert_eq!(message, "same message");
                    }
                    (code, other) => panic!("wrong error identity for {code}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn audit_incomplete_preserves_usage_and_identity_through_bridge() {
        let mut interpreter =
            ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
        let payload = json!({"type":"response.incomplete","response":{"id":"partial", "incomplete_details":{"reason":"max_output_tokens"},
            "usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30}}}).to_string();
        let Err(ResponsesEventError::Api(error)) = interpreter.process_payload(&payload) else {
            panic!("incomplete outcome");
        };
        let mapped = crate::api_bridge::map_api_error(error);
        assert!(!mapped.is_retryable());
        let codex_protocol::error::CodexErr::IncompleteResponse(response) = mapped else {
            panic!("typed outcome");
        };
        assert_eq!(response.response_id.as_deref(), Some("partial"));
        assert_eq!(response.reason, "max_output_tokens");
        assert_eq!(response.token_usage.unwrap().total_tokens, 30);
    }

    #[test]
    fn audit_required_item_diagnostics_do_not_copy_private_payload() {
        let mut interpreter =
            ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
        let secret = "private_payload".repeat(100_000);
        let payload = json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":secret}}).to_string();
        let Err(ResponsesEventError::Api(error)) = interpreter.process_payload(&payload) else {
            panic!("invalid item");
        };
        let message = error.to_string();
        assert!(message.len() < 512);
        assert!(!message.contains("private_payload"));
        assert!(message.contains("Data"));
    }

    #[test]
    fn audit_incomplete_is_terminal_and_preserves_reason() {
        for (reason, retryable) in [
            ("content_filter", false),
            ("max_output_tokens", false),
            ("unknown", false),
        ] {
            let mut interpreter =
                ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
            let payload = json!({"type": "response.incomplete", "response": {
                "incomplete_details": {"reason": reason}
            }})
            .to_string();
            let error = match interpreter.process_payload(&payload) {
                Err(ResponsesEventError::Api(error)) => error,
                _ => panic!("expected incomplete response error"),
            };
            let error = crate::api_bridge::map_api_error(error);
            assert_eq!(error.is_retryable(), retryable);
            assert!(error.to_string().contains(reason));
        }
    }

    #[test]
    fn ordinary_events_skip_transport_error_parsing() {
        let mut interpreter =
            ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
        admit_test_item(
            &mut interpreter,
            json!({"type":"message","id":"m","role":"assistant","content":[]}),
        );
        for payload in [
            r#"{"type":"response.output_text.delta","delta":"hello"}"#,
            r#"{"delta":"hello","type":"response.output_text.\u0064elta"}"#,
            r#"{"type":"response.output_text.delta","delta":"hello","obfuscation":"ignored","sequence_number":7}"#,
        ] {
            let events = interpreter
                .process_payload_with_error_mapper(payload, || {
                    panic!("ordinary events must not invoke the transport error parser")
                })
                .expect("valid delta")
                .collect::<Vec<_>>();
            assert!(matches!(
                events.as_slice(),
                [ResponseEvent::OutputTextDelta(text)] if text == "hello"
            ));
        }
    }

    #[test]
    fn metadata_parses_all_shared_headers_in_initial_event_order() {
        let mut headers = HeaderMap::new();
        headers.insert(
            OPENAI_MODEL_HEADER,
            HeaderValue::from_static("server-model"),
        );
        headers.insert(X_MODELS_ETAG_HEADER, HeaderValue::from_static("etag-1"));
        headers.insert(
            X_REASONING_INCLUDED_HEADER,
            HeaderValue::from_static("true"),
        );
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("req-1"));
        headers.insert(
            X_CODEX_TURN_STATE_HEADER,
            HeaderValue::from_static("turn-1"),
        );

        let metadata = ResponsesStreamMetadata::from_headers(&headers);
        let turn_state = OnceLock::new();
        metadata.apply_turn_state(Some(&turn_state));
        assert_eq!(metadata.upstream_request_id(), Some("req-1"));
        assert_eq!(turn_state.get().map(String::as_str), Some("turn-1"));

        let events = metadata.initial_events();
        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], ResponseEvent::ServerModel(model) if model == "server-model"));
        assert!(matches!(&events[1], ResponseEvent::RateLimits(_)));
        assert!(matches!(&events[2], ResponseEvent::ModelsEtag(etag) if etag == "etag-1"));
        assert!(matches!(
            &events[3],
            ResponseEvent::ServerReasoningIncluded(true)
        ));
    }

    #[test]
    fn interpreter_applies_metadata_rate_limits_and_safety_equally_for_all_transports() {
        let turn_state = Arc::new(OnceLock::new());
        let mut interpreter = ResponsesEventInterpreter::new(
            &ResponsesStreamMetadata::default(),
            Some(Arc::clone(&turn_state)),
        );
        admit_test_item(
            &mut interpreter,
            json!({"type":"message","id":"m","role":"assistant","content":[]}),
        );

        let metadata_events = interpreter
            .process_payload(
                &json!({
                    "type": "response.metadata",
                    "headers": {
                        "openai-model": "routed-model",
                        "x-codex-turn-state": "sticky-1",
                        "x-codex-safety-buffering-faster-model": "fast-model"
                    },
                    "metadata": {
                        "openai_verification_recommendation": ["trusted_access_for_cyber"]
                    }
                })
                .to_string(),
            )
            .expect("metadata should be interpreted")
            .collect::<Vec<_>>();
        assert_eq!(turn_state.get().map(String::as_str), Some("sticky-1"));
        assert!(
            matches!(&metadata_events[0], ResponseEvent::ServerModel(model) if model == "routed-model")
        );
        assert!(
            matches!(&metadata_events[1], ResponseEvent::ModelVerifications(values) if values == &[ModelVerification::TrustedAccessForCyber])
        );

        let safety_events = interpreter
            .process_payload(
                r#"{"type":"response.output_text.delta","delta":"x","safety_buffering":{"use_cases":["cyber"],"reasons":[]}}"#,
            )
            .expect("safety event should be interpreted")
            .collect::<Vec<_>>();
        assert!(
            matches!(&safety_events[0], ResponseEvent::SafetyBuffering(buffering) if buffering.faster_model.as_deref() == Some("fast-model"))
        );
        assert!(matches!(&safety_events[1], ResponseEvent::OutputTextDelta(delta) if delta == "x"));

        let rate_limit_events = interpreter
            .process_payload(
                r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":12.5,"window_minutes":60,"reset_at":42}}}"#,
            )
            .expect("rate limits should be interpreted")
            .collect::<Vec<_>>();
        assert!(
            matches!(&rate_limit_events[0], ResponseEvent::RateLimits(snapshot) if snapshot.primary.as_ref().is_some_and(|window| window.used_percent == 12.5))
        );
    }

    #[test]
    fn interpreter_reuses_event_storage_and_does_not_replay_dropped_or_failed_frames() {
        let mut interpreter =
            ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
        admit_test_item(
            &mut interpreter,
            json!({"type":"message","id":"m","role":"assistant","content":[]}),
        );
        let frame = r#"{"type":"response.output_text.delta","delta":"first","headers":{"openai-model":["model-a"]}}"#;
        let mut events = interpreter.process_payload(frame).unwrap();
        assert!(
            matches!(events.next(), Some(ResponseEvent::ServerModel(model)) if model == "model-a")
        );
        // Dropping a partially consumed frame must discard its remaining delta.
        drop(events);
        let allocation = interpreter.events.as_ptr();

        for delta in ["second", "third"] {
            let payload = json!({"type":"response.output_text.delta", "delta":delta,
                "headers":{"X-OpenAI-Model":"model-a"}})
            .to_string();
            let mut events = interpreter.process_payload(&payload).unwrap();
            assert!(
                matches!(events.next(), Some(ResponseEvent::OutputTextDelta(text)) if text == delta)
            );
            assert!(
                events.next().is_none(),
                "unchanged model must not be re-emitted"
            );
            drop(events);
            assert_eq!(interpreter.events.as_ptr(), allocation);
        }

        assert!(
            interpreter
                .process_payload(
                    r#"{"type":"response.output_item.done","headers":{"openai-model":"model-b"}}"#
                )
                .is_err()
        );
        let mut events = interpreter
            .process_payload(r#"{"type":"response.output_text.delta","delta":"after error"}"#)
            .unwrap();
        assert!(
            matches!(events.next(), Some(ResponseEvent::OutputTextDelta(text)) if text == "after error")
        );
        assert!(
            events.next().is_none(),
            "failed frame metadata must not leak into the next frame"
        );
    }

    #[test]
    fn known_output_item_events_require_valid_items() {
        for event_kind in ["response.output_item.added", "response.output_item.done"] {
            for (case, item) in [
                ("missing", None),
                ("invalid", Some(json!({"type": "message"}))),
            ] {
                let mut payload = json!({"type": event_kind});
                if let Some(item) = item {
                    payload["item"] = item;
                }
                let mut interpreter = ResponsesEventInterpreter::new(
                    &ResponsesStreamMetadata::default(),
                    /*turn_state*/ None,
                );

                let error = interpreter
                    .process_payload(&payload.to_string())
                    .expect_err("known output-item event should reject an unusable item");

                match error {
                    ResponsesEventError::Api(ApiError::ProviderFailure { code, message }) => {
                        assert_eq!(code.as_deref(), Some("invalid_response"));
                        assert!(message.contains(event_kind), "case {case}: {message}");
                    }
                    other => panic!("unexpected error for {event_kind} {case}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn deserialized_rate_limit_event_supplies_snapshot_without_reparse() {
        let event: RateLimitStreamEvent = serde_json::from_str(
            r#"{"type":"codex.rate_limits","metered_limit_name":"codex-fast","rate_limits":{"primary":{"used_percent":25.0}}}"#,
        )
        .expect("rate-limit event should deserialize once");

        let snapshot = rate_limit_snapshot_from_event(event.rate_limit)
            .expect("deserialized rate-limit fields should convert");

        assert_eq!(snapshot.limit_id.as_deref(), Some("codex_fast"));
        assert_eq!(
            snapshot.primary.map(|window| window.used_percent),
            Some(25.0)
        );
    }

    #[test]
    fn interpreter_preserves_rate_limit_error_with_overflowing_retry_delay() {
        for delay in [
            "18446744073709551616 seconds".to_string(),
            format!("{} seconds", "9".repeat(400)),
            format!("{}ms", "9".repeat(400)),
        ] {
            let message = format!("Please try again in {delay}.");
            let payload = json!({
                "type": "response.failed",
                "response": { "error": {
                    "code": "rate_limit_exceeded",
                    "message": message,
                } },
            });
            let mut interpreter =
                ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);

            let error = interpreter
                .process_payload(&payload.to_string())
                .unwrap_err();

            assert!(
                matches!(error, ResponsesEventError::Api(ApiError::Retryable {
                delay: None,
                message: actual,
            }) if actual == message)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn parses_retry_after_units() {
        for (message, expected) in [
            ("Please try again in 28ms.", Duration::from_millis(28)),
            ("Please try again in 1.5ms.", Duration::from_micros(1_500)),
            ("Please try again in 1.898s.", Duration::from_millis(1_898)),
            ("Try again in 35 seconds.", Duration::from_secs(35)),
        ] {
            let payload = json!({
                "type": "response.failed",
                "response": { "error": {
                    "code": "rate_limit_exceeded",
                    "message": message,
                } },
            });
            let mut interpreter =
                ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
            let error = interpreter
                .process_payload(&payload.to_string())
                .unwrap_err();
            assert!(
                matches!(error, ResponsesEventError::Api(ApiError::Retryable {
                delay: Some(actual_delay),
                message: actual_message,
            }) if actual_delay.remaining_delay() == expected && actual_message == message)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn parses_compound_retry_after_durations() {
        let failed = |code: &str, message: &str| {
            json!({
                "type": "response.failed",
                "response": { "error": { "code": code, "message": message } },
            })
            .to_string()
        };
        for (message, expected) in [
            ("Please try again in 1m30s.", Some(Duration::from_secs(90))),
            ("Please try again in 6m0s.", Some(Duration::from_secs(360))),
            (
                "Please try again in 1h2m3.5s.",
                Some(Duration::from_millis(3_723_500)),
            ),
            (
                "Please try again in 20m10.4s.",
                Some(Duration::from_millis(1_210_400)),
            ),
            ("Try again in 2 minutes.", Some(Duration::from_secs(120))),
            // Unknown units and unit-like words keep the generic backoff.
            ("Please try again in 3 days.", None),
            ("Please try again in 5 moments.", None),
        ] {
            let mut interpreter =
                ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
            let error = interpreter
                .process_payload(&failed("rate_limit_exceeded", message))
                .unwrap_err();
            let ResponsesEventError::Api(ApiError::Retryable { delay, .. }) = error else {
                panic!("expected a retryable rate limit for {message}");
            };
            assert_eq!(
                delay.map(|delay| delay.remaining_delay()),
                expected,
                "{message}"
            );
        }

        // Only rate limits carry parseable advice.
        let mut interpreter =
            ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
        let error = interpreter
            .process_payload(&failed("server_error", "Please try again in 1m30s."))
            .unwrap_err();
        assert!(matches!(
            error,
            ResponsesEventError::Api(ApiError::Retryable { delay: None, .. })
        ));
    }
}
#[cfg(test)]
mod component_receipt_regressions {
    use super::*;

    #[test]
    fn cache_details_survive_every_terminal_shape_without_inventing_zero() {
        for (details, cached, write, invalid) in [
            (None, None, None, false),
            (Some(serde_json::Value::Null), None, None, false),
            (
                Some(serde_json::json!({"cached_tokens": 0})),
                Some(0),
                None,
                false,
            ),
            (
                Some(serde_json::json!({"cached_tokens": 40, "cache_write_tokens": 0})),
                Some(40),
                Some(0),
                false,
            ),
            (
                Some(serde_json::json!({"cached_tokens": 40, "cache_write_tokens": 30})),
                Some(40),
                Some(30),
                false,
            ),
            (
                Some(serde_json::json!({"cached_tokens": null, "cache_write_tokens": "bad"})),
                None,
                None,
                true,
            ),
            (
                Some(serde_json::json!({"cached_tokens": 40, "cache_write_tokens": -1})),
                Some(40),
                None,
                true,
            ),
            (
                Some(serde_json::json!({"cached_tokens": 80, "cache_write_tokens": 30})),
                Some(80),
                Some(30),
                true,
            ),
        ] {
            for terminal in [
                "response.completed",
                "response.failed",
                "response.incomplete",
            ] {
                let mut usage =
                    serde_json::json!({"input_tokens":100,"output_tokens":20,"total_tokens":120});
                if let Some(details) = &details {
                    usage["input_tokens_details"] = details.clone();
                }
                let payload = serde_json::json!({"type": terminal, "response": {
                    "id":"receipt", "usage":usage,
                    "error":{"code":"server_error","message":"failed"},
                    "incomplete_details":{"reason":"max_output_tokens"}
                }})
                .to_string();
                let mut interpreter =
                    ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
                let usage = match interpreter.process_payload(&payload) {
                    Ok(mut events) => events
                        .find_map(|event| match event {
                            ResponseEvent::Completed { token_usage, .. } => token_usage,
                            _ => None,
                        })
                        .expect("completion receipt"),
                    Err(ResponsesEventError::Api(error)) => crate::api_bridge::map_api_error(error)
                        .terminal_usage()
                        .expect("terminal observation")
                        .clone()
                        .expect("receipt"),
                    Err(error) => panic!("unexpected terminal error: {error:?}"),
                };
                assert_eq!(usage.known_input(), Some(100));
                assert_eq!(usage.known_cached_input(), cached);
                assert_eq!(usage.known_cache_write_input(), write);
                assert_eq!(usage.coverage.as_ref().unwrap().reported_receipts, Some(1));
                assert_eq!(
                    usage.coverage.as_ref().unwrap().invalid_receipts,
                    u64::from(invalid)
                );
                let ordinary = if invalid {
                    None
                } else {
                    cached.zip(write).map(|(read, write)| 100 - read - write)
                };
                assert_eq!(usage.known_ordinary_input(), ordinary);
                let mut subtotal = TokenUsage::empty_ledger();
                subtotal.add_assign(&usage);
                subtotal.add_assign(&TokenUsage::missing_receipt());
                assert_eq!(subtotal.known_input(), None);
                assert_eq!(subtotal.coverage.as_ref().unwrap().receipts, 2);
                assert_eq!(
                    subtotal.coverage.as_ref().unwrap().reported_receipts,
                    Some(1)
                );
                assert_eq!(subtotal.known_cache_write_input(), None);
            }
        }
    }

    #[test]
    fn invalid_signed_component_does_not_reduce_or_erase_independent_usage() {
        let usage = parse_usage(Some(serde_json::json!({
            "input_tokens": -5, "output_tokens": 7, "total_tokens": 2,
            "input_tokens_details":{"cached_tokens":0}
        })))
        .unwrap();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.known_input(), None);
        assert_eq!(usage.known_cached_input(), Some(0));
        assert_eq!(usage.known_output(), Some(7));
        assert_eq!(usage.known_reasoning_output(), None);
        assert_eq!(usage.known_visible_output(), None);
        assert_eq!(usage.known_total(), Some(2));
        assert_eq!(usage.coverage.unwrap().invalid_receipts, 1);
        let mut ledger = TokenUsage::empty_ledger();
        ledger.add_assign(&usage);
        assert_eq!(ledger.known_total(), Some(2));
    }
}
#[cfg(test)]
mod harness_usage_regressions {
    use super::*;

    #[test]
    fn invalid_relationships_cannot_be_laundered_by_later_valid_receipts() {
        let invalid = parse_usage(Some(serde_json::json!({
            "input_tokens":10,"input_tokens_details":{"cached_tokens":20},
            "output_tokens":2,"output_tokens_details":{"reasoning_tokens":5},"total_tokens":12
        }))).unwrap();
        let valid = parse_usage(Some(serde_json::json!({
            "input_tokens":100,"input_tokens_details":{"cached_tokens":0},
            "output_tokens":10,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":110
        }))).unwrap();
        let mut ledger = TokenUsage::empty_ledger();
        ledger.add_assign(&invalid);
        ledger.add_assign(&valid);
        assert_eq!(ledger.known_input(), Some(110));
        assert_eq!(ledger.known_output(), Some(12));
        assert_eq!(ledger.coverage.unwrap().invalid_receipts, 1);
        assert_eq!(ledger.known_non_cached_input(), None);
        assert_eq!(ledger.known_visible_output(), None);
    }

    #[test]
    fn malformed_whole_usage_remains_an_invalid_reported_receipt() {
        for value in [serde_json::json!(42), serde_json::json!("bad"), serde_json::json!([])] {
            let usage = parse_usage(Some(value)).unwrap();
            let coverage = usage.coverage.unwrap();
            assert_eq!(coverage.receipts, 1);
            assert_eq!(coverage.reported_receipts, Some(1));
            assert_eq!(coverage.invalid_receipts, 1);
            assert_eq!(usage.known_total(), None);
        }
        assert_eq!(parse_usage(Some(Value::Null)), None);
        assert_eq!(parse_usage(None), None);
    }

    #[test]
    fn normal_completion_interprets_usage_once_without_losing_usage() {
        let mut interpreter = ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
        USAGE_PAYLOAD_PARSE_COUNT.with(|count| count.set(0));
        let events = interpreter.process_payload(r#"{"type":"response.completed","response":{"id":"response-1","usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}}"#).unwrap().collect::<Vec<_>>();
        assert!(matches!(events.as_slice(), [ResponseEvent::Completed { token_usage: Some(usage), .. }] if usage.known_total() == Some(12)));
        USAGE_PAYLOAD_PARSE_COUNT.with(|count| assert_eq!(count.get(), 1));
    }
}
