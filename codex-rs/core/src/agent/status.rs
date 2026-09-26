use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;

use codex_agent_task_store::AgentStatusClaim;
use codex_agent_task_store::AgentTask;
use codex_agent_task_store::AssignmentAdmissionOrigin;

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
            codex_protocol::protocol::TurnAbortReason::Replaced => Some(AgentStatus::Errored(
                "The turn was replaced by another turn.".to_string(),
            )),
            codex_protocol::protocol::TurnAbortReason::ReviewEnded => Some(AgentStatus::Errored(
                "The review ended before the agent completed.".to_string(),
            )),
            codex_protocol::protocol::TurnAbortReason::InternalError => Some(AgentStatus::Errored(
                "The turn was aborted because of an internal error.".to_string(),
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
    let receipt = task.receipt.as_ref()?;
    let observed_status = observed_status
        .filter(|(attempt_id, _)| {
            *attempt_id == task.current_attempt.attempt_id && *attempt_id == receipt.attempt_id
        })
        .map(|(_, status)| status);
    let plain_message = matches!(
        task.assignment.admission_origin,
        AssignmentAdmissionOrigin::LegacyMessage { .. }
    );
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
    if !plain_message && !task.workspace_status.pending_gates.is_empty() {
        return Some(AgentStatus::Errored(format!(
            "durable typed task has pending gates: {}",
            task.workspace_status
                .pending_gates
                .iter()
                .map(|gate| format!("{gate:?}").to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    match receipt.status {
        AgentStatusClaim::Completed
            if matches!(
                observed_status,
                Some(AgentStatus::CompletedWithSurface { .. })
            ) =>
        {
            observed_status.cloned()
        }
        AgentStatusClaim::Completed if plain_message => {
            let evidence_note = if receipt.risks.is_empty() {
                ""
            } else {
                "\nUnresolved risks are retained in the task record."
            };
            Some(AgentStatus::Completed(Some(format!(
                "Agent-reported result (behavior unverified): {}{evidence_note}",
                receipt.summary
            ))))
        }
        AgentStatusClaim::Completed => Some(AgentStatus::Completed(Some(format!(
            "{}\n\nAgent-reported summary (not verification evidence): {}",
            task.completion_evidence_summary(),
            receipt.summary
        )))),
        AgentStatusClaim::NeedsMain | AgentStatusClaim::Blocked => {
            Some(receipt_error_status(receipt))
        }
        AgentStatusClaim::Failed | AgentStatusClaim::Violated | AgentStatusClaim::Abandoned => {
            Some(receipt_error_status(receipt))
        }
    }
}

fn receipt_error_status(receipt: &codex_agent_task_store::AgentReceipt) -> AgentStatus {
    let status = match receipt.status {
        AgentStatusClaim::Completed => "completed",
        AgentStatusClaim::NeedsMain => "needs_main",
        AgentStatusClaim::Blocked => "blocked",
        AgentStatusClaim::Failed => "failed",
        AgentStatusClaim::Violated => "violated",
        AgentStatusClaim::Abandoned => "abandoned",
    };
    AgentStatus::Errored(format!(
        "durable typed receipt status: {status}: {}",
        receipt.summary
    ))
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
    fn aborted_turns_preserve_interruption_and_explain_failures() {
        use codex_protocol::protocol::TurnAbortReason;
        use codex_protocol::protocol::TurnAbortedEvent;

        for (reason, expected) in [
            (TurnAbortReason::Interrupted, AgentStatus::Interrupted),
            (TurnAbortReason::BudgetLimited, AgentStatus::Interrupted),
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

    fn typed_task_with_receipt(receipt_status: AgentStatusClaim, pending_gate: bool) -> AgentTask {
        let assignment_id = AssignmentId::new();
        let attempt_id = AttemptId::new();
        let now = chrono::Utc::now();
        AgentTask {
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
        for status in [AgentStatusClaim::NeedsMain, AgentStatusClaim::Blocked] {
            assert!(matches!(
                agent_status_from_task(&typed_task_with_receipt(status, false), None),
                Some(AgentStatus::Errored(message)) if message.contains("durable typed receipt status")
            ));
        }
        for status in [
            AgentStatusClaim::Failed,
            AgentStatusClaim::Violated,
            AgentStatusClaim::Abandoned,
        ] {
            assert!(matches!(
                agent_status_from_task(&typed_task_with_receipt(status, false), None),
                Some(AgentStatus::Errored(message)) if message.contains("durable typed receipt status")
            ));
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
    fn pending_gate_blocks_completed_receipt_projection() {
        assert!(matches!(
            agent_status_from_task(&typed_task_with_receipt(AgentStatusClaim::Completed, true), None),
            Some(AgentStatus::Errored(message)) if message.contains("pending gates")
        ));
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
            matches!(agent_status_from_task(&task, Some((attempt_id, &observed))), Some(AgentStatus::Errored(message)) if message.contains("pending gates"))
        );
    }
}
