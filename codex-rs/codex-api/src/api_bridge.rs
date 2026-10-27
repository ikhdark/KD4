use crate::TransportError;
use crate::X_ERROR_JSON_HEADER;
use crate::X_OPENAI_AUTHORIZATION_ERROR_HEADER;
use crate::error::ApiError;
use crate::rate_limits::parse_promo_message;
use crate::rate_limits::parse_rate_limit_for_limit;
use crate::rate_limits::parse_rate_limit_reached_type;
use crate::responses_stream::REQUEST_ID_HEADER;
use base64::Engine;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::error::CodexErr;
use codex_protocol::error::ConnectionFailedError;
use codex_protocol::error::ResponseStreamFailed;
use codex_protocol::error::RetryLimitReachedError;
use codex_protocol::error::UnexpectedResponseError;
use codex_protocol::error::UsageLimitReachedError;
use http::HeaderMap;
use serde::Deserialize;
use serde_json::Value;

pub fn map_api_error(err: ApiError) -> CodexErr {
    match err {
        ApiError::ContextWindowExceeded => CodexErr::ContextWindowExceeded,
        ApiError::QuotaExceeded => CodexErr::QuotaExceeded,
        ApiError::UsageNotIncluded => CodexErr::UsageNotIncluded,
        ApiError::Retryable { message, delay } => CodexErr::Stream(message, delay),
        ApiError::WebsocketRejected { reason, message, delay } => CodexErr::WebsocketRejected {
            reason, message, retry_after: delay,
        },
        ApiError::IncompleteResponse(response) => CodexErr::IncompleteResponse(response),
        ApiError::TerminalResponse {
            source,
            response_id,
            token_usage,
        } => CodexErr::TerminalResponse {
            source: Box::new(map_api_error(*source)),
            response_id,
            token_usage,
        },
        ApiError::ProviderFailure { code, message } => CodexErr::ProviderFailure { code, message },
        ApiError::WebsocketConnect(message) => {
            CodexErr::ConnectionFailed(ConnectionFailedError {
                message,
                status: None,
            })
        }
        ApiError::WebsocketClosed { code, message } => {
            // RFC policy/protocol/payload rejection cannot be repaired by
            // replaying the same inference over another transport. Other codes
            // (notably 1011, an unexpected server condition) retain recovery.
            if matches!(code, Some(1002 | 1003 | 1007 | 1008 | 1009 | 1010)) {
                CodexErr::ProviderFailure {
                    code: code.map(|code| format!("websocket_close_{code}")),
                    message,
                }
            } else {
                CodexErr::ResponseStreamFailed(ResponseStreamFailed {
                    message,
                    status: None,
                    request_id: None,
                    idle_timeout: false,
                    reconnectable_close: matches!(code, Some(1000 | 1001 | 1012 | 1013)),
                })
            }
        }
        ApiError::Stream(message) => {
            // A close that invites a reconnect is recoverable by rebuilding the
            // transport. Provider silence is only classified: post-dispatch
            // stalls still spend the bounded retry budget. Classify before the
            // message is moved into the error.
            let idle_timeout = crate::error::is_stream_idle_timeout_message(&message);
            let reconnectable_close = crate::error::is_reconnectable_close_message(&message);
            // The marker carried the close code past the frame; it is routing
            // state, not something to show a user.
            let message = crate::error::strip_reconnectable_close_marker(message);
            CodexErr::ResponseStreamFailed(ResponseStreamFailed {
                message,
                status: None,
                request_id: None,
                idle_timeout,
                reconnectable_close,
            })
        }
        ApiError::ServerOverloaded { retry_after } => CodexErr::ServerOverloaded { retry_after },
        ApiError::Api { status, message } => {
            let user_message = api_error_user_message(status, &message);
            map_unexpected_response(UnexpectedResponseError {
                retry_after: None,
                status,
                body: message,
                user_message,
                url: None,
                cf_ray: None,
                request_id: None,
                identity_authorization_error: None,
                identity_error_code: None,
            })
        }
        ApiError::InvalidRequest { message } => CodexErr::InvalidRequest(message),
        ApiError::CyberPolicy { message } => CodexErr::CyberPolicy { message },
        ApiError::Transport(transport) => match transport {
            TransportError::Http {
                status,
                url,
                headers,
                body,
                mut retry_after,
            } => {
                let body_text = body.unwrap_or_default();

                if let Ok(parsed) = serde_json::from_str::<Value>(&body_text)
                    && let Some(error) = parsed.get("error")
                    && let Some(classified) =
                        crate::responses_stream::classify_provider_error(error)
                {
                    // HTTP capacity identity and diagnostics belong to the
                    // status envelope. Do not refine a 429/503 into a generic
                    // stream error or discard its request id and body.
                    if !matches!(status, http::StatusCode::TOO_MANY_REQUESTS | http::StatusCode::SERVICE_UNAVAILABLE)
                        || !matches!(&classified, ApiError::ServerOverloaded { .. } | ApiError::Retryable { .. })
                    {
                        return map_api_error(classified).with_retry_after(retry_after);
                    }
                    retry_after = retry_after.or(match classified {
                        ApiError::ServerOverloaded { retry_after } => retry_after,
                        ApiError::Retryable { delay, .. } => delay,
                        _ => None,
                    });
                }

                if status == http::StatusCode::BAD_REQUEST {
                    if body_text
                        .contains("The image data you provided does not represent a valid image")
                    {
                        CodexErr::InvalidImageRequest()
                    } else {
                        CodexErr::InvalidRequest(body_text)
                    }
                } else if status == http::StatusCode::INTERNAL_SERVER_ERROR
                    // Upgrade rejections retain their normalized ws(s) URL. Keep them
                    // transport-class so exhausted WebSocket retries can fall back to HTTP;
                    // HTTP responses and established-stream errors remain provider-class.
                    && !url.as_deref().is_some_and(|url| {
                        url.starts_with("ws://") || url.starts_with("wss://")
                    })
                {
                    CodexErr::InternalServerError { retry_after }
                } else {
                    if status == http::StatusCode::TOO_MANY_REQUESTS
                        && let Ok(err) = serde_json::from_str::<UsageErrorResponse>(&body_text)
                    {
                        if err.error.error_type.as_deref() == Some("usage_limit_reached") {
                            let limit_id = extract_header(headers.as_ref(), ACTIVE_LIMIT_HEADER);
                            let rate_limits = headers.as_ref().and_then(|map| {
                                parse_rate_limit_for_limit(map, limit_id.as_deref())
                            });
                            let promo_message = headers.as_ref().and_then(parse_promo_message);
                            let rate_limit_reached_type =
                                headers.as_ref().and_then(parse_rate_limit_reached_type);
                            let resets_at = err
                                .error
                                .resets_at
                                .and_then(|value| value.as_i64())
                                .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0));
                            return CodexErr::UsageLimitReached(UsageLimitReachedError {
                                plan_type: err
                                    .error
                                    .plan_type
                                    .and_then(|value| serde_json::from_value(value).ok()),
                                resets_at,
                                rate_limits: rate_limits.map(Box::new),
                                promo_message,
                                rate_limit_reached_type,
                            });
                        } else if err.error.error_type.as_deref() == Some("usage_not_included") {
                            return CodexErr::UsageNotIncluded;
                        }
                    }

                    // Any other 429 is throttling that no lower layer retried: the
                    // non-idempotent transport policy never replays an HTTP status.
                    // Keep it a retryable unexpected status so the turn owner can
                    // honor Retry-After instead of reporting an exhausted budget.
                    map_unexpected_response(UnexpectedResponseError {
                        retry_after,
                        status,
                        user_message: api_error_user_message(status, &body_text),
                        body: body_text,
                        url,
                        cf_ray: extract_header(headers.as_ref(), CF_RAY_HEADER),
                        request_id: extract_request_id(headers.as_ref()),
                        identity_authorization_error: extract_header(
                            headers.as_ref(),
                            X_OPENAI_AUTHORIZATION_ERROR_HEADER,
                        ),
                        identity_error_code: extract_x_error_json_code(headers.as_ref()),
                    })
                }
            }
            TransportError::RetryLimit => CodexErr::RetryLimit(RetryLimitReachedError {
                retry_after: None,
                status: http::StatusCode::INTERNAL_SERVER_ERROR,
                request_id: None,
            }),
            TransportError::Timeout => CodexErr::RequestTimeout,
            TransportError::ConnectionTimeout => {
                CodexErr::ConnectionFailed(ConnectionFailedError {
                    message: "connection establishment timed out before model dispatch".into(),
                    status: None,
                })
            }
            TransportError::Connection(source) => {
                let status = source.status();
                CodexErr::ConnectionFailed(ConnectionFailedError {
                    message: source.to_string(),
                    status,
                })
            }
            TransportError::PreDispatch(msg) => CodexErr::PreDispatchRetryExhausted(msg),
            TransportError::Network(message) => {
                CodexErr::ResponseStreamFailed(ResponseStreamFailed {
                    message,
                    status: None,
                    request_id: None,
                    // A transport-level network failure, not provider silence.
                    // `TransportError::Connection` above is the recoverable
                    // case and already maps to `ConnectionFailed`.
                    idle_timeout: false,
                    // The transport broke; the provider never closed the stream.
                    reconnectable_close: false,
                })
            }
            TransportError::Build(msg) => CodexErr::RequestBuild(msg),
            TransportError::ResponseTooLarge { limit } => CodexErr::ProviderFailure {
                code: Some("response_body_too_large".into()),
                message: format!(
                    "Successful response exceeded the {limit} byte receive limit; the request was not replayed."
                ),
            },
        },
        ApiError::RateLimit(msg) => CodexErr::Stream(msg, None),
    }
}

