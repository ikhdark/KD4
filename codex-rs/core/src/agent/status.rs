use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;

use codex_agent_task_store::AgentStatusClaim;
use codex_agent_task_store::AgentTask;
use codex_agent_task_store::AssignmentAdmissionOrigin;

pub(crate) const NO_COMPLETION_DIAGNOSTICS: &str = "Host terminal boundary: no completion diagnostics recorded; this is not verification evidence.";

/// Derive the next agent status from a single emitted event.
/// Returns `None` when the event does not affect status tracking.
pub(crate) fn agent_status_from_event(msg: &EventMsg) -> Option<AgentStatus> {
    match msg {
        EventMsg::TurnStarted(_) => Some(AgentStatus::Running),
        EventMsg::TurnComplete(ev) => {
            let status = if let Some(error) = ev.error.as_ref() {
                AgentStatus::Errored(error.message.clone())
            } else if let Some(stop_cause) = ev.timing.as_ref().and_then(|timing| timing.stop_cause)
            {
                AgentStatus::Stopped {
                    last_agent_message: ev.last_agent_message.clone(),
                    stop_cause,
                }
            } else {
                match ev.surfaced_result.clone() {
                    Some(surfaced_result) => AgentStatus::CompletedWithSurface {
                        last_agent_message: ev.last_agent_message.clone(),
                        surfaced_result,
                    },
                    None => AgentStatus::Completed(ev.last_agent_message.clone()),
                }
            };
            Some(status)
        }
        EventMsg::TurnAborted(ev) => match ev.reason {
            codex_protocol::protocol::TurnAbortReason::Interrupted
            | codex_protocol::protocol::TurnAbortReason::BudgetLimited
            | codex_protocol::protocol::TurnAbortReason::SessionShutdown => {
                Some(AgentStatus::Interrupted)
            }
            codex_protocol::protocol::TurnAbortReason::Replaced => Some(AgentStatus::Errored(
                "The turn was replaced by another turn.".to_string(),
            )),
            codex_protocol::protocol::TurnAbortReason::ReviewEnded => Some(AgentStatus::Errored(
                "The review ended before the agent completed.".to_string(),
            )),
            codex_protocol::protocol::TurnAbortReason::InternalError => Some(AgentStatus::Errored(
                "The turn was aborted because of an internal error.".to_string(),
            )),
            codex_protocol::protocol::TurnAbortReason::ProcessLost => Some(AgentStatus::Errored(
                "The process was lost before the turn completed.".to_string(),
            )),
        },
        EventMsg::Error(ev) if ev.affects_turn_status() => {
            Some(AgentStatus::Errored(ev.message.clone()))
        }
        // A rejected steer or rollback request leaves the active turn and its status unchanged.
        EventMsg::Error(_) => None,
        EventMsg::ShutdownComplete => Some(AgentStatus::Shutdown),
        _ => None,
    }
}

/// Projects the durable typed-task outcome used by parent notifications.
pub(crate) fn agent_status_from_task(
    task: &AgentTask,
    observed_status: Option<(codex_agent_task_store::AttemptId, &AgentStatus)>,
) -> Option<AgentStatus> {
    let mut status = agent_status_from_task_inner(task, observed_status)?;
    let current_observation =
        observed_status.filter(|(attempt, _)| *attempt == task.current_attempt.attempt_id);
    let cause = match current_observation {
        Some((_, AgentStatus::Stopped { stop_cause, .. })) => Some(*stop_cause),
        Some(_) => None, // A later live outcome must not revive an older capacity stop.
        None => task
            .observations
            .iter()
            .rev()
            .find(|observation| {
                observation.attempt_id == task.current_attempt.attempt_id
                    && observation.kind == codex_agent_task_store::ObservationKind::HostCompletion
                    && observation
                        .call_id
                        .as_deref()
                        .is_some_and(|origin| !origin.ends_with(":parent-delivery"))
            })
            .and_then(|observation| observation.execution_stop_cause.as_ref())
            .and_then(|code| serde_json::from_value(serde_json::Value::String(code.clone())).ok()),
    };
    if let Some(stop_cause) = cause {
        let last_agent_message = match status {
            AgentStatus::Completed(message)
            | AgentStatus::Stopped {
                last_agent_message: message,
                ..
            }
            | AgentStatus::CompletedWithSurface {
                last_agent_message: message,
                ..
            } => message,
            AgentStatus::Errored(message) => Some(message),
            _ => None,
        };
        status = AgentStatus::Stopped {
            last_agent_message,
            stop_cause,
        };
    }
    Some(status)
}

/// Only the newest terminal origin and its delivery recovery apply. A later
/// clean terminal marker supersedes older diagnostics even within one attempt.
pub(crate) fn completion_diagnostics_from_task(task: &AgentTask) -> Option<String> {
    let observations = || {
        task.observations.iter().filter(|observation| {
            observation.attempt_id == task.current_attempt.attempt_id
                && observation.kind == codex_agent_task_store::ObservationKind::HostCompletion
        })
    };
    let terminal = observations().rev().find(|observation| {
        observation
            .call_id
            .as_deref()
            .is_some_and(|origin| !origin.ends_with(":parent-delivery"))
    });
    let Some(terminal) = terminal else {
        return observations().next_back().map(|observation| {
            format!(
                "Terminal diagnostic provenance is incomplete. {}",
                observation.summary,
            )
        });
    };
    let delivery_origin = format!("{}:parent-delivery", terminal.call_id.as_deref()?);
    let mut summaries = Vec::new();
    if terminal.summary != NO_COMPLETION_DIAGNOSTICS {
        summaries.push(terminal.summary.as_str());
    }
    if let Some(delivery) = observations()
        .rev()
        .find(|observation| observation.call_id.as_deref() == Some(delivery_origin.as_str()))
    {
        summaries.push(&delivery.summary);
    }
    (!summaries.is_empty()).then(|| summaries.join("\n\n"))
}

