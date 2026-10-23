use super::*;
use crate::session::tests::make_session_configuration_for_tests;
use crate::state::AutoCompactWindowSnapshot;
use codex_protocol::protocol::CreditsSnapshot;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RateLimitWindow;
use codex_protocol::protocol::SpendControlLimitSnapshot;
use codex_protocol::protocol::TokenUsageCoverage;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn connector_selection_merges_deduplicates_and_clears() {
    let session_configuration = make_session_configuration_for_tests().await;
    let mut state = SessionState::new(session_configuration);
    let merged = state.merge_connector_selection([
        "calendar".to_string(),
        "calendar".to_string(),
        "drive".to_string(),
    ]);

    assert_eq!(
        merged,
        HashSet::from(["calendar".to_string(), "drive".to_string()])
    );
    assert_eq!(state.get_connector_selection(), merged);
    assert_eq!(
        state.merge_connector_selection(["calendar".to_string(), "mail".to_string()]),
        HashSet::from([
            "calendar".to_string(),
            "drive".to_string(),
            "mail".to_string()
        ])
    );

    state.clear_connector_selection();

    assert_eq!(state.get_connector_selection(), HashSet::new());
}

#[tokio::test]
async fn set_rate_limits_defaults_limit_id_to_codex_when_missing() {
    let session_configuration = make_session_configuration_for_tests().await;
    let mut state = SessionState::new(session_configuration);

    state.set_rate_limits(RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 12.0,
            window_minutes: Some(60),
            resets_at: Some(100),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    });

    assert_eq!(
        state
            .latest_rate_limits
            .as_ref()
            .and_then(|v| v.limit_id.clone()),
        Some("codex".to_string())
    );
}

#[tokio::test]
async fn replace_history_clears_auto_compact_window_prefill() {
    let session_configuration = make_session_configuration_for_tests().await;
    let mut state = SessionState::new(session_configuration);

    state.set_auto_compact_window_estimated_prefill(/*tokens*/ 100);
    state
        .prompt_token_calibration
        .observe("provider", "model", Some((100, 200)));
    assert_eq!(
        state
            .prompt_token_calibration
            .estimate("provider", "model", 100),
        200
    );
    state.replace_history(Vec::new(), /*reference_context_item*/ None);

    assert_eq!(
        state.auto_compact_window_snapshot(),
        AutoCompactWindowSnapshot {
            prefill_input_tokens: None,
        }
    );
    assert_eq!(
        state
            .prompt_token_calibration
            .estimate("provider", "model", 100),
        100
    );
}

#[tokio::test]
async fn replace_history_reconciles_additional_context_delivery() {
    use codex_protocol::protocol::AdditionalContextEntry;
    use codex_protocol::protocol::AdditionalContextKind;
    use indexmap::IndexMap;

    let mut state = SessionState::new(make_session_configuration_for_tests().await);
    let values = IndexMap::from([(
        "constraint".to_string(),
        AdditionalContextEntry {
            value: "Do not publish this project.".to_string(),
            kind: AdditionalContextKind::Application,
        },
    )]);
    let first = state.additional_context.merge(values.clone());
    let retained = first.iter().cloned().map(ResponseItem::from).collect();
    state.replace_history(retained, None);
    assert!(state.additional_context.merge(values.clone()).is_empty());

    state.replace_history(Vec::new(), None);
    assert_eq!(state.additional_context.merge(values.clone()), first);
    assert!(state.additional_context.merge(values).is_empty());
}

#[tokio::test]
async fn set_rate_limits_defaults_to_codex_when_limit_id_missing_after_other_bucket() {
    let session_configuration = make_session_configuration_for_tests().await;
    let mut state = SessionState::new(session_configuration);

    state.set_rate_limits(RateLimitSnapshot {
        limit_id: Some("codex_other".to_string()),
        limit_name: Some("codex_other".to_string()),
        primary: Some(RateLimitWindow {
            used_percent: 20.0,
            window_minutes: Some(60),
            resets_at: Some(200),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    });
    state.set_rate_limits(RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 30.0,
            window_minutes: Some(60),
            resets_at: Some(300),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    });

    assert_eq!(
        state
            .latest_rate_limits
            .as_ref()
            .and_then(|v| v.limit_id.clone()),
        Some("codex".to_string())
    );
}

#[tokio::test]
async fn set_rate_limits_carries_account_metadata_from_codex_to_codex_other() {
    let session_configuration = make_session_configuration_for_tests().await;
    let mut state = SessionState::new(session_configuration);

    state.set_rate_limits(RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: Some("codex".to_string()),
        primary: Some(RateLimitWindow {
            used_percent: 10.0,
            window_minutes: Some(60),
            resets_at: Some(100),
        }),
        secondary: None,
        credits: Some(CreditsSnapshot {
            has_credits: true,
            unlimited: false,
            balance: Some("50".to_string()),
        }),
        individual_limit: Some(SpendControlLimitSnapshot {
            limit: "25000".to_string(),
            used: "8000".to_string(),
            remaining_percent: 68,
            resets_at: 300,
        }),
        spend_control_reached: Some(true),
        plan_type: Some(codex_protocol::account::PlanType::Plus),
        rate_limit_reached_type: None,
    });

    state.set_rate_limits(RateLimitSnapshot {
        limit_id: Some("codex_other".to_string()),
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 30.0,
            window_minutes: Some(120),
            resets_at: Some(200),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    });

    assert_eq!(
        state.latest_rate_limits,
        Some(RateLimitSnapshot {
            limit_id: Some("codex_other".to_string()),
            limit_name: None,
            primary: Some(RateLimitWindow {
                used_percent: 30.0,
                window_minutes: Some(120),
                resets_at: Some(200),
            }),
            secondary: None,
            credits: Some(CreditsSnapshot {
                has_credits: true,
                unlimited: false,
                balance: Some("50".to_string()),
            }),
            individual_limit: Some(SpendControlLimitSnapshot {
                limit: "25000".to_string(),
                used: "8000".to_string(),
                remaining_percent: 68,
                resets_at: 300,
            }),
            spend_control_reached: Some(true),
            plan_type: Some(codex_protocol::account::PlanType::Plus),
            rate_limit_reached_type: None,
        })
    );

    state.set_rate_limits(RateLimitSnapshot {
        limit_id: Some("codex_other".to_string()),
        limit_name: None,
        primary: None,
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: Some(false),
        plan_type: None,
        rate_limit_reached_type: None,
    });

    assert_eq!(
        state
            .latest_rate_limits
            .as_ref()
            .and_then(|snapshot| snapshot.spend_control_reached),
        Some(false)
    );
}

