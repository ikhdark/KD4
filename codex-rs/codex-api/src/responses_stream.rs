use crate::common::ResponseEvent;
use crate::common::SafetyBuffering;
use crate::common::SafetyBufferingTreatment;
use crate::error::ApiError;
use crate::rate_limits::parse_all_rate_limits;
use crate::rate_limits::parse_rate_limit_event;
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
use serde_json::Map as JsonMap;
use serde_json::Value;
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
            reasoning_included: headers.contains_key(X_REASONING_INCLUDED_HEADER),
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
        }
    }

    pub(crate) fn process_payload(
        &mut self,
        payload: &str,
    ) -> Result<Vec<ResponseEvent>, ResponsesEventError> {
        let event: ResponsesStreamEvent = serde_json::from_str(payload)?;

        if let Some(response_turn_state) = event.turn_state()
            && let Some(turn_state) = self.turn_state.as_deref()
        {
            let _ = turn_state.set(response_turn_state);
        }

        if let Some(headers) = event.headers.as_ref().and_then(Value::as_object)
            && let Some(updated_treatment) =
                treatment_from_headers(&json_headers_to_http_headers(headers))
        {
            self.safety_buffering_treatment = updated_treatment;
        }

        if event.kind() == "codex.rate_limits" {
            return Ok(parse_rate_limit_event(payload)
                .map(ResponseEvent::RateLimits)
                .into_iter()
                .collect());
        }

        let mut events = Vec::new();
        if let Some(model) = event.response_model()
            && self.last_server_model.as_deref() != Some(model.as_str())
        {
            self.last_server_model = Some(model.clone());
            events.push(ResponseEvent::ServerModel(model));
        }
        if let Some(verifications) = event.model_verifications() {
            events.push(ResponseEvent::ModelVerifications(verifications));
        }
        if let Some(metadata) = event.turn_moderation_metadata() {
            events.push(ResponseEvent::TurnModerationMetadata(metadata));
        }
        if let Some(buffering) = event.safety_buffering(&self.safety_buffering_treatment) {
            events.push(ResponseEvent::SafetyBuffering(buffering));
        }
        if let Some(event) = process_responses_event(event)? {
            events.push(event);
        }
        Ok(events)
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
#[allow(dead_code)]
struct Error {
    r#type: Option<String>,
    code: Option<String>,
    message: Option<String>,
    plan_type: Option<String>,
    resets_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ResponseCompleted {
    id: String,
    #[serde(default)]
    usage: Option<ResponseCompletedUsage>,
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
pub(crate) struct ResponsesStreamEvent {
    #[serde(rename = "type")]
    kind: String,
    headers: Option<Value>,
    metadata: Option<Value>,
    response: Option<Value>,
    item: Option<Value>,
    item_id: Option<String>,
    call_id: Option<String>,
    delta: Option<String>,
    text: Option<String>,
    summary_index: Option<i64>,
    content_index: Option<i64>,
    safety_buffering: Option<Value>,
}

impl ResponsesStreamEvent {
    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }

    pub(crate) fn response_model(&self) -> Option<String> {
        self.response
            .as_ref()
            .and_then(|response| response.get("headers"))
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
        let mut buffering: SafetyBuffering = serde_json::from_value(value.clone()).ok()?;
        buffering.show_buffering_ui = true;
        if !retry_model_present {
            buffering.faster_model.clone_from(&treatment.faster_model);
        }
        Some(buffering)
    }
}

fn header_openai_model_value_from_json(value: &Value) -> Option<String> {
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
            .then(|| json_value_as_string(value))
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

fn json_value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Array(items) => items.first().and_then(json_value_as_string),
        _ => None,
    }
}

fn process_responses_event(
    event: ResponsesStreamEvent,
) -> Result<Option<ResponseEvent>, ResponsesEventError> {
    match event.kind.as_str() {
        "response.output_item.done" => {
            if let Some(item_value) = event.item {
                if let Ok(item) = serde_json::from_value::<ResponseItem>(item_value) {
                    return Ok(Some(ResponseEvent::OutputItemDone(item)));
                }
                debug!("failed to parse ResponseItem from output_item.done");
            }
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
            let Some(response_value) = event.response else {
                return Err(ResponsesEventError::Api(ApiError::Stream(
                    "response.failed event received".into(),
                )));
            };
            let mut response_error = ApiError::Stream("response.failed event received".into());
            if let Some(error) = response_value.get("error")
                && let Ok(error) = serde_json::from_value::<Error>(error.clone())
            {
                if is_context_window_error(&error) {
                    response_error = ApiError::ContextWindowExceeded;
                } else if is_quota_exceeded_error(&error) {
                    response_error = ApiError::QuotaExceeded;
                } else if is_usage_not_included(&error) {
                    response_error = ApiError::UsageNotIncluded;
                } else if is_cyber_policy_error(&error) {
                    response_error = ApiError::CyberPolicy {
                        message: cyber_policy_message(error.message),
                    };
                } else if matches!(error.code.as_deref(), Some("invalid_prompt" | "bio_policy")) {
                    response_error = ApiError::InvalidRequest {
                        message: error
                            .message
                            .unwrap_or_else(|| "Invalid request.".to_string()),
                    };
                } else if is_server_overloaded_error(&error) {
                    response_error = ApiError::ServerOverloaded;
                } else {
                    response_error = ApiError::Retryable {
                        delay: try_parse_retry_after(&error),
                        message: error.message.unwrap_or_default(),
                    };
                }
            }
            return Err(ResponsesEventError::Api(response_error));
        }
        "response.incomplete" => {
            let reason = event
                .response
                .as_ref()
                .and_then(|response| response.get("incomplete_details"))
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return Err(ResponsesEventError::Api(ApiError::Stream(format!(
                "Incomplete response returned, reason: {reason}"
            ))));
        }
        "response.completed" => {
            if let Some(response_value) = event.response {
                let response = serde_json::from_value::<ResponseCompleted>(response_value)
                    .map_err(|error| {
                        let message = format!("failed to parse ResponseCompleted: {error}");
                        debug!("{message}");
                        ResponsesEventError::Api(ApiError::Stream(message))
                    })?;
                return Ok(Some(ResponseEvent::Completed {
                    response_id: response.id,
                    token_usage: response.usage.map(Into::into),
                    end_turn: response.end_turn,
                }));
            }
        }
        "response.output_item.added" => {
            if let Some(item_value) = event.item {
                if let Ok(item) = serde_json::from_value::<ResponseItem>(item_value) {
                    return Ok(Some(ResponseEvent::OutputItemAdded(item)));
                }
                debug!("failed to parse ResponseItem from output_item.added");
            }
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

pub(crate) fn json_headers_to_http_headers(headers: &JsonMap<String, Value>) -> HeaderMap {
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
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
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
        Some(Duration::from_secs_f64(value))
    } else if unit == "ms" {
        Some(Duration::from_millis(value as u64))
    } else {
        None
    }
}

fn is_context_window_error(error: &Error) -> bool {
    error.code.as_deref() == Some("context_length_exceeded")
}

fn is_quota_exceeded_error(error: &Error) -> bool {
    error.code.as_deref() == Some("insufficient_quota")
}

fn is_usage_not_included(error: &Error) -> bool {
    error.code.as_deref() == Some("usage_not_included")
}

fn is_cyber_policy_error(error: &Error) -> bool {
    error.code.as_deref() == Some("cyber_policy")
}

fn is_server_overloaded_error(error: &Error) -> bool {
    matches!(
        error.code.as_deref(),
        Some("server_is_overloaded" | "slow_down")
    )
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
            .expect("metadata should be interpreted");
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
            .expect("safety event should be interpreted");
        assert!(
            matches!(&safety_events[0], ResponseEvent::SafetyBuffering(buffering) if buffering.faster_model.as_deref() == Some("fast-model"))
        );
        assert!(matches!(&safety_events[1], ResponseEvent::OutputTextDelta(delta) if delta == "x"));

        let rate_limit_events = interpreter
            .process_payload(
                r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":12.5,"window_minutes":60,"reset_at":42}}}"#,
            )
            .expect("rate limits should be interpreted");
        assert!(
            matches!(&rate_limit_events[0], ResponseEvent::RateLimits(snapshot) if snapshot.primary.as_ref().is_some_and(|window| window.used_percent == 12.5))
        );
    }

    #[test]
    fn parses_retry_after_units() {
        for (message, expected) in [
            ("Please try again in 28ms.", Duration::from_millis(28)),
            (
                "Please try again in 1.898s.",
                Duration::from_secs_f64(1.898),
            ),
            ("Try again in 35 seconds.", Duration::from_secs(35)),
        ] {
            let error = Error {
                r#type: None,
                code: Some("rate_limit_exceeded".to_string()),
                message: Some(message.to_string()),
                plan_type: None,
                resets_at: None,
            };
            assert_eq!(try_parse_retry_after(&error), Some(expected));
        }
    }
}
