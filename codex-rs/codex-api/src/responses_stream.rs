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

/// Transport-independent metadata returned when a Responses stream is established.
#[derive(Debug, Clone)]
pub(crate) struct ResponsesStreamMetadata {
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
    last_server_model: Option<String>,
    safety_buffering_treatment: SafetyBufferingTreatment,
    turn_state: Option<Arc<OnceLock<String>>>,
    events: Vec<ResponseEvent>,
}

impl ResponsesEventInterpreter {
    pub(crate) fn new(
        metadata: &ResponsesStreamMetadata,
        turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Self {
        Self {
            last_server_model: None,
            safety_buffering_treatment: metadata.safety_buffering_treatment.clone(),
            turn_state,
            events: Vec::new(),
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

        let event = event?;

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
        if let Some(event) = process_responses_event(event)? {
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

#[derive(Debug, Deserialize)]
struct ResponseCompleted {
    id: String,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default)]
    end_turn: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ResponseCompletedUsage {
    input_tokens: i64,
    input_tokens_details: Option<ResponseCompletedInputTokensDetails>,
    output_tokens: i64,
    output_tokens_details: Option<ResponseCompletedOutputTokensDetails>,
    total_tokens: i64,
}

impl From<ResponseCompletedUsage> for TokenUsage {
    fn from(value: ResponseCompletedUsage) -> Self {
        Self {
            input_tokens: value.input_tokens,
            cached_input_tokens: value
                .input_tokens_details
                .map(|details| details.cached_tokens)
                .unwrap_or(0),
            output_tokens: value.output_tokens,
            reasoning_output_tokens: value
                .output_tokens_details
                .map(|details| details.reasoning_tokens)
                .unwrap_or(0),
            total_tokens: value.total_tokens,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResponseCompletedInputTokensDetails {
    cached_tokens: i64,
}

#[derive(Debug, Deserialize)]
struct ResponseCompletedOutputTokensDetails {
    reasoning_tokens: i64,
}

#[derive(Deserialize, Debug)]
pub(crate) struct ResponsesStreamEvent<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    headers: Option<Value>,
    metadata: Option<Value>,
    response: Option<ResponseEnvelope>,
    item: Option<Value>,
    item_id: Option<String>,
    call_id: Option<String>,
    delta: Option<String>,
    text: Option<String>,
    summary_index: Option<i64>,
    content_index: Option<i64>,
    safety_buffering: Option<Value>,
}

// Ignore full output snapshots: items have their own ordered stream events.
#[derive(Debug, Deserialize)]
struct ResponseEnvelope {
    id: Option<Value>,
    usage: Option<Value>,
    end_turn: Option<Value>,
    error: Option<Value>,
    incomplete_details: Option<Value>,
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
    let value = value.filter(|value| !value.is_null())?;
    match serde_json::from_value::<ResponseCompletedUsage>(value) {
        Ok(usage) => Some(usage.into()),
        Err(error) => {
            debug!(category = ?error.classify(), line = error.line(), column = error.column(),
                "response usage unavailable");
            None
        }
    }
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
        "server_is_overloaded" | "slow_down" => ApiError::ServerOverloaded,
        "server_error" | "internal_server_error" | "rate_limit_exceeded" => {
            let delay = try_parse_retry_after(&Error {
                code: code.map(str::to_owned),
                message: Some(message.clone()),
            });
            ApiError::Retryable { message, delay }
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
        "response.custom_tool_call_input.delta" => {
            if let (Some(delta), Some(item_id)) =
                (event.delta, event.item_id.clone().or(event.call_id.clone()))
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
            let error = event
                .response
                .and_then(|response| response.error)
                .unwrap_or(Value::Null);
            return Err(ResponsesEventError::Api(provider_error(&error)));
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
                ResponsesEventError::Api(ApiError::Stream(
                    "response.completed event missing response".into(),
                ))
            })?;
            let response = serde_json::from_value::<ResponseCompleted>(serde_json::json!({
                "id": response.id, "end_turn": response.end_turn, "usage": response.usage
            }))
            .map_err(|error| {
                ResponsesEventError::Api(ApiError::Stream(decode_diagnostic(
                    "failed to parse ResponseCompleted",
                    &error,
                )))
            })?;
            return Ok(Some(ResponseEvent::Completed {
                response_id: response.id,
                token_usage: parse_usage(response.usage),
                end_turn: response.end_turn,
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
        ResponsesEventError::Api(ApiError::Stream(message))
    })?;
    serde_json::from_value(item).map_err(|error| {
        let message = decode_diagnostic(
            &format!("failed to parse ResponseItem from {event_kind}"),
            &error,
        );
        debug!("{message}");
        ResponsesEventError::Api(ApiError::Stream(message))
    })
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
    let value = captures.get(1)?.as_str().parse::<f64>().ok()?;
    let unit = captures.get(2)?.as_str().to_ascii_lowercase();
    if unit == "s" || unit.starts_with("second") {
        Duration::try_from_secs_f64(value).ok()
    } else if unit == "ms" {
        Duration::try_from_secs_f64(value / 1000.0).ok()
    } else {
        None
    }
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
        regex_lite::Regex::new(r"(?i)try again in\s*(\d+(?:\.\d+)?)\s*(s|ms|seconds?)").unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;
    use serde_json::json;

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
                "output":[{"unused":"snapshot"}]}}).to_string();
            let events = interpreter
                .process_payload(&payload)
                .unwrap()
                .collect::<Vec<_>>();
            assert!(matches!(events.as_slice(), [ResponseEvent::Completed {
                response_id, token_usage: None, end_turn: Some(true) }] if response_id == "done"));
        }
        for response in [
            json!({"end_turn":true}),
            json!({"id":"done","end_turn":"true"}),
        ] {
            let mut interpreter =
                ResponsesEventInterpreter::new(&ResponsesStreamMetadata::default(), None);
            assert!(
                interpreter
                    .process_payload(
                        &json!({"type":"response.completed","response":response}).to_string()
                    )
                    .is_err()
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
        assert!(
            interpreter
                .process_payload(r#"{"type":"response.output_text.delta","delta":{}}"#)
                .is_err()
        );
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
                if code == "future_failure" {
                    assert!(
                        matches!(mapped, codex_protocol::error::CodexErr::ProviderFailure { code: Some(ref actual), .. } if actual == code)
                    );
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
        for payload in [
            r#"{"type":"response.output_text.delta","delta":"hello"}"#,
            r#"{"delta":"hello","type":"response.output_text.\u0064elta"}"#,
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
                    ResponsesEventError::Api(ApiError::Stream(message)) => {
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
    fn ordinary_stream_event_ignores_unmatched_fields_without_rate_limit_flattening() {
        let mut interpreter = ResponsesEventInterpreter::new(
            &ResponsesStreamMetadata::default(),
            /*turn_state*/ None,
        );

        let events = interpreter
            .process_payload(
                r#"{"type":"response.output_text.delta","delta":"hello","obfuscation":"ignored","sequence_number":7}"#,
            )
            .expect("ordinary delta should ignore unmatched transport fields");

        assert!(
            matches!(events.as_slice(), [ResponseEvent::OutputTextDelta(delta)] if delta == "hello")
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

    #[test]
    fn parses_retry_after_units() {
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
            }) if actual_delay == expected && actual_message == message)
            );
        }
    }
}