fn codex_windows() -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 42.0,
            window_minutes: Some(300),
            resets_at: Some(1_000),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 7.0,
            window_minutes: Some(10_080),
            resets_at: Some(2_000),
        }),
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    }
}

/// What `parse_all_rate_limits` yields for a response without rate-limit headers.
fn header_less(limit_id: &str) -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: Some(limit_id.to_string()),
        limit_name: None,
        primary: None,
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    }
}

fn reported_usage(input_tokens: i64, cached_input_tokens: i64, output_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_output_tokens: 0,
        total_tokens: input_tokens + output_tokens,
        coverage: Some(TokenUsageCoverage::fully_reported()),
    }
}

fn published_token_counts(events: &async_channel::Receiver<Event>) -> Vec<TokenCountEvent> {
    std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event.msg {
            EventMsg::TokenCount(count) => Some(count),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn set_rate_limits_keeps_bucket_windows_for_window_less_updates() {
    let mut state = SessionState::new(make_session_configuration_for_tests().await);
    state.set_rate_limits(codex_windows());

    state.set_rate_limits(header_less("codex"));
    assert_eq!(state.latest_rate_limits, Some(codex_windows()));

    // Another bucket's windows describe a different quota.
    state.set_rate_limits(header_less("codex_other"));
    assert_eq!(state.latest_rate_limits, Some(header_less("codex_other")));
}

#[tokio::test]
async fn token_count_publishes_only_changed_snapshots() {
    let (session, turn, events) = crate::session::tests::make_session_and_context_with_rx().await;
    while events.try_recv().is_ok() {}

    session.update_rate_limits(&turn, codex_windows()).await;
    // Repeated headers, a header-less response, and a lifecycle re-send change
    // neither usage nor limits.
    session.update_rate_limits(&turn, codex_windows()).await;
    session.update_rate_limits(&turn, header_less("codex")).await;
    session.send_token_count_event(&turn).await;
    let usage = reported_usage(100, 0, 10);
    session
        .update_token_usage_info(&turn, Some(&usage))
        .await
        .expect("usage should be recorded");
    session.send_token_count_event(&turn).await;

    let published = published_token_counts(&events);
    assert_eq!(published.len(), 2);
    assert_eq!(published[0].rate_limits, Some(codex_windows()));
    let info = published[1].info.as_ref().expect("published usage");
    assert_eq!(info.last_token_usage, usage);
    assert_eq!(published[1].rate_limits, Some(codex_windows()));
}

#[tokio::test]
async fn compaction_usage_is_published_as_latest_request_without_moving_context() {
    let (session, turn, events) = crate::session::tests::make_session_and_context_with_rx().await;
    let conversation = reported_usage(441_132, 437_120, 1_889);
    session
        .record_token_usage_info(&turn, Some(&conversation))
        .await
        .expect("conversation usage should be recorded");
    let before = session.token_usage_info().await.expect("token usage");
    let context_before = session.get_context_tokens_for_turn(&turn).await;
    while events.try_recv().is_ok() {}

    let compaction = reported_usage(327_291, 0, 3_132);
    session
        .record_compaction_token_usage(&turn, Some(&compaction))
        .await
        .expect("compaction usage should be recorded");

    let stored = session.token_usage_info().await.expect("token usage");
    let published = published_token_counts(&events);
    let [published] = published.as_slice() else {
        panic!("expected one token count, got {published:?}");
    };
    let info = published.info.as_ref().expect("published usage");
    assert_eq!(info.last_token_usage, compaction);
    assert_eq!(info.total_token_usage, stored.total_token_usage);
    assert_eq!(info.context_tokens(), before.context_tokens());
    // History keeps the conversation receipt as its context baseline.
    assert_eq!(stored.last_token_usage, before.last_token_usage);
    assert_eq!(stored.estimated_context_tokens, before.estimated_context_tokens);
    assert_eq!(session.get_context_tokens_for_turn(&turn).await, context_before);

    // The next conversation receipt is the latest request again.
    let next = reported_usage(9_000, 8_000, 100);
    session
        .update_token_usage_info(&turn, Some(&next))
        .await
        .expect("next usage should be recorded");
    let published = published_token_counts(&events);
    let [published] = published.as_slice() else {
        panic!("expected one token count, got {published:?}");
    };
    let info = published.info.as_ref().expect("published usage");
    assert_eq!(info.last_token_usage, next);
    assert_eq!(info.estimated_context_tokens, None);
}
