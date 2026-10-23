use super::should_retry_with_current_model;
use codex_protocol::error::CodexErr;
use codex_protocol::error::RetryLimitReachedError;
use codex_protocol::error::UnexpectedResponseError;
use codex_protocol::error::UsageLimitReachedError;
use http::StatusCode;

#[test]
fn current_model_fallback_matches_model_specific_failures() {
    let fallback_errors = [
        CodexErr::InvalidRequest("invalid request".to_string()),
        CodexErr::UnexpectedStatus(UnexpectedResponseError {
            retry_after: None,
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
        CodexErr::ServerOverloaded { retry_after: None },
        CodexErr::InternalServerError { retry_after: None },
    ];
    for error in &fallback_errors {
        assert!(
            should_retry_with_current_model(error),
            "expected current-model fallback for {error:?}"
        );
    }

    let non_fallback_errors = [
        CodexErr::UsageLimitReached(UsageLimitReachedError {
            plan_type: None,
            resets_at: None,
            rate_limits: None,
            promo_message: None,
            rate_limit_reached_type: None,
        }),
        CodexErr::RetryLimit(RetryLimitReachedError {
            retry_after: None,
            status: StatusCode::TOO_MANY_REQUESTS,
            request_id: None,
        }),
        CodexErr::RegionRestricted(UnexpectedResponseError {
            retry_after: None,
            status: StatusCode::FORBIDDEN,
            body: "Cloudflare blocked".to_string(),
            user_message: Some("service unavailable in this region".to_string()),
            url: None,
            cf_ray: None,
            request_id: None,
            identity_authorization_error: None,
            identity_error_code: None,
        }),
        CodexErr::UnexpectedStatus(UnexpectedResponseError {
            retry_after: None,
            status: StatusCode::UNAUTHORIZED,
            body: String::new(),
            user_message: None,
            url: None,
            cf_ray: None,
            request_id: None,
            identity_authorization_error: None,
            identity_error_code: None,
        }),
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

#[test]
fn local_compaction_does_not_bypass_provider_or_storage_failures() {
    use super::should_retry_with_local_compaction;
    for error in [
        CodexErr::QuotaExceeded,
        CodexErr::UsageNotIncluded,
        CodexErr::Fatal("recovery artifact unavailable".into()),
        CodexErr::Io(std::io::Error::other("recovery write failed")),
        CodexErr::RequestTimeout,
        CodexErr::ProviderFailure {
            code: Some("dispatch_outcome_unknown".into()),
            message: "accepted upload then reset".into(),
        },
        CodexErr::TurnAborted,
    ] {
        assert!(!should_retry_with_local_compaction(&error));
    }
    assert!(should_retry_with_local_compaction(
        &CodexErr::InvalidRequest("unsupported compaction format".into(),)
    ));
}

#[test]
fn rejected_checkpoints_fall_back_but_harness_failures_do_not() {
    use super::is_compaction_contract_violation;
    use super::should_retry_with_local_compaction;
    for message in [
        "compaction completed without a checkpoint handoff",
        "default compaction requires structured checkpoint sections",
        "compaction handoff is incomplete; missing non-empty sections: ## Evidence",
        "remote compaction v2 expected exactly one compaction output item, got 0 from 1 output items",
        "remote compaction v2 returned an empty checkpoint",
    ] {
        let error = CodexErr::Fatal(message.to_string());
        assert!(is_compaction_contract_violation(&error), "{message}");
        assert!(should_retry_with_current_model(&error), "{message}");
        assert!(should_retry_with_local_compaction(&error), "{message}");
    }
    for message in [
        "Compaction recovery artifact is unavailable; original history was retained.",
        "Compaction could not preserve exact unresolved text; original history was retained.",
        "Compaction source retention failed; original history was retained: disk full",
        "Compaction did not bring the context below its configured token limit.",
        // A diagnostic that quotes a rejection is not that rejection.
        "recovery failed after: remote compaction v2 returned an empty checkpoint",
    ] {
        let error = CodexErr::Fatal(message.to_string());
        assert!(!is_compaction_contract_violation(&error), "{message}");
        assert!(!should_retry_with_current_model(&error), "{message}");
        assert!(!should_retry_with_local_compaction(&error), "{message}");
    }
}
