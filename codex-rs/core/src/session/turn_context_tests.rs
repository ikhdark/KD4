use super::*;

#[test]
fn long_running_progress_requires_commentary_after_one_minute() {
    let started_at = Instant::now();
    let mut state = LongRunningProgressState::new(started_at);

    assert!(!state.mark_required_if_due(started_at + Duration::from_secs(59)));
    assert!(state.mark_required_if_due(started_at + Duration::from_secs(60)));

    state.record_commentary(started_at + Duration::from_secs(61));
    assert!(!state.mark_required_if_due(started_at + Duration::from_secs(120)));
    assert!(state.mark_required_if_due(started_at + Duration::from_secs(121)));
}
