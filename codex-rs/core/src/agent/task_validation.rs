//! Host-owned validation receipts for exact typed-assignment obligations.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use chrono::Utc;
use codex_agent_task_store::AssignmentAdmissionOrigin;
use codex_agent_task_store::LocalAgentTaskStore;
use codex_agent_task_store::ValidationCall;
use codex_agent_task_store::ValidationCallStatus;
use codex_agent_task_store::ValidationEvidence;
use codex_protocol::validation::ValidationCommandContext;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

pub(crate) struct TaskValidation {
    store: Arc<LocalAgentTaskStore>,
    call: Option<ValidationCall>,
    argv: Vec<String>,
    started: Instant,
    heartbeat: tokio::task::AbortHandle,
}

impl TaskValidation {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start(
        session: &Session,
        turn: &TurnContext,
        call_id: &str,
        command: &str,
        argv: &[String],
        local_cwd: Option<&Path>,
        validation: Option<&ValidationCommandContext>,
    ) -> Result<Option<Self>, String> {
        let coordinator = session.services.agent_control.task_coordinator();
        let Some(binding) = coordinator.binding_for_source(&turn.session_source) else {
            return Ok(None);
        };
        let task = coordinator
            .get_agent_task(binding.assignment_id, Some(0))
            .await
            .map_err(|e| e.to_string())?;
        if task.assignment.admission_origin != AssignmentAdmissionOrigin::Typed
            || !task
                .assignment
                .required_evidence
                .iter()
                .any(|required| required == command)
        {
            return Ok(None);
        }
        if task.current_attempt.attempt_id != binding.attempt_id
            || task.current_attempt.state != codex_agent_task_store::AttemptState::Active
            || task.current_attempt.sealed_at.is_some()
        {
            return Err(
                "validation caller is bound to an inactive or obsolete attempt".to_string(),
            );
        }
        let cwd = local_cwd
            .ok_or("typed validation evidence requires execution in its local bound workspace")?
            .to_path_buf();
        let workspace_id = tokio::task::spawn_blocking(move || {
            let root = crate::tools::handlers::resolve_repository_root(&cwd);
            codex_agent_task_store::repository_workspace_id(&root)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        if workspace_id != task.assignment.workspace_id {
            return Err("validation command cwd is outside its bound workspace".to_string());
        }
        let store = coordinator
            .store()
            .ok_or("typed task store is unavailable")?;
        let call = ValidationCall {
            call_id: call_id.to_string(),
            attempt_id: binding.attempt_id,
            command_summary: command.to_string(),
            evidence: ValidationEvidence {
                input_paths: Some(
                    validation.map_or_else(|| vec![".".to_string()], |v| v.covered_paths.clone()),
                ),
                ..Default::default()
            },
            status: ValidationCallStatus::Running,
            recorded_at: Utc::now(),
        };
        let argv = argv.to_vec();
        // Once registration starts, cancellation must not leave a durable Running call
        // without an owner. A dropped join result drops the guard and records cancellation.
        tokio::spawn(async move {
            store
                .record_validation_call(call.clone())
                .await
                .map_err(|e| e.to_string())?;
            let heartbeat_store = Arc::clone(&store);
            let heartbeat_id = call.call_id.clone();
            let heartbeat = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    match heartbeat_store
                        .heartbeat_validation_call(heartbeat_id.clone(), Utc::now())
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(error) => tracing::warn!(%error, "validation lease renewal failed"),
                    }
                }
            })
            .abort_handle();
            Ok(Some(Self {
                store,
                call: Some(call),
                argv,
                started: Instant::now(),
                heartbeat,
            }))
        })
        .await
        .map_err(|e| e.to_string())?
    }

    pub(crate) async fn finish(self, exit_code: Option<i32>) -> Result<(), String> {
        tokio::spawn(async move {
            let mut this = self;
            let mut call = this.call.as_ref().ok_or("missing owned validation call")?.clone();
            call.status = if exit_code == Some(0) {
                ValidationCallStatus::Succeeded
            } else {
                ValidationCallStatus::Failed
            };
            call.recorded_at = Utc::now();
            call.evidence.validation_result = Some(serde_json::json!({
                "argv": this.argv,
                "coveredPaths": call.evidence.input_paths,
                "callId": call.call_id,
                "status": if exit_code == Some(0) { "succeeded" } else { "failed" },
                "durationMs": u64::try_from(this.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            }));
            this.store
                .record_validation_call(call)
                .await
                .map_err(|e| e.to_string())?;
            this.call = None;
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

impl Drop for TaskValidation {
    fn drop(&mut self) {
        self.heartbeat.abort();
        if let Some(mut call) = self.call.take() {
            let store = Arc::clone(&self.store);
            call.status = ValidationCallStatus::Cancelled;
            call.recorded_at = Utc::now();
            call.evidence.output_summary =
                Some("Validation owner cancelled before a successful terminal result".to_string());
            tokio::spawn(async move {
                if let Err(error) = store.record_validation_call(call).await {
                    tracing::warn!(%error, "validation cancellation could not be recorded");
                }
            });
        }
    }
}

#[cfg(test)]
#[path = "task_validation_tests.rs"]
mod tests;
