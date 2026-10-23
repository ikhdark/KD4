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
            let mut status = if let Some(error) = ev.error.as_ref() {
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
            if let Some(summary) = ev
                .timing
                .as_ref()
                .and_then(|timing| timing.completion_assessment.as_ref())
                .and_then(|assessment| assessment.bounded_summary(&ev.turn_id))
            {
                append_host_observation(&mut status, &summary);
            }
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
    if let Some(summary) = completion_diagnostics_from_task(task) {
        append_host_observation(&mut status, &summary);
    }
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

fn append_host_observation(status: &mut AgentStatus, summary: &str) {
    let message = match status {
        // Host observations must not manufacture an authored final response:
        // the missing-receipt path distinguishes Completed(None) from Some.
        AgentStatus::Completed(Some(message)) => message,
        AgentStatus::CompletedWithSurface {
            last_agent_message: message,
            ..
        } => message.get_or_insert_with(String::new),
        AgentStatus::Stopped {
            last_agent_message: Some(message),
            ..
        } => message,
        AgentStatus::Errored(message) => message,
        _ => return,
    };
    // Only presentation deduplication: no prose is parsed into host authority.
    if !message.starts_with(summary) {
        // Put known host facts before potentially long authored prose so the
        // bounded notification cannot discard them merely because prose is long.
        *message = if message.is_empty() {
            summary.to_string()
        } else {
            format!("{summary}\n\n{message}")
        };
    }
}

/// The current attempt's terminal host observation, selected exactly as in
/// `completion_diagnostics_from_task`. The live terminal status, and a legacy
/// receipt sealed from it, already start with this text.
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

/// Removes only an exact leading copy of the host observation (as written by
/// `append_host_observation`); `agent_status_from_task` prepends it once.
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
    // The live status, and a receipt sealed from it, already start with the
    // terminal host observation; `agent_status_from_task` prepends it once.
    let host_observation = terminal_host_observation_summary(task);
    let receipt_summary = without_host_observation(&receipt.summary, host_observation);
    let plain_message = matches!(
        task.assignment.admission_origin,
        AssignmentAdmissionOrigin::LegacyMessage { .. }
    );
    let failures = task
        .gates
        .iter()
        .filter(|gate| {
            matches!(
                gate.status,
                codex_agent_task_store::GateStatus::Failed
                    | codex_agent_task_store::GateStatus::ChangesRequested
                    | codex_agent_task_store::GateStatus::Violated
            )
        })
        .map(|gate| format!("{:?}: {:?}: {}", gate.kind, gate.status, gate.reason))
        .collect::<Vec<_>>()
        .join("; ");
    let needs_main = task.current_attempt.state == codex_agent_task_store::AttemptState::NeedsMain;
    if receipt.status == AgentStatusClaim::Completed
        && (needs_main || (!plain_message && !failures.is_empty()))
    {
        let acceptance = if needs_main {
            "needs_main"
        } else {
            "blocked by gates"
        };
        return Some(AgentStatus::Errored(format!(
            "Agent execution completed; acceptance {acceptance}. {failures}\n\nAgent-reported result (not verification evidence): {}",
            receipt_summary
        )));
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
    if !plain_message
        && receipt.status == AgentStatusClaim::Completed
        && !task.workspace_status.pending_gates.is_empty()
    {
        // Execution finished; acceptance is still owned by the durable gates.
        // Do not surface a typed result or turn pending review into a failure.
        return Some(AgentStatus::Completed(Some(format!(
            "Agent execution completed; durable typed task has pending gates: {}\n\nAgent-reported result (not verification evidence): {reported_message}\n\nResolve the gates on assignment {}; do not rerun the agent merely to recover this report.",
            task.workspace_status
                .pending_gates
                .iter()
                .map(|gate| format!("{gate:?}").to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(", "),
            task.assignment.assignment_id,
        ))));
    }
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
        AgentStatusClaim::Completed if plain_message => {
            let evidence_note = if receipt.risks.is_empty() {
                ""
            } else {
                "\nUnresolved risks are retained in the task record."
            };
            Some(AgentStatus::Completed(Some(format!(
                "Agent-reported result (behavior unverified): {}{evidence_note}",
                receipt_summary
            ))))
        }
        AgentStatusClaim::Completed => Some(AgentStatus::Completed(Some(format!(
            "{}\n\nAgent-reported summary (not verification evidence): {}",
            task.completion_evidence_summary(),
            reported_message
        )))),
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_agent_task_store::AcceptanceCriterion;
    use codex_agent_task_store::AgentReceipt;
    use codex_agent_task_store::AgentRole;
    use codex_agent_task_store::Assignment;
    use codex_agent_task_store::AssignmentAdmissionOrigin;
    use codex_agent_task_store::AssignmentId;
    use codex_agent_task_store::Attempt;
    use codex_agent_task_store::AttemptId;
    use codex_agent_task_store::AttemptState;
    use codex_agent_task_store::CapabilityProfile;
    use codex_agent_task_store::IntegrationPlan;
    use codex_agent_task_store::WorkspaceStrategy;
    use codex_agent_task_store::WorkspaceTaskStatus;
    use codex_protocol::protocol::CodexErrorInfo;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::NonSteerableTurnKind;
    use codex_protocol::protocol::SurfacedToolResult;
    use codex_protocol::protocol::TurnCompleteEvent;

    #[test]
    fn durable_projection_aggregates_same_turn_facts_and_clears_older_diagnostics() {
        let mut task = typed_task_with_receipt(AgentStatusClaim::Completed, false);
        let observation =
            |attempt_id, origin: &str, summary: &str| codex_agent_task_store::RuntimeObservation {
                execution_stop_cause: None,
                event_id: codex_agent_task_store::MutationEventId::new(),
                wake_event_id: codex_agent_task_store::WakeEventId::new(),
                assignment_id: task.assignment.assignment_id,
                attempt_id,
                kind: codex_agent_task_store::ObservationKind::HostCompletion,
                summary: summary.into(),
                call_id: Some(origin.into()),
                created_at: chrono::Utc::now(),
            };
        task.observations = vec![
            observation(
                task.current_attempt.attempt_id,
                "thread:turn",
                "validation failed",
            ),
            observation(
                task.current_attempt.attempt_id,
                "thread:turn:parent-delivery",
                "delivery failed",
            ),
            observation(AttemptId::new(), "other:turn", "wrong attempt"),
        ];
        let summary = completion_diagnostics_from_task(&task).unwrap();
        assert_eq!(summary, "validation failed\n\ndelivery failed");
        assert!(
            matches!(agent_status_from_task(&task, None), Some(AgentStatus::Completed(Some(text)))
            if text.starts_with(&summary) && !text.contains("wrong attempt"))
        );
        task.observations.push(observation(
            task.current_attempt.attempt_id,
            "thread:resolved",
            NO_COMPLETION_DIAGNOSTICS,
        ));
        assert_eq!(completion_diagnostics_from_task(&task), None);
    }

    #[test]
    fn aborted_turns_preserve_interruption_and_explain_failures() {
        use codex_protocol::protocol::TurnAbortReason;
        use codex_protocol::protocol::TurnAbortedEvent;

        for (reason, expected) in [
            (TurnAbortReason::Interrupted, AgentStatus::Interrupted),
            (TurnAbortReason::BudgetLimited, AgentStatus::Interrupted),
            (TurnAbortReason::SessionShutdown, AgentStatus::Interrupted),
            (
                TurnAbortReason::Replaced,
                AgentStatus::Errored("The turn was replaced by another turn.".to_string()),
            ),
            (
                TurnAbortReason::ReviewEnded,
                AgentStatus::Errored("The review ended before the agent completed.".to_string()),
            ),
            (
                TurnAbortReason::InternalError,
                AgentStatus::Errored(
                    "The turn was aborted because of an internal error.".to_string(),
                ),
            ),
            (
                TurnAbortReason::ProcessLost,
                AgentStatus::Errored("The process was lost before the turn completed.".to_string()),
            ),
        ] {
            assert_eq!(
                agent_status_from_event(&EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: Some("turn-1".to_string()),
                    reason,
                    completed_at: None,
                    duration_ms: None,
                    timing: None,
                })),
                Some(expected)
            );
        }
    }

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

    #[test]
    fn only_turn_failing_errors_finalize_agent_status() {
        for codex_error_info in [
            CodexErrorInfo::ThreadRollbackFailed,
            CodexErrorInfo::ActiveTurnNotSteerable {
                turn_kind: NonSteerableTurnKind::Review,
            },
            CodexErrorInfo::ActiveTurnNotSteerable {
                turn_kind: NonSteerableTurnKind::Compact,
            },
        ] {
            assert_eq!(
                agent_status_from_event(&EventMsg::Error(ErrorEvent {
                    message: "request rejected while the turn continues".to_string(),
                    codex_error_info: Some(codex_error_info.clone()),
                })),
                None,
                "{codex_error_info:?} must not finalize a running agent"
            );
        }
        for codex_error_info in [None, Some(CodexErrorInfo::Other)] {
            let status = agent_status_from_event(&EventMsg::Error(ErrorEvent {
                message: "turn failed".to_string(),
                codex_error_info,
            }));
            assert_eq!(
                status,
                Some(AgentStatus::Errored("turn failed".to_string()))
            );
            assert!(is_final(status.as_ref().expect("errored status")));
        }
    }

    #[test]
    fn surfaced_result_survives_agent_status_conversion() {
        let surfaced_result = SurfacedToolResult {
            adapter: "owner".to_string(),
            value: serde_json::json!({"answer": 42}),
            canonical_message: None,
        };
        let status = agent_status_from_event(&EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-surfaced".to_string(),
            last_agent_message: Some("done".to_string()),
            surfaced_result: Some(surfaced_result.clone()),
            error: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
            timing: None,
        }));

        assert_eq!(
            status,
            Some(AgentStatus::CompletedWithSurface {
                last_agent_message: Some("done".to_string()),
                surfaced_result,
            })
        );
    }

    #[test]
    fn host_completion_diagnostics_survive_plain_and_surfaced_statuses() {
        let mut no_report = AgentStatus::Completed(None);
        append_host_observation(&mut no_report, "host-only evidence");
        assert_eq!(no_report, AgentStatus::Completed(None));
        for surfaced in [false, true] {
            let event = TurnCompleteEvent {
                turn_id: "diagnostic-turn".into(),
                last_agent_message: Some("done".into()),
                surfaced_result: surfaced.then(|| SurfacedToolResult {
                    adapter: "owner".into(),
                    value: serde_json::json!(42),
                    canonical_message: None,
                }),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
                timing: Some(codex_protocol::protocol::TurnTiming {
                    completion_assessment: Some(
                        codex_protocol::protocol::TurnCompletionAssessment {
                            failed_checks: vec!["check-alpha failed".into()],
                            verification_gaps: vec!["check-beta awaits terminal evidence".into()],
                            ..Default::default()
                        },
                    ),
                    ..Default::default()
                }),
            };
            let status = agent_status_from_event(&EventMsg::TurnComplete(event)).unwrap();
            let mut task = typed_task_with_receipt(AgentStatusClaim::Completed, false);
            let projected =
                agent_status_from_task(&task, Some((task.current_attempt.attempt_id, &status)))
                    .unwrap();
            let text = match projected {
                AgentStatus::Completed(Some(message))
                | AgentStatus::CompletedWithSurface {
                    last_agent_message: Some(message),
                    ..
                } => message,
                _ => panic!("completion lifecycle must be unchanged"),
            };
            assert!(text.contains("check-alpha failed"));
            assert!(text.contains("check-beta awaits terminal evidence"));
            assert!(text.contains("diagnostic-turn"));
            task.workspace_status.pending_gates = vec![codex_agent_task_store::GateKind::Review];
            assert!(
                matches!(agent_status_from_task(&task, Some((task.current_attempt.attempt_id, &status))),
                Some(AgentStatus::Completed(Some(message))) if message.contains("pending gates") && message.contains("check-alpha"))
            );
            assert!(
                matches!(agent_status_from_task(&task, Some((AttemptId::new(), &status))),
                Some(AgentStatus::Completed(Some(message))) if !message.contains("check-alpha"))
            );
        }
    }

    #[test]
    fn audit115_capacity_cause_survives_event_and_durable_task_projection() {
        use codex_protocol::protocol::TurnCompleteEvent;
        use codex_protocol::protocol::TurnExecutionStopCause;
        use codex_protocol::protocol::TurnTiming;
        let cause = TurnExecutionStopCause::SubagentContextBudget;
        for report in [None, Some("One deliverable done; two remain.".to_owned())] {
            let event = EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "capacity-turn".into(),
                last_agent_message: report.clone(),
                surfaced_result: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
                timing: Some(TurnTiming {
                    stop_cause: Some(cause),
                    ..Default::default()
                }),
            });
            let replay: EventMsg =
                serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
            assert_eq!(
                agent_status_from_event(&replay),
                Some(AgentStatus::Stopped {
                    last_agent_message: report,
                    stop_cause: cause,
                })
            );
        }
        for accepted in [false, true] {
            let claim = if accepted {
                AgentStatusClaim::Completed
            } else {
                AgentStatusClaim::NeedsMain
            };
            let mut task = typed_task_with_receipt(claim, false);
            task.observations
                .push(codex_agent_task_store::RuntimeObservation {
                    execution_stop_cause: Some("subagent_context_budget".into()),
                    event_id: codex_agent_task_store::MutationEventId::new(),
                    wake_event_id: codex_agent_task_store::WakeEventId::new(),
                    assignment_id: task.assignment.assignment_id,
                    attempt_id: task.current_attempt.attempt_id,
                    kind: codex_agent_task_store::ObservationKind::HostCompletion,
                    summary: "Capacity stop; acceptance is independent.".into(),
                    call_id: Some("thread:turn".into()),
                    created_at: chrono::Utc::now(),
                });
            let cold: AgentTask =
                serde_json::from_value(serde_json::to_value(&task).unwrap()).unwrap();
            assert!(
                matches!(agent_status_from_task(&cold, None), Some(AgentStatus::Stopped {
                last_agent_message: Some(message), stop_cause,
            }) if stop_cause == cause && message.contains("durable summary"))
            );
            assert_eq!(
                cold.receipt.as_ref().unwrap().status,
                claim,
                "capacity alone must not downgrade an accepted receipt"
            );
            let mut later = cold;
            let mut clean = later.observations[0].clone();
            clean.execution_stop_cause = None;
            clean.call_id = Some("thread:later-clean".into());
            later.observations.push(clean);
            assert!(
                !matches!(
                    agent_status_from_task(&later, None),
                    Some(AgentStatus::Stopped { .. })
                ),
                "later clean outcomes supersede the scheduling cause"
            );
        }
    }

    fn typed_task_with_receipt(receipt_status: AgentStatusClaim, pending_gate: bool) -> AgentTask {
        let assignment_id = AssignmentId::new();
        let attempt_id = AttemptId::new();
        let now = chrono::Utc::now();
        AgentTask {
            dependency_admissions: Vec::new(),
            criterion_assessments: Vec::new(),
            assignment: Assignment {
                assignment_id,
                root_session_id: "root".to_string(),
                admission_origin: AssignmentAdmissionOrigin::Typed,
                repository_id: "repository".to_string(),
                workspace_id: "workspace".to_string(),
                role: AgentRole::Explorer,
                capability_profile: CapabilityProfile::ReadSearch,
                objective: "inspect".to_string(),
                acceptance_criteria: vec![AcceptanceCriterion {
                    evidence_policy: None,
                    id: "criterion".to_string(),
                    text: "report".to_string(),
                }],
                read_scope: Vec::new(),
                write_scope: Vec::new(),
                stop_condition: "done".to_string(),
                dependencies: Vec::new(),
                risk_hints: Vec::new(),
                required_evidence: Vec::new(),
                prohibited_changes: Vec::new(),
                contract_claims: Vec::new(),
                workspace_strategy: WorkspaceStrategy::Shared,
                start_epoch: 0,
                relation: None,
                architecture_contract_ref: None,
                integration_plan: IntegrationPlan::SingleWriter,
                benign_read_overlap_count: 0,
                task_capsule: None,
                created_at: now,
            },
            current_attempt: Attempt {
                attempt_id,
                assignment_id,
                ordinal: 0,
                amendment: None,
                state: if receipt_status == AgentStatusClaim::Completed {
                    AttemptState::Completed
                } else {
                    AttemptState::NeedsMain
                },
                created_at: now,
                sealed_at: Some(now),
            },
            gates: Vec::new(),
            receipt: Some(AgentReceipt {
                assignment_id,
                attempt_id,
                status: receipt_status,
                summary: "durable summary".to_string(),
                criterion_results: Vec::new(),
                declared_changes: Vec::new(),
                validation_call_ids: Vec::new(),
                blockers: Vec::new(),
                risks: Vec::new(),
                next_action: None,
                architecture_contract: None,
                evidence_epoch: 0,
                read_scope_fingerprint: None,
                sealed_at: now,
            }),
            validation_calls: Vec::new(),
            workspace_status: WorkspaceTaskStatus {
                pending_gates: pending_gate
                    .then_some(codex_agent_task_store::GateKind::Review)
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            isolation_handoff: None,
            integration_handoffs: Vec::new(),
            observations: Vec::new(),
        }
    }

    #[test]
    fn durable_receipt_status_controls_parent_completion() {
        let mut task = typed_task_with_receipt(AgentStatusClaim::Completed, false);
        task.receipt.as_mut().unwrap().summary =
            "Everything passed and Desktop is running the new build".to_string();
        let Some(AgentStatus::Completed(Some(message))) = agent_status_from_task(&task, None)
        else {
            panic!("sealed completed task must produce a parent notification");
        };
        assert!(message.contains("behavior unverified"));
        assert!(message.contains("Running Desktop build: not established"));
        assert!(message.starts_with("Recorded task status: Completed."));
        assert!(
            message
                .contains("Agent-reported summary (not verification evidence): Everything passed")
        );
        for (status, label) in [
            (AgentStatusClaim::NeedsMain, "needs_main"),
            (AgentStatusClaim::Blocked, "blocked"),
            (AgentStatusClaim::Failed, "failed"),
            (AgentStatusClaim::Violated, "violated"),
            (AgentStatusClaim::Abandoned, "abandoned"),
        ] {
            assert_eq!(
                agent_status_from_task(&typed_task_with_receipt(status, false), None),
                Some(AgentStatus::Errored(format!(
                    "durable typed receipt status: {label}: durable summary"
                )))
            );
        }
    }

    #[test]
    fn plain_message_completion_preserves_surface_without_claiming_verified_behavior() {
        let mut task = typed_task_with_receipt(AgentStatusClaim::Completed, true);
        task.assignment.admission_origin = AssignmentAdmissionOrigin::LegacyMessage {
            parent_assignment_id: None,
        };
        let observed = AgentStatus::CompletedWithSurface {
            last_agent_message: None,
            surfaced_result: SurfacedToolResult {
                adapter: "owner".to_string(),
                value: serde_json::json!({"result": "answer artifact"}),
                canonical_message: Some("answer artifact".to_string()),
            },
        };
        assert_eq!(
            agent_status_from_task(&task, Some((task.current_attempt.attempt_id, &observed))),
            Some(observed)
        );
        assert_eq!(
            agent_status_from_task(&task, None),
            Some(AgentStatus::Completed(Some(
                "Agent-reported result (behavior unverified): durable summary".to_string()
            )))
        );
        for failed_followup in [
            AgentStatus::Errored("follow-up preparation failed".to_string()),
            AgentStatus::Shutdown,
            AgentStatus::Interrupted,
        ] {
            assert_eq!(
                agent_status_from_task(
                    &task,
                    Some((task.current_attempt.attempt_id, &failed_followup))
                ),
                Some(failed_followup)
            );
        }
        task.receipt.as_mut().unwrap().status = AgentStatusClaim::Failed;
        assert!(matches!(
            agent_status_from_task(&task, None),
            Some(AgentStatus::Errored(_))
        ));
    }

    #[test]
    fn pending_gate_preserves_execution_completion_without_claiming_acceptance() {
        assert!(matches!(
            agent_status_from_task(&typed_task_with_receipt(AgentStatusClaim::Completed, true), None),
            Some(AgentStatus::Completed(Some(message))) if message.contains("pending gates")
        ));
        assert!(matches!(
            agent_status_from_task(&typed_task_with_receipt(AgentStatusClaim::Failed, true), None),
            Some(AgentStatus::Errored(message)) if message.contains("status: failed")
        ));
    }

    #[test]
    fn failed_acceptance_is_not_recovered_as_execution_success() {
        for kind in [
            codex_agent_task_store::GateKind::Review,
            codex_agent_task_store::GateKind::Verification,
        ] {
            let mut task = typed_task_with_receipt(AgentStatusClaim::Completed, false);
            task.current_attempt.state = AttemptState::NeedsMain;
            task.gates.push(codex_agent_task_store::AgentGate {
                assignment_id: task.assignment.assignment_id,
                kind,
                status: codex_agent_task_store::GateStatus::Failed,
                reason: "missing behavior".into(),
                waiver_reason: None,
                evidence_epoch: 0,
                updated_at: chrono::Utc::now(),
                sealed_at: Some(chrono::Utc::now()),
            });
            let observed = AgentStatus::Completed(Some("worker finished".into()));
            for live in [None, Some((task.current_attempt.attempt_id, &observed))] {
                assert!(
                    matches!(agent_status_from_task(&task, live), Some(AgentStatus::Errored(message))
                    if message.contains("execution completed") && message.contains("needs_main") && message.contains("missing behavior"))
                );
            }
            assert_eq!(
                task.receipt.as_ref().unwrap().status,
                AgentStatusClaim::Completed
            );
            // The first changes-requested review leaves execution Completed,
            // but acceptance is blocked until an explicit correction is reviewed.
            task.current_attempt.state = AttemptState::Completed;
            task.gates[0].status = codex_agent_task_store::GateStatus::ChangesRequested;
            assert!(
                matches!(agent_status_from_task(&task, None), Some(AgentStatus::Errored(message))
                if message.contains("blocked by gates") && message.contains("ChangesRequested"))
            );
        }
    }

    #[test]
    fn typed_completion_retains_attempt_bound_report_without_repeating_the_worker() {
        let mut task = typed_task_with_receipt(AgentStatusClaim::Completed, false);
        let observed = AgentStatus::Completed(Some("Detailed findings and coverage.".to_string()));
        let attempt_id = task.current_attempt.attempt_id;
        let Some(AgentStatus::Completed(Some(message))) =
            agent_status_from_task(&task, Some((attempt_id, &observed)))
        else {
            panic!("cleared gates allow completion");
        };
        assert!(message.contains("Detailed findings and coverage."));
        assert!(message.contains("not verification evidence"));
        assert!(message.contains("Running Desktop build: not established"));

        task.workspace_status.pending_gates = vec![codex_agent_task_store::GateKind::Review];
        let Some(AgentStatus::Completed(Some(message))) =
            agent_status_from_task(&task, Some((attempt_id, &observed)))
        else {
            panic!("report delivery must not clear pending gates");
        };
        assert!(message.contains("pending gates: review"));
        assert!(message.contains("Detailed findings and coverage."));
        assert!(message.contains("not verification evidence"));
        assert!(message.contains(&task.assignment.assignment_id.to_string()));

        let Some(AgentStatus::Completed(Some(stale))) =
            agent_status_from_task(&task, Some((AttemptId::new(), &observed)))
        else {
            panic!("stale report must not bypass pending gates");
        };
        assert!(!stale.contains("Detailed findings and coverage."));
        assert!(stale.contains("durable summary"));
    }

    #[test]
    fn typed_surface_requires_current_attempt_and_cleared_gates() {
        let mut task = typed_task_with_receipt(AgentStatusClaim::Completed, false);
        let observed = AgentStatus::CompletedWithSurface {
            last_agent_message: None,
            surfaced_result: SurfacedToolResult {
                adapter: "owner".to_string(),
                value: serde_json::json!({"answer": [1, 2, 3]}),
                canonical_message: None,
            },
        };
        let attempt_id = task.current_attempt.attempt_id;
        assert_eq!(
            agent_status_from_task(&task, Some((attempt_id, &observed))),
            Some(observed.clone())
        );
        assert!(matches!(
            agent_status_from_task(&task, Some((AttemptId::new(), &observed))),
            Some(AgentStatus::Completed(_))
        ));
        task.workspace_status =
            typed_task_with_receipt(AgentStatusClaim::Completed, true).workspace_status;
        assert!(
            matches!(agent_status_from_task(&task, Some((attempt_id, &observed))), Some(AgentStatus::Completed(Some(message))) if message.contains("pending gates"))
        );
    }

    #[test]
    fn audit149x_terminal_host_observation_is_reported_once_in_projected_completions() {
        let assessment = codex_protocol::protocol::TurnCompletionAssessment {
            verification_gaps: vec!["final workspace not verified".into()],
            ..Default::default()
        };
        let summary = assessment
            .bounded_summary("child-turn")
            .expect("nonempty host observation");
        let live = agent_status_from_event(&EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "child-turn".into(),
            last_agent_message: Some("authored report".into()),
            surfaced_result: None,
            error: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
            timing: Some(codex_protocol::protocol::TurnTiming {
                completion_assessment: Some(assessment),
                ..Default::default()
            }),
        }))
        .expect("terminal status");
        let AgentStatus::Completed(Some(live_message)) = live.clone() else {
            panic!("the live completion keeps its report");
        };
        assert!(live_message.starts_with(&summary));
        // The session records the same text as the attempt's terminal observation.
        let observation = |task: &AgentTask| codex_agent_task_store::RuntimeObservation {
            execution_stop_cause: None,
            event_id: codex_agent_task_store::MutationEventId::new(),
            wake_event_id: codex_agent_task_store::WakeEventId::new(),
            assignment_id: task.assignment.assignment_id,
            attempt_id: task.current_attempt.attempt_id,
            kind: codex_agent_task_store::ObservationKind::HostCompletion,
            summary: summary.clone(),
            call_id: Some("thread:child-turn".into()),
            created_at: chrono::Utc::now(),
        };
        let occurrences = |text: &str| text.matches(summary.as_str()).count();

        let mut typed = typed_task_with_receipt(AgentStatusClaim::Completed, false);
        let recorded = observation(&typed);
        typed.observations.push(recorded);
        let attempt_id = typed.current_attempt.attempt_id;
        let Some(AgentStatus::Completed(Some(message))) =
            agent_status_from_task(&typed, Some((attempt_id, &live)))
        else {
            panic!("typed completion keeps its lifecycle");
        };
        assert_eq!(occurrences(message.as_str()), 1, "{message}");
        assert!(message.starts_with(&summary));
        assert!(
            message.contains("Agent-reported summary (not verification evidence): authored report")
        );

        typed.workspace_status.pending_gates = vec![codex_agent_task_store::GateKind::Review];
        let Some(AgentStatus::Completed(Some(message))) =
            agent_status_from_task(&typed, Some((attempt_id, &live)))
        else {
            panic!("pending gates keep execution completion");
        };
        assert_eq!(occurrences(message.as_str()), 1, "{message}");
        assert!(
            message.contains("Agent-reported result (not verification evidence): authored report")
        );

        // A plain-message receipt sealed from the live status carries the same prefix.
        let mut plain = typed_task_with_receipt(AgentStatusClaim::Completed, false);
        plain.assignment.admission_origin = AssignmentAdmissionOrigin::LegacyMessage {
            parent_assignment_id: None,
        };
        plain.receipt.as_mut().unwrap().summary = live_message.clone();
        let recorded = observation(&plain);
        plain.observations.push(recorded);
        let Some(AgentStatus::Completed(Some(message))) = agent_status_from_task(&plain, None)
        else {
            panic!("plain completion keeps its lifecycle");
        };
        assert_eq!(occurrences(message.as_str()), 1, "{message}");
        assert!(message.contains("Agent-reported result (behavior unverified): authored report"));

        // Negative controls: without the durable observation the live report is
        // kept verbatim, and text that only begins like the observation is not cut.
        let unrecorded = typed_task_with_receipt(AgentStatusClaim::Completed, false);
        let Some(AgentStatus::Completed(Some(message))) = agent_status_from_task(
            &unrecorded,
            Some((unrecorded.current_attempt.attempt_id, &live)),
        ) else {
            panic!("typed completion keeps its lifecycle");
        };
        assert!(message.contains(&format!(
            "Agent-reported summary (not verification evidence): {live_message}"
        )));
        plain.receipt.as_mut().unwrap().summary =
            format!("{summary} continued without a separator");
        let Some(AgentStatus::Completed(Some(message))) = agent_status_from_task(&plain, None)
        else {
            panic!("plain completion keeps its lifecycle");
        };
        assert!(message.contains(&format!(
            "Agent-reported result (behavior unverified): {summary} continued without a separator"
        )));
    }
}
