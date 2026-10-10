use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_utils_output_truncation::approx_token_count;

use super::COMPLETION_MESSAGE_MAX_TOKENS;
use super::ERROR_NEXT_ACTION;
use super::TYPED_COMPLETION_NEXT_ACTION;
use super::format_inter_agent_completion_message;
use super::format_subagent_notification_message;
use super::inter_agent_completion_communication;

#[test]
fn error_completion_identity_preserves_producer_attempt_and_full_error() {
    let thread = codex_protocol::ThreadId::new();
    let make = |thread, sender: &str, receipt, error: &str| {
        inter_agent_completion_communication(
            AgentPath::root(), AgentPath::try_from(sender).unwrap(), thread,
            &AgentStatus::Errored(error.to_string()), receipt,
        ).unwrap()
    };
    let original = make(thread, "/root/worker", Some("attempt-1"), "failed");
    assert!(original.id.is_some());
    assert_eq!(original, make(thread, "/root/worker", Some("attempt-1"), "failed"));
    for changed in [
        make(thread, "/root/worker", Some("attempt-2"), "failed"),
        make(thread, "/root/other", Some("attempt-1"), "failed"),
        make(codex_protocol::ThreadId::new(), "/root/worker", Some("attempt-1"), "failed"),
        make(thread, "/root/worker", Some("attempt-1"), "different failure"),
    ] {
        assert_ne!(original.id, changed.id);
    }
    let long_error = "same prefix ".repeat(5_000);
    assert_ne!(
        make(thread, "/root/worker", None, &format!("{long_error}cause-a")).id,
        make(thread, "/root/worker", None, &format!("{long_error}cause-b")).id,
        "bounded displays must not alias distinct full errors",
    );
    let success = inter_agent_completion_communication(
        AgentPath::root(), AgentPath::try_from("/root/worker").unwrap(), thread,
        &AgentStatus::Completed(Some("done".into())), None,
    ).unwrap();
    let hidden_a = make(thread, "/root/worker", None, &format!("{long_error}cause-a{long_error}"));
    let hidden_b = make(thread, "/root/worker", None, &format!("{long_error}cause-b{long_error}"));
    assert_eq!(hidden_a.content, hidden_b.content, "the distinct causes must be outside the bounded display");
    assert_ne!(hidden_a.id, hidden_b.id, "deduplication must use the full error, not its display");
    assert!(success.id.is_none(), "successful follow-ups are not suppressed");
}

#[test]
fn complete_error_does_not_require_fetching_the_same_error_again() {
    let message = format_inter_agent_completion_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
        &AgentStatus::Errored("missing required command".to_string()),
        None,
    )
    .expect("error status should produce a completion message");
    assert!(message.contains("Agent errored: missing required command"));
    assert!(!message.contains("get_agent_task"));
}

#[test]
fn control_character_error_completion_bounds_the_rendered_envelope() {
    let message = format_inter_agent_completion_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
        &AgentStatus::Errored("\0".repeat(10_000)),
        None,
    )
    .expect("error status should produce a completion message");
    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
    assert!(message.contains(ERROR_NEXT_ACTION));
}

#[test]
fn error_completion_message_stays_below_manual_review_threshold() {
    let message = format_inter_agent_completion_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
        &AgentStatus::Errored("stream disconnected ".repeat(1_000)),
        None,
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
        None,
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
    let body = message
        .strip_prefix("<subagent_notification>")
        .and_then(|body| body.strip_suffix("</subagent_notification>"))
        .expect("notification envelope must survive truncation");
    let notification: serde_json::Value =
        serde_json::from_str(body).expect("valid notification JSON");
    assert_eq!(notification["agent_path"], "worker");
    assert!(
        notification["status"]["errored"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
}

#[test]
fn typed_completion_message_stays_bounded_and_points_to_durable_receipt() {
    let message = format_inter_agent_completion_message(
        AgentPath::try_from("/root/architect").expect("valid task path"),
        AgentPath::try_from("/root/architect").expect("valid agent path"),
        &AgentStatus::Completed(Some("architecture contract ".repeat(10_000))),
        None,
    )
    .expect("completed status should produce a completion message");

    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
    assert!(message.contains(TYPED_COMPLETION_NEXT_ACTION));
}

#[test]
fn completion_preserves_exact_receipt_and_attempt_under_truncation() {
    let receipt = "Receipt: get_agent_task({\"assignment_id\":\"assignment-123\"})\nProducer attempt: attempt-456";
    let message = format_inter_agent_completion_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").unwrap(),
        &AgentStatus::Completed(Some("result ".repeat(10_000))),
        Some(receipt),
    )
    .unwrap();
    assert!(message.contains(receipt));
    assert!(!message.contains("returned by spawn_agent"));
    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
}
