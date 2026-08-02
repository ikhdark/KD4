//! Apply Patch runtime: executes verified patches under the orchestrator.
//!
//! Assumes `apply_patch` verification/approval happened upstream. Reuses the
//! selected turn environment filesystem for both local and remote turns, with
//! sandboxing enforced by the explicit filesystem sandbox context.
use crate::agent::task_capabilities::ExternalMutationIntent;
use crate::agent::task_capabilities::TypedToolClass;
use crate::agent::task_capabilities::TypedToolRequest;
use crate::agent::task_capabilities::authorize_typed_tool;
use crate::agent::task_capabilities::normalize_absolute_repo_path;
use crate::exec::is_likely_sandbox_denied;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::hook_names::HookToolName;
use crate::tools::sandboxing::Approvable;
use crate::tools::sandboxing::ApprovalAction;
use crate::tools::sandboxing::ApprovalCtx;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::PermissionRequestPayload;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::Sandboxable;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::with_cached_approval;
use codex_agent_task_store::AgentTaskStore;
use codex_agent_task_store::AttemptState;
use codex_agent_task_store::AttributionConfidence;
use codex_agent_task_store::WorkspaceActorKind;
use codex_agent_task_store::WorkspaceMutationLease;
use codex_agent_task_store::WorkspaceMutationRequest;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::ApplyPatchAction;
use codex_exec_server::FileSystemSandboxContext;
use codex_git_utils::get_git_repo_root;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::ReviewDecision;
use codex_sandboxing::SandboxType;
use codex_sandboxing::SandboxablePreference;
use codex_sandboxing::policy_transforms::effective_permission_profile;
use codex_utils_path_uri::PathUri;
use futures::future::BoxFuture;
use std::path::PathBuf;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize)]
pub(crate) struct ApplyPatchApprovalKey {
    environment_id: String,
    path: PathUri,
}

#[derive(Debug)]
pub struct ApplyPatchRequest {
    pub turn_environment: TurnEnvironment,
    pub action: ApplyPatchAction,
    pub file_paths: Vec<PathUri>,
    pub changes: std::collections::HashMap<PathBuf, FileChange>,
    pub exec_approval_requirement: ExecApprovalRequirement,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub permissions_preapproved: bool,
}

#[derive(Default)]
pub struct ApplyPatchRuntime {
    committed_delta: AppliedPatchDelta,
    typed_mutations_started: bool,
    workspace_mutation: Option<(
        std::sync::Arc<dyn AgentTaskStore>,
        PathBuf,
        WorkspaceMutationLease,
    )>,
}

#[derive(Debug)]
pub struct ApplyPatchRuntimeOutput {
    pub exec_output: ExecToolCallOutput,
    pub delta: AppliedPatchDelta,
}

