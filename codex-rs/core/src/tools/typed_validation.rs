//! Bind native command observations to the existing typed-task evidence owner.
//!
//! Requirements must name the exact resolved argv (JSON array or shell-quoted
//! argv), not merely describe a similar check. Caller paths remain attribution,
//! not behavioral coverage. No result reuse or source-snapshot authority is added.
use crate::tools::context::ToolInvocation;
use codex_agent_task_store::AgentTaskBinding;
use codex_agent_task_store::ValidationCall;
use codex_agent_task_store::ValidationCallStatus;
use codex_agent_task_store::ValidationEvidence;
use codex_protocol::validation::ValidationResult;
use codex_protocol::validation::ValidationTerminalStatus;
use codex_tools::ToolOutput;
use codex_tools::ToolOutputOutcome;
use serde_json::Value;
use serde_json::json;
use std::time::Instant;

pub(super) struct Observation {
    binding: AgentTaskBinding,
    revision: u64,
    started: Instant,
    recorded_at: chrono::DateTime<chrono::Utc>,
}

impl Observation {
    pub(super) async fn capture(invocation: &ToolInvocation) -> Option<Self> {
        if invocation.tool_name.namespace.is_some()
            || !matches!(
                invocation.tool_name.name.as_str(),
                "exec_command" | "shell_command" | "write_stdin"
            )
        {
            return None;
        }
        let binding = invocation
            .session
            .services
            .agent_control
            .task_coordinator()
            .binding_for_turn(&invocation.step_context.turn)?;
        if binding.thread_id.as_deref() != Some(invocation.session.thread_id.to_string().as_str()) {
            return None;
        }
        Some(Self {
            binding,
            // The command's own evidence-only observation is not a source change.
            revision: invocation
                .tracker
                .lock()
                .await
                .current_validation_revision(),
            started: Instant::now(),
            recorded_at: chrono::Utc::now(),
        })
    }

    pub(super) async fn record(
        self,
        invocation: &ToolInvocation,
        output: &mut dyn ToolOutput,
    ) -> Option<String> {
        match self.record_inner(invocation, output).await {
            Ok(value) => value.map(|value| format!("Typed validation evidence: {value}")),
            Err(error) => Some(format!(
                "Typed validation evidence was not recorded: {error}. The command's actual outcome is unchanged; do not repeat it solely to repair evidence delivery."
            )),
        }
    }

