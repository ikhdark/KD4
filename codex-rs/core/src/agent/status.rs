use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;

/// Derive the next agent status from a single emitted event.
/// Returns `None` when the event does not affect status tracking.
pub(crate) fn agent_status_from_event(msg: &EventMsg) -> Option<AgentStatus> {
    match msg {
        EventMsg::TurnStarted(_) => Some(AgentStatus::Running),
        EventMsg::TurnComplete(ev) => Some(if let Some(error) = ev.error.as_ref() {
            AgentStatus::Errored(error.message.clone())
        } else {
            match ev.surfaced_result.clone() {
                Some(surfaced_result) => AgentStatus::CompletedWithSurface {
                    last_agent_message: ev.last_agent_message.clone(),
                    surfaced_result,
                },
                None => AgentStatus::Completed(ev.last_agent_message.clone()),
            }
        }),
        EventMsg::TurnAborted(ev) => match ev.reason {
            codex_protocol::protocol::TurnAbortReason::Interrupted
            | codex_protocol::protocol::TurnAbortReason::BudgetLimited => {
                Some(AgentStatus::Interrupted)
            }
            _ => Some(AgentStatus::Errored(format!("{:?}", ev.reason))),
        },
        EventMsg::Error(ev) => Some(AgentStatus::Errored(ev.message.clone())),
        EventMsg::ShutdownComplete => Some(AgentStatus::Shutdown),
        _ => None,
    }
}

pub(crate) fn is_final(status: &AgentStatus) -> bool {
    !matches!(
        status,
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::TurnCompleteEvent;

    #[test]
    fn completion_with_embedded_error_is_errored() {
        let status = agent_status_from_event(&EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: Some("not successful".to_string()),
            surfaced_result: None,
            error: Some(ErrorEvent {
                message: "terminal failure".to_string(),
                codex_error_info: None,
            }),
            completion: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
            timing: None,
        }));

        assert_eq!(
            status,
            Some(AgentStatus::Errored("terminal failure".to_string()))
        );
    }
}
