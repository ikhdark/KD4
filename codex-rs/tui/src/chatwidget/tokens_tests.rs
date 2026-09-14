use super::*;
use codex_app_server_protocol::AccountTokenUsageSummary;
use pretty_assertions::assert_eq;

#[test]
fn loaded_state_freezes_chart_anchor_date_at_completion() {
    let (cell, handle) = new_token_activity_output(TokenActivityView::Daily);
    let today =
        NaiveDate::from_ymd_opt(/*year*/ 2026, /*month*/ 5, /*day*/ 29).expect("valid date");

    handle.finish_with_today(
        Ok(GetAccountTokenUsageResponse {
            summary: AccountTokenUsageSummary {
                lifetime_tokens: None,
                peak_daily_tokens: None,
                longest_running_turn_sec: None,
                current_streak_days: None,
                longest_streak_days: None,
            },
            daily_usage_buckets: Some(vec![
                codex_app_server_protocol::AccountTokenUsageDailyBucket {
                    start_date: "2026-05-29".to_string(),
                    tokens: 100,
                },
            ]),
        }),
        today,
    );

    let lines = cell.display_lines(108);
    let last_day_cell = |label: &str| {
        lines
            .iter()
            .find(|line| line.spans.first().is_some_and(|span| span.content == label))
            .expect("weekday row")
            .spans
            .last()
            .expect("final week cell")
    };
    // May 29, 2026 is Friday: Friday has activity and Saturday is still in the future.
    assert_eq!(last_day_cell(" Sa ").content, " ");
    assert!(!last_day_cell(" Fr ").content.trim().is_empty());
    assert_ne!(last_day_cell(" Fr "), last_day_cell(" Th "));
    assert!(lines.iter().any(|line| line.to_string().contains("May")));
}
