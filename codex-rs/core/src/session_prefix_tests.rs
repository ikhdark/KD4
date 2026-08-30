use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_utils_output_truncation::approx_token_count;

use super::COMPLETION_MESSAGE_MAX_TOKENS;
use super::ERROR_NEXT_ACTION;
use super::TYPED_COMPLETION_NEXT_ACTION;
use super::format_inter_agent_completion_message;
use super::format_subagent_notification_message;

#[test]
fn error_completion_message_stays_below_manual_review_threshold() {
    let message = format_inter_agent_completion_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
        &AgentStatus::Errored("stream disconnected ".repeat(1_000)),
    )
    .expect("error status should produce a completion message");

    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
    assert!(message.contains(ERROR_NEXT_ACTION));
}

#[test]
fn over_truncation_error_completion_points_to_durable_exact_error() {
    let message = format_inter_agent_completion_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
        &AgentStatus::Errored(format!(
            "{}ROOT_CAUSE_AT_END",
            "transient wrapper: ".repeat(2_000)
        )),
    )
    .expect("error status should produce a completion message");

    assert!(message.contains("get_agent_task"));
    assert!(message.contains("full sealed error"));
    assert!(message.contains("assignment id returned by spawn_agent"));
    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
}

#[test]
fn legacy_error_completion_message_stays_below_manual_review_threshold() {
    let message =
        format_subagent_notification_message("worker", &AgentStatus::Errored("\0".repeat(10_000)));

    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
}

#[test]
fn typed_completion_message_stays_bounded_and_points_to_durable_receipt() {
    let message = format_inter_agent_completion_message(
        AgentPath::try_from("/root/architect").expect("valid task path"),
        AgentPath::try_from("/root/architect").expect("valid agent path"),
        &AgentStatus::Completed(Some("architecture contract ".repeat(10_000))),
    )
    .expect("completed status should produce a completion message");

    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
    assert!(message.contains(TYPED_COMPLETION_NEXT_ACTION));
}
