use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;

use codex_agent_task_store::AgentStatusClaim;
use codex_agent_task_store::AgentTask;

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

/// Projects the durable typed-task outcome used by parent notifications.
pub(crate) fn agent_status_from_task(task: &AgentTask) -> Option<AgentStatus> {
    let receipt = task.receipt.as_ref()?;
    if !task.workspace_status.pending_gates.is_empty() {
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
        AgentStatusClaim::Completed => Some(AgentStatus::Completed(Some(receipt.summary.clone()))),
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
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::SurfacedToolResult;
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
        assert_eq!(
            agent_status_from_task(&typed_task_with_receipt(AgentStatusClaim::Completed, false)),
            Some(AgentStatus::Completed(Some("durable summary".to_string())))
        );
        for status in [AgentStatusClaim::NeedsMain, AgentStatusClaim::Blocked] {
            assert!(matches!(
                agent_status_from_task(&typed_task_with_receipt(status, false)),
                Some(AgentStatus::Errored(message)) if message.contains("durable typed receipt status")
            ));
        }
        for status in [
            AgentStatusClaim::Failed,
            AgentStatusClaim::Violated,
            AgentStatusClaim::Abandoned,
        ] {
            assert!(matches!(
                agent_status_from_task(&typed_task_with_receipt(status, false)),
                Some(AgentStatus::Errored(message)) if message.contains("durable typed receipt status")
            ));
        }
    }

    #[test]
    fn pending_gate_blocks_completed_receipt_projection() {
        assert!(matches!(
            agent_status_from_task(&typed_task_with_receipt(AgentStatusClaim::Completed, true)),
            Some(AgentStatus::Errored(message)) if message.contains("pending gates")
        ));
    }
}
