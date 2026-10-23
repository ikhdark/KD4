//! Bind native command observations to the existing typed-task evidence owner.
//!
//! Requirements must name the exact resolved argv (JSON array or shell-quoted
//! argv), not merely describe a similar check. Public command schemas carry no
//! coverage claims. Historical observations add no result-reuse or source-snapshot
//! authority.
use crate::tools::context::ToolInvocation;
use codex_agent_task_store::AgentTaskBinding;
use codex_agent_task_store::ValidationCall;
use codex_agent_task_store::ValidationCallStatus;
use codex_agent_task_store::ValidationEvidence;
use codex_tools::ToolOutput;
use codex_tools::ToolOutputOutcome;
use serde_json::Value;
use serde_json::json;
use std::time::Instant;

/// Retry evidence delivery, never process execution. Slots are root-tree owned
/// and serialize only observations of the same native execution identity.
#[derive(Default)]
pub(crate) struct DeliveryState {
    launch: Option<ValidationCall>,
    terminal: Option<ValidationCall>,
    retry: bool,
    #[cfg(test)]
    pub(crate) fail_next_write: bool,
}

impl DeliveryState {
    async fn flush(&mut self, store: &codex_agent_task_store::LocalAgentTaskStore, registered: bool) -> Result<(), String> {
        self.retry = true;
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_write) {
            return Err("injected transient validation write failure".into());
        }
        if !registered && let Some(launch) = &self.launch {
            store.record_validation_call(launch.clone()).await.map_err(|error| error.to_string())?;
        }
        if let Some(terminal) = &self.terminal {
            store.record_validation_call(terminal.clone()).await.map_err(|error| error.to_string())?;
        }
        self.retry = false;
        Ok(())
    }
}

async fn retry_delivery(invocation: &ToolInvocation, binding: &AgentTaskBinding) {
    let coordinator = invocation.session.services.agent_control.task_coordinator();
    let Some(store) = coordinator.store() else { return };
    let pending = coordinator.native_validation_deliveries.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter().map(|(id, slot)| (id.clone(), std::sync::Arc::clone(slot))).collect::<Vec<_>>();
    for (id, slot) in pending {
        // Never wait for another live poll just to retry unrelated delivery.
        let Ok(mut delivery) = slot.try_lock() else { continue };
        if !delivery.retry || delivery.launch.as_ref().is_none_or(|call| call.attempt_id != binding.attempt_id) {
            continue;
        }
        let registered = match store.get_validation_call(id.clone()).await {
            Ok(Some(existing)) if existing.status.is_terminal() => {
                if delivery.launch.as_ref().is_some_and(|launch|
                    launch.attempt_id == existing.attempt_id && launch.command_summary == existing.command_summary)
                {
                    coordinator.native_validation_deliveries.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
                }
                continue;
            }
            Ok(existing) => existing.is_some(),
            Err(error) => {
                tracing::warn!(%error, "native validation delivery retry could not query storage");
                continue;
            }
        };
        match delivery.flush(store.as_ref(), registered).await {
            Ok(()) if delivery.terminal.is_some() => {
                coordinator.native_validation_deliveries.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
            }
            Ok(()) => {}
            Err(error) => tracing::warn!(%error, "native validation evidence delivery remains pending"),
        }
    }
}

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
        retry_delivery(invocation, &binding).await;
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
            .filter(|requirement| codex_agent_task_store::required_command_argv(requirement).as_ref() == Some(&argv))
            .collect::<Vec<_>>();
        let [requirement] = matching.as_slice() else {
            return Ok(None);
        };
        let origin = output.post_tool_use_id(&invocation.call_id);
        let call_id = format!("command:{}:{origin}", invocation.session.thread_id);
        let slot = {
            let mut deliveries = coordinator.native_validation_deliveries.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::sync::Arc::clone(deliveries.entry(call_id.clone()).or_default())
        };
        // No coordinator/map/source lock is held across storage. A stalled
        // write delays only this execution's delivery, not unrelated commands.
        let mut delivery = slot.lock().await;
        let existing = store
            .get_validation_call(call_id.clone())
            .await
            .map_err(|e| e.to_string())?;
        let registered = existing.is_some();
        let mut call = if let Some(call) = existing.or_else(|| delivery.launch.clone()) {
            if call.attempt_id != self.binding.attempt_id || call.command_summary != **requirement {
                return Err(
                    "the original execution belongs to another attempt or obligation".into(),
                );
            }
            if call.status.is_terminal() {
                coordinator.native_validation_deliveries.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner).remove(&call_id);
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
                coordinator.native_validation_deliveries.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner).remove(&call_id);
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
            call
        };
        delivery.launch.get_or_insert_with(|| call.clone());
        if let Some(terminal) = delivery.terminal.clone() {
            delivery.flush(store.as_ref(), registered).await?;
            coordinator.native_validation_deliveries.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner).remove(&call_id);
            return Ok(Some(json!({"call_id": call_id, "status": terminal.status})));
        }
        if output.outcome_for_logging() == ToolOutputOutcome::Yielded {
            delivery.flush(store.as_ref(), registered).await?;
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
        // Public command schemas intentionally do not accept coverage claims.
        // Preserve the successful native observation without inventing paths or
        // weakening the separate, path-bearing public ValidationResult contract.
        call.evidence.validation_result = Some(json!({
            "kind": "native_command_execution_v1",
            "argv": argv,
            "callId": call_id,
            "status": if proved { "succeeded" }
                else if call.status == ValidationCallStatus::Failed { "failed" }
                else { "unverified" },
            "durationMs": signal["process_duration_ms"].as_u64().or_else(||
                (invocation.tool_name.name != "write_stdin").then(||
                    u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX))),
        }));
        call.evidence.output_summary = signal["semantic_evidence"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                signal["semantic_evidence"]
                    .as_array()?
                    .iter()
                    .map(Value::as_str)
                    .collect::<Option<Vec<_>>>()
                    .map(|lines| lines.join("\n"))
            });
        call.evidence.retained_output_ref = signal["native_output_artifact_id"].as_str().map(str::to_owned);
        if call.status == ValidationCallStatus::Failed
            && let Some(result) = call.evidence.validation_result.as_mut()
        {
            result["failureExcerpt"] = json!(call.evidence.output_summary);
        }
        call.recorded_at = chrono::Utc::now().max(call.recorded_at);
        delivery.terminal = Some(call.clone());
        delivery.flush(store.as_ref(), registered).await?;
        coordinator.native_validation_deliveries.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner).remove(&call_id);
        Ok(Some(json!({
            "call_id": call_id, "status": call.status,
            "workspace_id": task.assignment.workspace_id,
            "freshness": "unverified", "behavioral_coverage": "unverified",
        })))
    }
}

fn verified_success(signal: &Value, outcome: ToolOutputOutcome) -> bool {
    outcome == ToolOutputOutcome::Success
        && signal["command_validation"]["proof"] == true
        && signal["validation_admission_pending"] != true
        && (signal["command_validation"]["tests"] != true
            || (signal["runner_execution_receipt"]["exit_code"] == 0
                && signal["runner_execution_receipt"]["executed_tests"]
                    .as_u64()
                    .is_some_and(|count| count > 0))
            || (signal["test_execution_observation"]["kind"]
                == "producer_test_execution_observation_v1"
                && signal["test_execution_observation"]["exit_code"] == 0
                && signal["test_execution_observation"]["executed_tests"]
                    .as_u64()
                    .is_some_and(|count| count > 0)))
}