const ACTIVE_LIMIT_HEADER: &str = "x-codex-active-limit";
const OAI_REQUEST_ID_HEADER: &str = "x-oai-request-id";
const CF_RAY_HEADER: &str = "cf-ray";
const CLOUDFLARE_BLOCKED_MESSAGE: &str = "Access blocked by Cloudflare";


fn api_error_user_message(status: http::StatusCode, body: &str) -> Option<String> {
    if is_cloudflare_access_block(status, body) {
        Some(format!("{CLOUDFLARE_BLOCKED_MESSAGE} (status {status})"))
    } else {
        None
    }
}

fn map_unexpected_response(error: UnexpectedResponseError) -> CodexErr {
    CodexErr::UnexpectedStatus(error)
}

fn is_cloudflare_access_block(status: http::StatusCode, body: &str) -> bool {
    status == http::StatusCode::FORBIDDEN && body.contains("Cloudflare") && body.contains("blocked")
}

fn extract_request_id(headers: Option<&HeaderMap>) -> Option<String> {
    extract_header(headers, REQUEST_ID_HEADER)
        .or_else(|| extract_header(headers, OAI_REQUEST_ID_HEADER))
}

fn extract_header(headers: Option<&HeaderMap>, name: &str) -> Option<String> {
    headers.and_then(|map| {
        map.get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    })
}

fn extract_x_error_json_code(headers: Option<&HeaderMap>) -> Option<String> {
    let encoded = extract_header(headers, X_ERROR_JSON_HEADER)?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let parsed = serde_json::from_slice::<Value>(&decoded).ok()?;
    parsed
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[derive(Debug, Deserialize)]
struct UsageErrorResponse {
    error: UsageErrorBody,
}

#[derive(Debug, Deserialize)]
struct UsageErrorBody {
    #[serde(rename = "type")]
    error_type: Option<String>,
    plan_type: Option<Value>,
    resets_at: Option<Value>,
}
