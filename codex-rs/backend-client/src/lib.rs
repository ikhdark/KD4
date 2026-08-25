mod client;
pub(crate) mod types;

pub use client::AddCreditsNudgeCreditType;
pub use client::Client;
pub use client::RequestError;
pub use types::CodeTaskDetailsResponse;
pub use types::CodexUserSettingsResponse;
pub use types::CodexWorkspaceMessage;
pub use types::CodexWorkspaceMessageType;
pub use types::CodexWorkspaceMessagesResponse;
pub use types::ConfigBundleResponse;
pub use types::ConsumeRateLimitResetCreditCode;
pub use types::ConsumeRateLimitResetCreditResponse;
pub use types::DeliveredConfigToml;
pub use types::DeliveredManagedLayers;
pub use types::DeliveredRequirementsToml;
pub use types::DeliveredTomlFragment;
pub use types::PaginatedListTaskListItem;
pub use types::RateLimitResetCreditDetails;
pub use types::RateLimitResetCreditsDetails;
pub use types::RateLimitResetCreditsSummary;
pub use types::RateLimitsWithResetCredits;
pub use types::TaskListItem;
pub use types::TokenUsageProfile;
pub use types::TokenUsageProfileDailyBucket;
pub use types::TokenUsageProfileStats;
pub use types::TurnAttemptsSiblingTurnsResponse;

#[cfg(test)]
mod concrete_task_details_tests {
    use super::CodeTaskDetailsResponse;

    #[test]
    fn task_details_helpers_are_inherent_response_methods() {
        let details: CodeTaskDetailsResponse = serde_json::from_str(include_str!(
            "../tests/fixtures/task_details_with_diff.json"
        ))
        .expect("fixture should deserialize");
        let failed_details: CodeTaskDetailsResponse = serde_json::from_str(include_str!(
            "../tests/fixtures/task_details_with_error.json"
        ))
        .expect("error fixture should deserialize");

        assert!(
            details
                .unified_diff()
                .is_some_and(|diff| diff.contains("diff --git"))
        );
        assert_eq!(
            details.assistant_text_messages(),
            vec!["Assistant response".to_string()]
        );
        assert_eq!(
            details.user_text_prompt().as_deref(),
            Some("First line\n\nSecond line")
        );
        assert_eq!(
            failed_details.assistant_error_message().as_deref(),
            Some("APPLY_FAILED: Patch could not be applied")
        );
    }
}