/// The current attempt's terminal host observation, selected exactly as in
/// `completion_diagnostics_from_task`. Older terminal statuses and receipts may
/// carry this text as a prefix.
fn terminal_host_observation_summary(task: &AgentTask) -> Option<&str> {
    task.observations
        .iter()
        .rev()
        .find(|observation| {
            observation.attempt_id == task.current_attempt.attempt_id
                && observation.kind == codex_agent_task_store::ObservationKind::HostCompletion
                && observation
                    .call_id
                    .as_deref()
                    .is_some_and(|origin| !origin.ends_with(":parent-delivery"))
        })
        .map(|observation| observation.summary.as_str())
        .filter(|summary| !summary.is_empty() && *summary != NO_COMPLETION_DIAGNOSTICS)
}

/// Removes only an exact leading copy of a legacy host observation.
fn without_host_observation<'a>(message: &'a str, summary: Option<&str>) -> &'a str {
    match summary.and_then(|summary| message.strip_prefix(summary)) {
        Some("") => "",
        Some(rest) => rest.strip_prefix("\n\n").unwrap_or(message),
        None => message,
    }
}

fn agent_status_from_task_inner(
    task: &AgentTask,
    observed_status: Option<(codex_agent_task_store::AttemptId, &AgentStatus)>,
) -> Option<AgentStatus> {
    let receipt = task.receipt.as_ref()?;
    if receipt.attempt_id != task.current_attempt.attempt_id {
        return None;
    }
    // Strip legacy host decorations without altering the authored report.
    let host_observation = terminal_host_observation_summary(task);
    let receipt_summary = without_host_observation(&receipt.summary, host_observation);
    let plain_message = matches!(
        task.assignment.admission_origin,
        AssignmentAdmissionOrigin::LegacyMessage { .. }
    );
    let needs_main = task.current_attempt.state == codex_agent_task_store::AttemptState::NeedsMain;
    if receipt.status == AgentStatusClaim::Completed && needs_main {
        return Some(AgentStatus::Errored(format!("Agent needs main: {receipt_summary}")));
    }
    let observed_status = observed_status
        .filter(|(attempt_id, _)| {
            *attempt_id == task.current_attempt.attempt_id && *attempt_id == receipt.attempt_id
        })
        .map(|(_, status)| status);
    // Preparation can fail before a follow-up gets a new attempt. Its live failure
    // must not be replaced by the previous turn's durable completed receipt.
    if plain_message
        && matches!(
            observed_status,
            Some(AgentStatus::Errored(_) | AgentStatus::Shutdown | AgentStatus::Interrupted)
        )
    {
        return observed_status.cloned();
    }
    // Keep the already-produced report instead of making the parent wake a sealed
    // worker to reconstruct it. The attempt check above still excludes stale text,
    // and neither this prose nor a pending gate is promoted into verification.
    let reported_message = match (receipt.status, observed_status) {
        (
            AgentStatusClaim::Completed,
            Some(
                AgentStatus::Completed(Some(message))
                | AgentStatus::CompletedWithSurface {
                    last_agent_message: Some(message),
                    ..
                },
            ),
        ) => without_host_observation(message, host_observation),
        _ => receipt_summary,
    };
    match receipt.status {
        AgentStatusClaim::Completed
            if matches!(
                observed_status,
                Some(AgentStatus::CompletedWithSurface { .. })
            ) =>
        {
            let mut status = observed_status.cloned();
            if let Some(AgentStatus::CompletedWithSurface {
                last_agent_message: Some(message),
                ..
            }) = status.as_mut()
            {
                *message = without_host_observation(message.as_str(), host_observation).to_string();
            }
            status
        }
        AgentStatusClaim::Completed => Some(AgentStatus::Completed(Some(reported_message.to_string()))),
        AgentStatusClaim::NeedsMain | AgentStatusClaim::Blocked => {
            Some(receipt_error_status(receipt, receipt_summary))
        }
        AgentStatusClaim::Failed | AgentStatusClaim::Violated | AgentStatusClaim::Abandoned => {
            Some(receipt_error_status(receipt, receipt_summary))
        }
    }
}

fn receipt_error_status(
    receipt: &codex_agent_task_store::AgentReceipt,
    summary: &str,
) -> AgentStatus {
    let status = match receipt.status {
        AgentStatusClaim::Completed => "completed",
        AgentStatusClaim::NeedsMain => "needs_main",
        AgentStatusClaim::Blocked => "blocked",
        AgentStatusClaim::Failed => "failed",
        AgentStatusClaim::Violated => "violated",
        AgentStatusClaim::Abandoned => "abandoned",
    };
    AgentStatus::Errored(format!("durable typed receipt status: {status}: {summary}"))
}

pub(crate) fn is_final(status: &AgentStatus) -> bool {
    !matches!(
        status,
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted
    )
}

