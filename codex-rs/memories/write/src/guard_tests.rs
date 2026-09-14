use super::*;
use codex_protocol::protocol::RateLimitReachedType;

fn snapshot(
    primary_used_percent: Option<f64>,
    secondary_used_percent: Option<f64>,
) -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: Some(crate::guard_limits::CODEX_LIMIT_ID.to_string()),
        limit_name: None,
        primary: primary_used_percent.map(window),
        secondary: secondary_used_percent.map(window),
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    }
}

fn window(used_percent: f64) -> RateLimitWindow {
    RateLimitWindow {
        used_percent,
        window_minutes: None,
        resets_at: None,
    }
}

#[test]
fn startup_check_uses_configured_remaining_threshold() {
    let snapshot = snapshot(
        /*primary_used_percent*/ Some(89.9),
        /*secondary_used_percent*/ Some(50.0),
    );

    assert!(snapshot_allows_startup(
        &snapshot, /*min_remaining_percent*/ 10
    ));
    assert!(!snapshot_allows_startup(
        &snapshot, /*min_remaining_percent*/ 11
    ));
}

#[test]
fn startup_check_skips_when_primary_or_secondary_is_too_low() {
    assert!(!snapshot_allows_startup(
        &snapshot(
            /*primary_used_percent*/ Some(75.1),
            /*secondary_used_percent*/ Some(10.0),
        ),
        /*min_remaining_percent*/ 25,
    ));
    assert!(!snapshot_allows_startup(
        &snapshot(
            /*primary_used_percent*/ Some(10.0),
            /*secondary_used_percent*/ Some(75.1),
        ),
        /*min_remaining_percent*/ 25,
    ));
    assert!(snapshot_allows_startup(
        &snapshot(
            /*primary_used_percent*/ Some(74.9),
            /*secondary_used_percent*/ Some(74.9),
        ),
        /*min_remaining_percent*/ 25,
    ));
}

#[test]
fn startup_check_skips_when_limit_is_reached() {
    let mut snapshot = snapshot(
        /*primary_used_percent*/ Some(10.0),
        /*secondary_used_percent*/ Some(10.0),
    );
    snapshot.rate_limit_reached_type = Some(RateLimitReachedType::RateLimitReached);

    assert!(!snapshot_allows_startup(
        &snapshot, /*min_remaining_percent*/ 25,
    ));
}

#[test]
fn quota_selection_requires_explicit_codex_identity() {
    let codex = snapshot(Some(95.0), None);
    let mut other = snapshot(Some(10.0), None);
    other.limit_id = Some("other".to_string());
    for snapshots in [
        [codex.clone(), other.clone()],
        [other.clone(), codex.clone()],
    ] {
        let selected = codex_snapshot(&snapshots).unwrap();
        assert_eq!(selected, &codex);
        assert!(!snapshot_allows_startup(selected, 10));
    }
    assert!(codex_snapshot(&[other]).is_none());
    let mut anonymous = codex;
    anonymous.limit_id = None;
    assert!(codex_snapshot(&[anonymous]).is_none());
    assert!(codex_snapshot(&[]).is_none());
}