    async fn record_inner(
        &self,
        invocation: &ToolInvocation,
        output: &mut dyn ToolOutput,
    ) -> Result<Option<Value>, String> {
        let Some(signal) = output.sampling_request_signal() else {
            return Ok(None);
        };
        let validation = &signal["command_validation"];
        if validation["validation"] != true || signal["validation_admission_pending"] == true {
            return Ok(None);
        }
        let context = &validation["execution_context"];
        let Some(argv) = context["command"].as_array().and_then(|values| {
            values
                .iter()
                .map(|value| value.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        }) else {
            return Ok(None);
        };
        let Some(primary) = invocation.step_context.environments.primary() else {
            return Ok(None);
        };
        // The local task store cannot establish a remote/secondary workspace's
        // authority. Neither an environment label nor a caller path is enough.
        if primary.environment.is_remote() || context["environment_id"] != primary.environment_id {
            return Ok(None);
        }
        let Ok(cwd) = primary.cwd().to_abs_path() else {
            return Ok(None);
        };
        let Some(actual_cwd) = context["cwd"].as_str().map(std::path::Path::new) else {
            return Ok(None);
        };
        if !actual_cwd.starts_with(cwd.as_path())
            || actual_cwd.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Ok(None);
        }
        let coordinator = invocation.session.services.agent_control.task_coordinator();
        let Some(store) = coordinator.store() else {
            return Ok(None);
        };
        let task = store
            .get_agent_task(self.binding.assignment_id, Some(0))
            .await
            .map_err(|e| e.to_string())?;
        let matching = task
            .assignment
            .required_evidence
            .iter()
            .filter(|requirement| exact_argv(requirement).as_ref() == Some(&argv))
            .collect::<Vec<_>>();
        let [requirement] = matching.as_slice() else {
            return Ok(None);
        };
        let origin = output.post_tool_use_id(&invocation.call_id);
        let call_id = format!("command:{}:{origin}", invocation.session.thread_id);
        let existing = store
            .get_validation_call(call_id.clone())
            .await
            .map_err(|e| e.to_string())?;
        let mut call = if let Some(call) = existing {
            if call.attempt_id != self.binding.attempt_id || call.command_summary != **requirement {
                return Err(
                    "the original execution belongs to another attempt or obligation".into(),
                );
            }
            if call.status.is_terminal() {
                return Ok(Some(
                    json!({"call_id": call.call_id, "status": call.status}),
                ));
            }
            call
        } else {
            // A later poll may settle an original record, never manufacture a
            // new attempt's evidence for a process launched before that attempt.
            if invocation.tool_name.name == "write_stdin"
                || task.current_attempt.attempt_id != self.binding.attempt_id
            {
                return Ok(None);
            }
            let call = ValidationCall {
                call_id: call_id.clone(),
                attempt_id: self.binding.attempt_id,
                command_summary: (*requirement).clone(),
                status: ValidationCallStatus::Running,
                recorded_at: self.recorded_at,
                evidence: ValidationEvidence {
                    validation_result: Some(json!({
                        "kind": "native_command_observation_v1", "context": context,
                        "turn_id": invocation.step_context.turn.sub_id, "mutation_revision": self.revision,
                    })),
                    ..Default::default()
                },
            };
            // Registration follows an actual native execution observation, not
            // command recognition/preflight. Busy and rejected calls never enter.
            store
                .record_validation_call(call.clone())
                .await
                .map_err(|e| e.to_string())?;
            call
        };
        if output.outcome_for_logging() == ToolOutputOutcome::Yielded {
            return Ok(Some(json!({"call_id": call_id, "status": "running"})));
        }
        let start = call
            .evidence
            .validation_result
            .as_ref()
            .ok_or("missing launch observation")?;
        let same_source = start["context"] == *context
            && start["turn_id"] == invocation.step_context.turn.sub_id
            && start["mutation_revision"].as_u64()
                == Some(
                    invocation
                        .tracker
                        .lock()
                        .await
                        .current_validation_revision(),
                );
        let cancelled = invocation.cancellation_token.is_cancelled();
        let proved =
            same_source && !cancelled && verified_success(&signal, output.outcome_for_logging());
        call.status = if cancelled {
            ValidationCallStatus::Cancelled
        } else if proved {
            ValidationCallStatus::Succeeded
        } else if output.outcome_for_logging() == ToolOutputOutcome::Failure {
            ValidationCallStatus::Failed
        } else {
            ValidationCallStatus::NotExecuted
        };
        let covered_paths = output
            .projection_metadata()
            .and_then(|metadata| {
                metadata.essential_inline["validation"]["covered_paths"]
                    .as_array()
                    .cloned()
            })
            .and_then(|values| {
                values
                    .into_iter()
                    .map(serde_json::from_value::<String>)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()
            })
            .unwrap_or_default();
        let result = ValidationResult {
            argv,
            covered_paths,
            call_id: call_id.clone(),
            process_id: None,
            status: if proved { ValidationTerminalStatus::Succeeded }
                else if call.status == ValidationCallStatus::Failed { ValidationTerminalStatus::Failed }
                else { ValidationTerminalStatus::Unverified },
            duration_ms: self.started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            summary: Some("Native command observation; declared paths are attribution only, source freshness and behavioral coverage remain unverified.".into()),
            failure_excerpt: None,
            raw_artifact_ref: None,
            raw_artifact_sha256: None,
        };
        // The public result validator also rejects malformed/empty path scope.
        // Never let a successful exit manufacture valid task proof from it.
        let encoded = serde_json::to_value(result).map_err(|e| e.to_string())?;
        if serde_json::from_value::<ValidationResult>(encoded.clone()).is_err() {
            call.status = ValidationCallStatus::NotExecuted;
            call.evidence.validation_result = None;
        } else {
            call.evidence.validation_result = Some(encoded);
        }
        call.recorded_at = chrono::Utc::now().max(call.recorded_at);
        store
            .record_validation_call(call.clone())
            .await
            .map_err(|e| e.to_string())?;
        Ok(Some(json!({
            "call_id": call_id, "status": call.status,
            "workspace_id": task.assignment.workspace_id,
            "freshness": "unverified", "behavioral_coverage": "unverified",
        })))
    }
}

fn exact_argv(requirement: &str) -> Option<Vec<String>> {
    serde_json::from_str(requirement)
        .ok()
        .or_else(|| shlex::split(requirement))
}

fn verified_success(signal: &Value, outcome: ToolOutputOutcome) -> bool {
    outcome == ToolOutputOutcome::Success
        && signal["command_validation"]["proof"] == true
        && signal["validation_admission_pending"] != true
        && (signal["command_validation"]["tests"] != true
            || (signal["runner_execution_receipt"]["exit_code"] == 0
                && signal["runner_execution_receipt"]["executed_tests"]
                    .as_u64()
                    .is_some_and(|count| count > 0)))
}