impl ApplyPatchRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn committed_delta(&self) -> &AppliedPatchDelta {
        &self.committed_delta
    }

    async fn begin_workspace_mutations(
        &mut self,
        req: &ApplyPatchRequest,
        ctx: &ToolCtx,
    ) -> Result<(), ToolError> {
        if self.typed_mutations_started {
            return Ok(());
        }
        let coordinator = ctx.session.services.agent_control.task_coordinator();
        if coordinator.store().is_none() {
            coordinator
                .initialize_for_workspace_coordination(
                    ctx.session.services.state_db.clone(),
                    ctx.turn.config.sqlite_home.clone(),
                    ctx.turn.config.model_provider_id.clone(),
                    ctx.session.services.agent_control.session_id().to_string(),
                )
                .await
                .map_err(|error| {
                    ToolError::Rejected(format!(
                        "apply_patch: workspace coordination could not initialize: {error}"
                    ))
                })?;
        }
        let binding = coordinator.binding_for_source(&ctx.turn.session_source);
        let task = match &binding {
            Some(binding) => {
                let task = coordinator
                    .get_agent_task(binding.assignment_id, Some(0))
                    .await
                    .map_err(|error| {
                        ToolError::Rejected(format!(
                            "apply_patch: typed assignment state is unavailable: {error}"
                        ))
                    })?;
                if task.current_attempt.attempt_id != binding.attempt_id
                    || task.current_attempt.state != AttemptState::Active
                {
                    return Err(ToolError::Rejected(
                        "apply_patch: the bound typed assignment attempt is no longer active"
                            .to_string(),
                    ));
                }
                Some(task)
            }
            None => None,
        };

        let cwd = match req.turn_environment.cwd().to_abs_path() {
            Ok(cwd) => cwd.to_path_buf(),
            Err(_) if binding.is_none() => {
                // Workspace leases are host-local. Untyped patches against a
                // remote environment are still protected by that environment's
                // filesystem sandbox and cannot be represented in the local
                // workspace task store.
                self.typed_mutations_started = true;
                return Ok(());
            }
            Err(error) => {
                return Err(ToolError::Rejected(format!(
                    "apply_patch: typed assignments require a local filesystem environment: {error}"
                )));
            }
        };
        let repo_root = get_git_repo_root(&cwd).unwrap_or(cwd);
        let repo_paths = req
            .file_paths
            .iter()
            .map(|path| {
                path.to_abs_path()
                    .map_err(|error| {
                        ToolError::Rejected(format!(
                            "apply_patch: typed mutation path is not local: {error}"
                        ))
                    })
                    .and_then(|path| {
                        normalize_absolute_repo_path(&repo_root, path.as_path()).map_err(|error| {
                            ToolError::Rejected(format!(
                                "apply_patch: typed mutation path is invalid: {error}"
                            ))
                        })
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let normalized_repo_paths = if let Some(task) = &task {
            authorize_typed_tool(
                &task.assignment,
                &repo_root,
                TypedToolRequest {
                    class: TypedToolClass::StructuredEdit,
                    external_mutation_intent: ExternalMutationIntent::MayMutate,
                    repo_paths: &repo_paths,
                },
            )
            .map_err(|error| {
                ToolError::Rejected(format!(
                    "apply_patch: typed assignment capability denied: {error}"
                ))
            })?
            .normalized_repo_paths
        } else {
            repo_paths
        };
        if binding.is_none() {
            ctx.session
                .services
                .agent_control
                .reconcile_live_typed_actor_heartbeats()
                .await
                .map_err(|error| {
                    ToolError::Rejected(format!(
                        "apply_patch: typed-agent liveness could not be reconciled: {error}"
                    ))
                })?;
        }
        let store = coordinator.store().ok_or_else(|| {
            ToolError::Rejected("apply_patch: the workspace task store is unavailable".to_string())
        })?;
        let root_session_id = coordinator.root_session_id().ok_or_else(|| {
            ToolError::Rejected("apply_patch: root task identity is unavailable".to_string())
        })?;
        let agent_path = ctx
            .turn
            .session_source
            .get_agent_path()
            .map(|path| path.to_string())
            .unwrap_or_else(|| "/root".to_string());
        let (kind, actor_id) = if let Some(binding) = &binding {
            (
                WorkspaceActorKind::Typed,
                format!("attempt:{}", binding.attempt_id),
            )
        } else if ctx.turn.session_source.is_non_root_agent() {
            (
                WorkspaceActorKind::Legacy,
                format!("legacy:{root_session_id}:{agent_path}"),
            )
        } else {
            (WorkspaceActorKind::Root, format!("root:{root_session_id}"))
        };
        let expected_manifest = store
            .supporting_read_manifest(&repo_root, actor_id.clone(), normalized_repo_paths.clone())
            .await
            .map_err(|error| {
                ToolError::Rejected(format!(
                    "apply_patch: supporting-read manifest could not be loaded: {error}"
                ))
            })?;
        let lease = store
            .begin_workspace_mutation(
                &repo_root,
                WorkspaceMutationRequest {
                    root_session_id,
                    actor_id,
                    kind,
                    attempt_id: binding.as_ref().map(|binding| binding.attempt_id),
                    paths: normalized_repo_paths,
                    contracts: task
                        .as_ref()
                        .map(|task| task.assignment.contract_claims.clone())
                        .unwrap_or_default(),
                    expected_manifest,
                },
            )
            .await
            .map_err(|error| {
                ToolError::Rejected(format!(
                    "apply_patch: workspace mutation was rejected: {error}"
                ))
            })?;
        if let Some(binding) = &binding {
            for path in &lease.paths {
                if let Err(error) = store
                    .begin_mutation(
                        binding.attempt_id,
                        &repo_root,
                        path.clone(),
                        AttributionConfidence::Definitive,
                    )
                    .await
                {
                    if let Err(cleanup_error) = store
                        .finish_workspace_mutation(&repo_root, lease.clone())
                        .await
                    {
                        tracing::warn!(
                            %cleanup_error,
                            "apply_patch failed to release workspace lease after mutation-evidence failure"
                        );
                    }
                    return Err(ToolError::Rejected(format!(
                        "apply_patch: failed to capture typed mutation evidence: {error}"
                    )));
                }
            }
        }
        self.workspace_mutation = Some((store, repo_root, lease));
        self.typed_mutations_started = true;
        Ok(())
    }

    async fn finish_workspace_mutation(&mut self) -> Result<(), ToolError> {
        let Some((store, repo_root, lease)) = self.workspace_mutation.take() else {
            self.typed_mutations_started = false;
            return Ok(());
        };
        let result = store.finish_workspace_mutation(&repo_root, lease).await;
        self.typed_mutations_started = false;
        result.map(|_| ()).map_err(|error| {
            ToolError::Rejected(format!(
                "apply_patch: workspace mutation finalization failed: {error}"
            ))
        })
    }

    fn build_guardian_review_request(
        req: &ApplyPatchRequest,
        call_id: &str,
    ) -> std::io::Result<ApprovalAction> {
        // TODO(anp): Remove this conversion once the guardian API supports PathUri.
        let cwd = req.action.cwd.to_abs_path()?;
        let files = req
            .file_paths
            .iter()
            .map(PathUri::to_abs_path)
            .collect::<std::io::Result<Vec<_>>>()?;
        Ok(ApprovalAction::ApplyPatch {
            id: call_id.to_string(),
            cwd,
            files,
            patch: req.action.patch.clone(),
        })
    }

    fn file_system_sandbox_context_for_attempt(
        req: &ApplyPatchRequest,
        attempt: &SandboxAttempt<'_>,
    ) -> Option<FileSystemSandboxContext> {
        if attempt.sandbox == SandboxType::None {
            return None;
        }

        let permissions =
            effective_permission_profile(attempt.permissions, req.additional_permissions.as_ref());
        Some(FileSystemSandboxContext {
            permissions: permissions.into(),
            cwd: Some(attempt.sandbox_cwd.clone()),
            workspace_roots: attempt
                .workspace_roots
                .iter()
                .map(PathUri::from_abs_path)
                .collect(),
            windows_sandbox_level: attempt.windows_sandbox_level,
            windows_sandbox_private_desktop: attempt.windows_sandbox_private_desktop,
            use_legacy_landlock: attempt.use_legacy_landlock,
        })
    }
}

impl Sandboxable for ApplyPatchRuntime {
    fn sandbox_preference(&self) -> SandboxablePreference {
        SandboxablePreference::Auto
    }
    fn escalate_on_failure(&self) -> bool {
        true
    }
}

impl Approvable<ApplyPatchRequest> for ApplyPatchRuntime {
    type ApprovalKey = ApplyPatchApprovalKey;

    fn approval_keys(&self, req: &ApplyPatchRequest) -> Vec<Self::ApprovalKey> {
        req.file_paths
            .iter()
            .cloned()
            .map(|path| ApplyPatchApprovalKey {
                environment_id: req.turn_environment.environment_id.clone(),
                path,
            })
            .collect()
    }

    fn start_approval_async<'a>(
        &'a mut self,
        req: &'a ApplyPatchRequest,
        ctx: ApprovalCtx<'a>,
    ) -> BoxFuture<'a, ReviewDecision> {
        let session = ctx.session;
        let turn = ctx.turn;
        let call_id = ctx.call_id.to_string();
        let retry_reason = ctx.retry_reason.clone();
        let approval_keys = self.approval_keys(req);
        let changes = req.changes.clone();
        Box::pin(async move {
            if req.permissions_preapproved && retry_reason.is_none() {
                return ReviewDecision::Approved;
            }
            if let Some(reason) = retry_reason {
                return session
                    .request_patch_approval(
                        turn,
                        call_id,
                        changes.clone(),
                        Some(reason),
                        /*grant_root*/ None,
                    )
                    .await;
            }

            with_cached_approval(
                &session.services,
                "apply_patch",
                approval_keys,
                || async move {
                    session
                        .request_patch_approval(
                            turn, call_id, changes, /*reason*/ None, /*grant_root*/ None,
                        )
                        .await
                },
            )
            .await
        })
    }

    fn approval_action(
        &self,
        req: &ApplyPatchRequest,
        ctx: &ApprovalCtx<'_>,
    ) -> std::io::Result<ApprovalAction> {
        ApplyPatchRuntime::build_guardian_review_request(req, ctx.call_id)
    }

    fn wants_no_sandbox_approval(&self, policy: AskForApproval) -> bool {
        match policy {
            AskForApproval::Never => false,
            AskForApproval::Granular(granular_config) => granular_config.allows_sandbox_approval(),
            AskForApproval::OnRequest => true,
            AskForApproval::UnlessTrusted => true,
        }
    }

    // apply_patch approvals are decided upstream by assess_patch_safety.
    //
    // This override ensures the orchestrator runs the patch approval flow when required instead
    // of falling back to the global exec approval policy.
    fn exec_approval_requirement(
        &self,
        req: &ApplyPatchRequest,
    ) -> Option<ExecApprovalRequirement> {
        Some(req.exec_approval_requirement.clone())
    }

    fn permission_request_payload(
        &self,
        req: &ApplyPatchRequest,
    ) -> Option<PermissionRequestPayload> {
        Some(PermissionRequestPayload {
            tool_name: HookToolName::apply_patch(),
            tool_input: serde_json::json!({ "command": req.action.patch }),
        })
    }
}

impl ToolRuntime<ApplyPatchRequest, ApplyPatchRuntimeOutput> for ApplyPatchRuntime {
    fn sandbox_cwd<'a>(&self, req: &'a ApplyPatchRequest) -> Option<&'a PathUri> {
        Some(&req.action.cwd)
    }

    async fn run(
        &mut self,
        req: &ApplyPatchRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ApplyPatchRuntimeOutput, ToolError> {
        self.begin_workspace_mutations(req, ctx).await?;
        let heartbeat_stop = CancellationToken::new();
        let heartbeat_task = self
            .workspace_mutation
            .as_ref()
            .map(|(store, repo_root, lease)| {
                let store = store.clone();
                let repo_root = repo_root.clone();
                let lease_id = lease.lease_id.clone();
                let actor_id = lease.actor_id.clone();
                let heartbeat_stop = heartbeat_stop.clone();
                AbortOnDropHandle::new(tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = heartbeat_stop.cancelled() => break,
                            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                                match store
                                    .heartbeat_workspace_mutation(
                                        &repo_root,
                                        lease_id.clone(),
                                        actor_id.clone(),
                                    )
                                    .await
                                {
                                    Ok(true) => {}
                                    Ok(false) => {
                                        tracing::warn!(
                                            %lease_id,
                                            "apply_patch workspace mutation lease expired before heartbeat"
                                        );
                                        break;
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            %error,
                                            %lease_id,
                                            "apply_patch workspace mutation heartbeat failed"
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }))
            });
        let started_at = Instant::now();
        let fs = req.turn_environment.environment.get_filesystem();
        let sandbox = Self::file_system_sandbox_context_for_attempt(req, attempt);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = codex_apply_patch::apply_patch(
            &req.action.patch,
            &req.action.cwd,
            &mut stdout,
            &mut stderr,
            fs.as_ref(),
            sandbox.as_ref(),
        )
        .await;
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        let failed = result.is_err();
        let exit_code = if failed { 1 } else { 0 };
        let delta = match result {
            Ok(delta) => delta,
            Err(failure) => failure.into_parts().1,
        };
        self.committed_delta.append(delta);
        heartbeat_stop.cancel();
        if let Some(heartbeat_task) = heartbeat_task
            && let Err(error) = heartbeat_task.await
        {
            tracing::warn!(%error, "apply_patch workspace mutation heartbeat task failed");
        }
        let output = ExecToolCallOutput {
            exit_code,
            stdout: StreamOutput::new(stdout.clone()),
            stderr: StreamOutput::new(stderr.clone()),
            aggregated_output: StreamOutput::new(format!("{stdout}{stderr}")),
            duration: started_at.elapsed(),
            timed_out: false,
        };
        if failed && is_likely_sandbox_denied(attempt.sandbox, &output) {
            return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                output: Box::new(output),
                network_policy_decision: None,
            })));
        }
        self.finish_workspace_mutation().await?;
        Ok(ApplyPatchRuntimeOutput {
            exec_output: output,
            delta: self.committed_delta.clone(),
        })
    }
}

#[cfg(test)]
#[path = "apply_patch_tests.rs"]
mod tests;
