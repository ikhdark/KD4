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

        // Stage metadata until the entire frame is admitted. A rejected frame
        // must not poison caller-owned OnceLock state or model/treatment caches.
        let response_turn_state = event.turn_state();
        let updated_treatment = event.headers.as_ref().and_then(Value::as_object).and_then(|headers| {
            treatment_from_headers(&json_headers_to_http_headers(headers.iter().filter(|(name, _)| {
                name.eq_ignore_ascii_case(X_CODEX_SAFETY_BUFFERING_ENABLED_HEADER)
                    || name.eq_ignore_ascii_case(X_CODEX_SAFETY_BUFFERING_FASTER_MODEL_HEADER)
            })))
        });
        let updated_model = event
            .response_model()
            .filter(|model| self.last_server_model.as_deref() != Some(*model))
            .map(str::to_owned);
        // Failed/incomplete envelopes are admissible observations even though
        // conversion returns an error. Foreign terminal frames are not.
        let terminal_failure =
            matches!(event.kind.as_ref(), "response.failed" | "response.incomplete");
        let mut ownership_admitted = false;

        if let Some(response) = event.response.as_ref()
            && let Some(reported_model) = response.model.as_ref().and_then(Value::as_str)
        {
            let header_model = response.headers.as_ref().and_then(header_openai_model_value_from_json)
                .or_else(|| event.headers.as_ref().and_then(header_openai_model_value_from_json));
            tracing::info!(event.name = "codex.provider_reported_model",
                response_id = response.id.as_ref().and_then(|value| value.as_str()).unwrap_or_default(),
                reported_service_tier = response.service_tier.as_ref().and_then(|value| value.as_str())
                    .filter(|tier| tier.len() <= 64 && tier.bytes().all(|byte|
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))),
                reported_model, provenance = "response.model", header_model,
                "provider-reported identity; not an inferred reroute reason");
        }
        if let Some(model) = &updated_model {
            self.events
                .push(ResponseEvent::ServerModel(model.clone()));
        }
        if let Some(verifications) = event.model_verifications() {
            self.events
                .push(ResponseEvent::ModelVerifications(verifications));
        }
        if let Some(metadata) = event.turn_moderation_metadata() {
            self.events
                .push(ResponseEvent::TurnModerationMetadata(metadata));
        }
        if let Some(buffering) = event.safety_buffering(
            updated_treatment.as_ref().unwrap_or(&self.safety_buffering_treatment),
        ) {
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
            .process(event, payload.len(), completion_usage, &mut ownership_admitted)
            .map_err(|error| match (error, rejected_completion_usage) {
                (ResponsesEventError::Api(source), Some((response_id, usage))) => {
                    ResponsesEventError::Api(ApiError::TerminalResponse {
                        source: Box::new(source),
                        response_id,
                        token_usage: Some(usage),
                    })
                }
                (error, _) => error,
            });
        if interpreted.is_ok() || (terminal_failure && ownership_admitted) {
            if let Some(response_turn_state) = response_turn_state
                && let Some(turn_state) = self.turn_state.as_deref()
            {
                let _ = turn_state.set(response_turn_state);
            }
            if let Some(treatment) = updated_treatment {
                self.safety_buffering_treatment = treatment;
            }
            if let Some(model) = updated_model {
                self.last_server_model = Some(model);
            }
        }
        let interpreted = interpreted?;
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
    service_tier: Option<Value>,
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
            | "response.refusal.delta"
            | "response.refusal.done"
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


fn parse_usage(value: Option<Value>) -> Option<TokenUsage> {
    use codex_protocol::protocol::TokenUsageCoverage;
    let value = value?;
    if value.is_null() {
        return None;
    }
    let Some(object) = value.as_object() else {
        return Some(TokenUsage {
            coverage: Some(TokenUsageCoverage {
                receipts: 1,
                invalid_receipts: 1,
                reported_receipts: Some(1),
                ..Default::default()
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
    invalid |= input
        .zip(cache_write)
        .is_some_and(|(input, write)| write > input);
    invalid |= output
        .zip(reasoning)
        .is_some_and(|(output, reasoning)| reasoning > output);
    invalid |= input
        .zip(cached)
        .zip(cache_write)
        .is_some_and(|((input, cached), write)| {
            cached.checked_add(write).is_none_or(|sum| sum > input)
        });
    invalid |= total.is_some_and(|total| {
        [input, output, cached, cache_write, reasoning]
            .into_iter()
            .flatten()
            .any(|component| component > total)
            || input.zip(output).is_some_and(|(input, output)| {
                input.checked_add(output).is_none_or(|sum| sum != total)
            })
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
/// adapters keep their status-based fallback when both code and type are unknown.
pub(crate) fn classify_provider_error(value: &Value) -> Option<ApiError> {
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // A provider-specific code can refine a stable type without being known to
    // this client. Prefer recognized codes, but do not let an unknown one hide
    // a recognized type (for example server_error).
    [value.get("code"), value.get("type")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(|code| {
            Some(match code {
                "context_length_exceeded" => ApiError::ContextWindowExceeded,
                "insufficient_quota" | "quota_exceeded" | "billing_hard_limit_reached" => {
                    ApiError::QuotaExceeded
                }
                "usage_not_included" => ApiError::UsageNotIncluded,
                "cyber_policy" => ApiError::CyberPolicy {
                    message: cyber_policy_message(Some(message.to_owned())),
                },
                "server_is_overloaded" | "slow_down" => {
                    ApiError::ServerOverloaded { retry_after: None }
                }
                "server_error" | "internal_server_error" | "rate_limit_exceeded" => {
                    let delay = try_parse_retry_after(&Error {
                        code: Some(code.to_owned()),
                        message: Some(message.to_owned()),
                    });
                    ApiError::Retryable {
                        message: message.to_owned(),
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
                | "image_file_not_found" => ApiError::InvalidRequest {
                    message: message.to_owned(),
                },
                _ => return None,
            })
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
        "response.output_text.delta" | "response.refusal.delta" => {
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
                    matches!(
                        part,
                        codex_protocol::models::ContentItem::OutputText { .. }
                            | codex_protocol::models::ContentItem::Refusal { .. }
                    )
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


