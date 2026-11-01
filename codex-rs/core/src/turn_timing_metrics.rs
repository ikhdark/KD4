//! Session-facing metric emission. Clock and observation state live in codex-turn-timing.
pub(crate) use codex_turn_timing::*;
use crate::session::turn_context::TurnContext;
use crate::ResponseEvent;
use codex_protocol::items::TurnItem;
use codex_otel::TURN_TTFM_DURATION_METRIC;

pub(crate) fn record_turn_ttft_metric(turn_context: &TurnContext, event: &ResponseEvent) {
    let Some(duration) = turn_context
        .turn_timing_state
        .record_response_event_milestones(event)
    else {
        return;
    };
    turn_context.session_telemetry.record_turn_ttft(duration);
}

pub(crate) fn record_turn_ttfm_metric(turn_context: &TurnContext, item: &TurnItem) {
    let Some(duration) = turn_context
        .turn_timing_state
        .record_ttfm_for_turn_item(item)
    else {
        return;
    };
    turn_context
        .session_telemetry
        .record_duration(TURN_TTFM_DURATION_METRIC, duration, &[]);
}
