//! Apply Patch runtime: executes verified patches under the orchestrator.
//!
//! Assumes `apply_patch` verification/approval happened upstream. Reuses the
//! selected turn environment filesystem for both local and remote turns, with
//! sandboxing enforced by the explicit filesystem sandbox context.
use crate::agent::task_capabilities::normalize_absolute_repo_path;
use crate::exec::is_likely_sandbox_denied;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::hook_names::HookToolName;
use crate::tools::sandboxing::Approvable;
use crate::tools::sandboxing::ApprovalCtx;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::PermissionRequestPayload;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::Sandboxable;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::with_cached_approval;
use codex_agent_task_store::AttemptState;
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
use codex_sandboxing::policy_transforms::effective_permission_profile_uri;
use codex_utils_path_uri::PathUri;
use futures::future::BoxFuture;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize)]
pub(crate) struct ApplyPatchApprovalKey {
    environment_id: String,
    approval_scope_id: String,
    path: PathUri,
}

/// Keeps approval for a less-restricted retry separate from ordinary patch
/// approval. `ApprovalStore` includes the key type in its serialized namespace.
#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize)]
struct ApplyPatchEscalationApprovalKey(ApplyPatchApprovalKey);

#[derive(Debug)]
pub struct ApplyPatchRequest {
    pub turn_environment: TurnEnvironment,
    pub action: ApplyPatchAction,
    pub file_paths: Vec<PathUri>,
    pub changes: std::collections::HashMap<PathBuf, FileChange>,
    pub exec_approval_requirement: ExecApprovalRequirement,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub permissions_preapproved: bool,
    pub cancellation_token: tokio_util::sync::CancellationToken,
}

#[derive(Default)]
pub struct ApplyPatchRuntime {
    committed_delta: AppliedPatchDelta,
    patch_mismatch: Option<codex_apply_patch::PatchContextMismatch>,
    workspace_tracking_started: bool,
    workspace_tracking_finished: bool,
    mutation_repo_root: Option<PathBuf>,
    mutation_repo_paths: Vec<String>,
    workspace_operation_permit: Option<crate::scoped_workspace_gate::WorkspaceLease>,
    mutation_in_progress: std::sync::Arc<std::sync::atomic::AtomicBool>,
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

    pub(crate) fn with_workspace_operation_permit(
        workspace_operation_permit: Option<crate::scoped_workspace_gate::WorkspaceLease>,
    ) -> Self {
        Self {
            workspace_operation_permit,
            ..Self::new()
        }
    }

    pub fn mutation_in_progress(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.mutation_in_progress.clone()
    }

    pub fn committed_delta(&self) -> &AppliedPatchDelta {
        &self.committed_delta
    }

    pub(crate) fn patch_mismatch(&self) -> Option<&codex_apply_patch::PatchContextMismatch> {
        self.patch_mismatch.as_ref()
    }

    pub async fn finish_pending_workspace_tracking(&mut self, ctx: &ToolCtx) {
        if self.mutation_repo_root.is_some() && !self.workspace_tracking_finished {
            self.finish_workspace_tracking(ctx, true).await;
        }
    }


    async fn begin_workspace_tracking(
        &mut self,
        req: &ApplyPatchRequest,
        ctx: &ToolCtx,
    ) -> Result<(), ToolError> {
        if self.workspace_tracking_started {
            return Ok(());
        }

        let coordinator = ctx.session.services.agent_control.task_coordinator();
        let binding = coordinator.binding_for_source(&ctx.turn.session_source);
        if let Some(binding) = &binding {
            let authorization = coordinator
                .get_agent_task_authorization(binding.assignment_id)
                .await
                .map_err(|error| {
                    ToolError::Rejected(format!(
                        "apply_patch: typed assignment state is unavailable: {error}"
                    ))
                })?;
            if authorization.current_attempt.attempt_id != binding.attempt_id
                || authorization.current_attempt.state != AttemptState::Active
            {
                return Err(ToolError::Rejected(
                    "apply_patch: the bound typed assignment attempt is no longer active"
                        .to_string(),
                ));
            }
        }

        let cwd = match req.turn_environment.cwd().to_abs_path() {
            Ok(cwd) => cwd.to_path_buf(),
            Err(error) => {
                if binding.is_some() {
                    return Err(ToolError::Rejected(format!(
                        "apply_patch: typed patch tracking requires a host-native workspace: {error}"
                    )));
                }
                self.workspace_tracking_started = true;
                return Ok(());
            }
        };
        let repo_root = get_git_repo_root(&cwd).unwrap_or(cwd);
        let mutation_paths = native_mutation_repo_paths(
            &repo_root,
            &req.file_paths,
            /*require_complete*/ binding.is_some(),
        )?;
        let repo_paths = mutation_paths.paths;

        if repo_paths.is_empty() || !mutation_paths.complete {
            ctx.session
                .services
                .git_workspace
                .note_host_workspace_mutation();
        } else {
            ctx.session
                .services
                .git_workspace
                .note_host_workspace_mutation_paths(&repo_root, &repo_paths)
                .await;
        }

        self.mutation_repo_root = Some(repo_root);
        self.mutation_repo_paths = repo_paths;
        self.workspace_tracking_started = true;
        Ok(())
    }

