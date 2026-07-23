use super::should_retry_with_current_model;
use codex_protocol::error::CodexErr;
use codex_protocol::error::RetryLimitReachedError;
use codex_protocol::error::UnexpectedResponseError;
use codex_protocol::error::UsageLimitReachedError;
use reqwest::StatusCode;

#[test]
fn current_model_fallback_matches_model_specific_failures() {
    let fallback_errors = [
        CodexErr::InvalidRequest("invalid request".to_string()),
        CodexErr::UnexpectedStatus(UnexpectedResponseError {
            status: StatusCode::BAD_GATEWAY,
            body: String::new(),
            user_message: None,
            url: None,
            cf_ray: None,
            request_id: None,
            identity_authorization_error: None,
            identity_error_code: None,
        }),
        CodexErr::ContextWindowExceeded,
        CodexErr::UsageLimitReached(UsageLimitReachedError {
            plan_type: None,
            resets_at: None,
            rate_limits: None,
            promo_message: None,
            rate_limit_reached_type: None,
        }),
        CodexErr::ServerOverloaded,
        CodexErr::InternalServerError,
        CodexErr::RetryLimit(RetryLimitReachedError {
            status: StatusCode::TOO_MANY_REQUESTS,
            request_id: None,
        }),
    ];
    for error in &fallback_errors {
        assert!(
            should_retry_with_current_model(error),
            "expected current-model fallback for {error:?}"
        );
    }

    let non_fallback_errors = [
        CodexErr::QuotaExceeded,
        CodexErr::CyberPolicy {
            message: "policy".to_string(),
        },
        CodexErr::Stream("stream".to_string(), None),
        CodexErr::TurnAborted,
    ];
    for error in &non_fallback_errors {
        assert!(
            !should_retry_with_current_model(error),
            "unexpected current-model fallback for {error:?}"
        );
    }
}