    async fn finish_workspace_tracking(&mut self, ctx: &ToolCtx, finish_tracking: bool) {
        self.workspace_tracking_started = false;
        let (repo_root, repo_paths) = if finish_tracking {
            (
                self.mutation_repo_root.take(),
                std::mem::take(&mut self.mutation_repo_paths),
            )
        } else {
            // A denied attempt can await retry approval. Retain the pending
            // invalidation for cancellation there, while still rechecking admission
            // on a later runtime attempt.
            (
                self.mutation_repo_root.clone(),
                self.mutation_repo_paths.clone(),
            )
        };
        if let Some(repo_root) = repo_root.as_ref() {
            if self.committed_delta.is_exact() && !repo_paths.is_empty() {
                ctx.session
                    .services
                    .git_workspace
                    .note_host_workspace_mutation_paths(repo_root, &repo_paths)
                    .await;
            } else if !self.committed_delta.is_empty() {
                ctx.session
                    .services
                    .git_workspace
                    .note_host_workspace_mutation();
            }
        }

        if finish_tracking {
            self.workspace_tracking_finished = true;
        }
    }

    fn file_system_sandbox_context_for_attempt(
        req: &ApplyPatchRequest,
        attempt: &SandboxAttempt<'_>,
    ) -> Option<FileSystemSandboxContext> {
        if req.turn_environment.environment.is_remote() {
            // The executor chooses its own platform sandbox. Preserve the requested
            // policy even when this host cannot select a concrete sandbox wrapper.
            return attempt.sandbox_requested.then(|| FileSystemSandboxContext {
                permissions: effective_permission_profile_uri(
                    attempt.exec_server_permissions,
                    req.additional_permissions.clone().map(Into::into).as_ref(),
                ),
                cwd: Some(attempt.sandbox_cwd.clone()),
                workspace_roots: Vec::new(),
                windows_sandbox_level: attempt.windows_sandbox_level,
                windows_sandbox_private_desktop: attempt.windows_sandbox_private_desktop,
            });
        }
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
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
struct NativeMutationRepoPaths {
    paths: Vec<String>,
    complete: bool,
}

fn native_mutation_repo_paths(
    repo_root: &std::path::Path,
    file_paths: &[PathUri],
    require_complete: bool,
) -> Result<NativeMutationRepoPaths, ToolError> {
    let mut paths = Vec::new();
    let mut complete = true;
    for path in file_paths {
        let native_path = match path.to_abs_path() {
            Ok(path) => path,
            Err(error) if require_complete => {
                return Err(ToolError::Rejected(format!(
                    "apply_patch: patch tracking cannot represent `{path}` on this host: {error}"
                )));
            }
            Err(_) => {
                complete = false;
                continue;
            }
        };
        match normalize_absolute_repo_path(repo_root, native_path.as_path()) {
            Ok(path) => paths.push(path),
            Err(error) if require_complete => {
                return Err(ToolError::Rejected(format!(
                    "apply_patch: mutation path `{path}` is outside the patch workspace: {error}"
                )));
            }
            Err(_) => complete = false,
        }
    }
    // Patch changes originate in a map. Keep admission and any rejection stable
    // across equivalent requests, including partially admitted multi-path calls.
    paths.sort_unstable();
    Ok(NativeMutationRepoPaths { paths, complete })
}

impl Sandboxable for ApplyPatchRuntime {
    fn sandbox_preference(&self) -> SandboxablePreference {
        SandboxablePreference::Auto
    }
    fn escalate_on_failure(&self) -> bool {
        self.committed_delta.is_empty() && self.committed_delta.is_exact()
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
                approval_scope_id: req
                    .turn_environment
                    .environment
                    .approval_scope_id()
                    .to_string(),
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
                let escalation_approval_keys = approval_keys
                    .into_iter()
                    .map(ApplyPatchEscalationApprovalKey)
                    .collect();
                return with_cached_approval(
                    &session.services,
                    "apply_patch",
                    escalation_approval_keys,
                    || async move {
                        session
                            .request_patch_approval(
                                turn,
                                call_id,
                                changes,
                                Some(reason),
                                /*grant_root*/ None,
                            )
                            .await
                    },
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
        self.mutation_in_progress
            .store(true, std::sync::atomic::Ordering::Release);
        let result = async {
            if self.workspace_operation_permit.is_none() {
                let _tool_wait = ctx.turn.turn_timing_state.begin_tool_execution();
                let _phase = crate::tools::tool_dispatch_trace::begin_tool_phase("patch_gate_wait");
                self.workspace_operation_permit = Some(tokio::select! {
                    biased;
                    _ = req.cancellation_token.cancelled() => {
                        self.finish_workspace_tracking(ctx, true).await;
                        return Err(ToolError::Codex(CodexErr::TurnAborted));
                    }
                    permit = crate::workspace_operation_gate::acquire_patch_operation_with_timeout(
                        &req.turn_environment.environment,
                        &req.action.cwd,
                    ) => permit.map_err(|error| ToolError::Denied(error.to_string()))?,
                });
            }
            if let Err(error) = self.begin_workspace_tracking(req, ctx).await {
                self.finish_workspace_tracking(ctx, true).await;
                return Err(error);
            }
            let started_at = Instant::now();
            let fs = req.turn_environment.environment.get_filesystem();
            let sandbox = Self::file_system_sandbox_context_for_attempt(req, attempt);
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let result = codex_apply_patch::apply_patch_with_cancellation(
                &req.action.patch,
                &req.action.cwd,
                &mut stdout,
                &mut stderr,
                fs.as_ref(),
                sandbox.as_ref(),
                &|| req.cancellation_token.is_cancelled(),
            )
            .await;
            let stdout = String::from_utf8_lossy(&stdout).into_owned();
            let mut stderr = String::from_utf8_lossy(&stderr).into_owned();
            let failed = result.is_err();
            let exit_code = if failed { 1 } else { 0 };
            let (delta, failure_kind, io_failure) = match result {
                Ok(delta) => (delta, "none", false),
                Err(failure) => {
                    let (error, delta) = failure.into_parts();
                    if let codex_apply_patch::ApplyPatchError::PatchContextMismatch(mismatch) = &error {
                        self.patch_mismatch = Some(mismatch.clone());
                    }
                    let kind = patch_failure_kind(&error);
                    let io_failure = matches!(error, codex_apply_patch::ApplyPatchError::IoError(_));
                    (delta, kind, io_failure)
                }
            };
            self.committed_delta.append(delta);
            let retry_safe = self.escalate_on_failure();
            if failed && !retry_safe {
                stderr.push_str("Automatic patch retry withheld because files changed or a failed write may have changed them.\n");
            }
            let output = ExecToolCallOutput {
                exit_code,
                stdout: StreamOutput::new(stdout.clone()),
                stderr: StreamOutput::new(stderr.clone()),
                aggregated_output: StreamOutput::new(format!("{stdout}{stderr}")),
                duration: started_at.elapsed(),
                timed_out: false,
            };
            let sandbox_denied = io_failure
                && retry_safe
                && is_likely_sandbox_denied(attempt.sandbox, &output);
            ctx.session.services.session_telemetry.counter(
                "codex.apply_patch.attempt", 1,
                &[
                    ("outcome", if failed { "failed" } else { "success" }),
                    ("failure_kind", failure_kind),
                    ("mutation", if self.committed_delta.is_empty() && self.committed_delta.is_exact() {
                        "none"
                    } else if self.committed_delta.is_exact() {
                        "exact"
                    } else {
                        "uncertain"
                    }),
                ],
            );
            self.finish_workspace_tracking(ctx, !sandbox_denied).await;
            if sandbox_denied {
                // Keep the verified file state serialized across retry approval too.
                return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                    output: Box::new(output),
                    network_policy_decision: None,
                })));
            }
            Ok(ApplyPatchRuntimeOutput {
                exec_output: output,
                delta: self.committed_delta.clone(),
            })
        }
        .await;
        self.mutation_in_progress
            .store(false, std::sync::atomic::Ordering::Release);
        result
    }
}

pub(crate) fn patch_failure_kind(error: &codex_apply_patch::ApplyPatchError) -> &'static str {
    use codex_apply_patch::ApplyPatchError;
    match error {
        ApplyPatchError::PatchContextMismatch(mismatch) => match mismatch.kind {
            codex_apply_patch::PatchContextMismatchKind::AmbiguousMatch => "ambiguous_match",
            _ => "context_mismatch",
        },
        ApplyPatchError::ParseError(_) => "parse",
        ApplyPatchError::IoError(_) => "io",
        ApplyPatchError::ComputeReplacements(_) => "replacement",
        ApplyPatchError::PathUri(_) => "path",
        ApplyPatchError::ImplicitInvocation => "implicit_invocation",
        ApplyPatchError::EnvironmentIdMismatch { .. } => "environment_mismatch",
    }
}

#[cfg(test)]
#[path = "apply_patch_tests.rs"]
mod tests;
