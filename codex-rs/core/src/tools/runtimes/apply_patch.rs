//! Apply Patch runtime: executes verified patches under the orchestrator.
//!
//! Assumes `apply_patch` verification/approval happened upstream. Reuses the
//! selected turn environment filesystem for both local and remote turns, with
//! sandboxing enforced by the explicit filesystem sandbox context.
use crate::exec::is_likely_sandbox_denied;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::hook_names::HookToolName;
use crate::tools::sandboxing::Approvable;
use crate::tools::sandboxing::ApprovalCtx;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::PermissionRequestPayload;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::with_cached_approval;
use codex_agent_task_store::AttemptState;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::ApplyPatchAction;
use codex_exec_server::FileSystemSandboxContext;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::ReviewDecision;
use codex_sandboxing::SandboxType;
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
    revision_guard_mismatch: bool,
    failure: Option<codex_protocol::tool_failure::ToolFailure>,
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

    pub(crate) fn revision_guard_mismatch(&self) -> bool {
        self.revision_guard_mismatch
    }

    pub(crate) fn failure(&self) -> Option<&codex_protocol::tool_failure::ToolFailure> {
        self.failure.as_ref()
    }

    async fn validate_assignment(ctx: &ToolCtx, req: &ApplyPatchRequest) -> Result<(), ToolError> {
        let coordinator = ctx.session.services.agent_control.task_coordinator();
        let Some(binding) = coordinator.binding_for_turn(&ctx.turn) else {
            if matches!(&ctx.turn.session_source, codex_protocol::protocol::SessionSource::SubAgent(_)) {
                return Err(ToolError::Rejected("apply_patch: unbound subagents are read-only".into()));
            }
            return Ok(());
        };
        if req.turn_environment.environment.is_remote() {
            return Err(ToolError::Rejected("apply_patch: typed source contracts require the bound local repository".into()));
        }
        let authorization = coordinator.get_agent_task_authorization(binding.assignment_id).await
            .map_err(|error| ToolError::Rejected(format!("apply_patch: typed assignment unavailable: {error}")))?;
        if authorization.current_attempt.attempt_id != binding.attempt_id
            || authorization.current_attempt.state != AttemptState::Active
        {
            return Err(ToolError::Rejected("apply_patch: bound assignment attempt is no longer active".into()));
        }
        let assignment = &authorization.assignment;
        let amendment = authorization.current_attempt.amendment.as_ref();
        codex_agent_task_store::require_write_contract(assignment, amendment)
            .and_then(|()| codex_agent_task_store::dispatch_manifest(assignment, amendment).map(|_| ()))
            .map_err(|error| ToolError::Rejected(format!("apply_patch: {error}")))?;
        let root = std::path::Path::new(&authorization.repo_root);
        for (path, change) in req.action.changes() {
            let absolute = path.to_abs_path().map_err(|error| ToolError::Rejected(error.to_string()))?;
            let relative = codex_agent_task_store::normalize_absolute_source_path(root, &absolute)
                .map_err(|error| ToolError::Rejected(format!("apply_patch: {error}")))?;
            use codex_agent_task_store::SourceWriteOperation as Op;
            let operation = match change {
                codex_apply_patch::ApplyPatchFileChange::Add { .. } => Op::Create,
                codex_apply_patch::ApplyPatchFileChange::Delete { .. } => Op::Delete,
                codex_apply_patch::ApplyPatchFileChange::Update { .. } => Op::Update,
            };
            codex_agent_task_store::authorize_source_write(assignment, amendment, &relative, operation)
                .map_err(|error| ToolError::Rejected(format!("apply_patch: {error}")))?;
            if let codex_apply_patch::ApplyPatchFileChange::Update { move_path: Some(destination), .. } = change {
                let absolute = destination.to_abs_path().map_err(|error| ToolError::Rejected(error.to_string()))?;
                let destination = codex_agent_task_store::normalize_absolute_source_path(root, &absolute)
                    .map_err(|error| ToolError::Rejected(format!("apply_patch: {error}")))?;
                let destination_operation = if absolute.exists() { Op::Update } else { Op::Create };
                for (path, operation) in [(&relative, Op::Move), (&relative, Op::Delete),
                    (&destination, Op::Move), (&destination, destination_operation)]
                {
                    codex_agent_task_store::authorize_source_write(assignment, amendment, path, operation)
                        .map_err(|error| ToolError::Rejected(format!("apply_patch: {error}")))?;
                }
            }
        }
        Ok(())
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
                        return Err(ToolError::Codex(CodexErr::TurnAborted));
                    }
                    permit = crate::workspace_operation_gate::acquire_patch_operation_with_timeout(
                        &req.turn_environment.environment,
                        &req.action.cwd,
                    ) => permit.map_err(ToolError::Denied)?,
                });
            }
            Self::validate_assignment(ctx, req).await?;
            let started_at = Instant::now();
            let fs = req.turn_environment.environment.get_filesystem();
            let sandbox = Self::file_system_sandbox_context_for_attempt(req, attempt);
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let result = codex_apply_patch::apply_verified_patch_with_cancellation(
                &req.action,
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
                    self.failure = Some(patch_failure(&error));
                    // Preflight reports conflicts in order, so the first typed
                    // mismatch is the one a retry must correct first.
                    match &error {
                        codex_apply_patch::ApplyPatchError::PatchContextMismatch(mismatch) => {
                            self.patch_mismatch = Some(mismatch.clone());
                        }
                        codex_apply_patch::ApplyPatchError::MultiplePatchConflicts {
                            mismatches,
                            ..
                        } => {
                            self.patch_mismatch = mismatches.first().cloned();
                        }
                        codex_apply_patch::ApplyPatchError::RevisionGuardMismatch(_) => {
                            self.revision_guard_mismatch = true;
                        }
                        _ => {}
                    }
                    let kind = patch_failure_kind(&error);
                    let io_failure = matches!(error, codex_apply_patch::ApplyPatchError::IoError(_));
                    (delta, kind, io_failure)
                }
            };
            self.committed_delta.append(delta);
            let retry_safe = self.committed_delta.is_empty() && self.committed_delta.is_exact();
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
            if sandbox_denied {
                // Preserve the denied output without replaying the patch.
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
        ApplyPatchError::RevisionGuardMismatch(_) => "revision_guard_mismatch",
        ApplyPatchError::MultiplePatchConflicts { .. } => "multiple_conflicts",
        ApplyPatchError::PathUri(_) => "path",
        ApplyPatchError::ImplicitInvocation => "implicit_invocation",
        ApplyPatchError::EnvironmentIdMismatch { .. } => "environment_mismatch",
    }
}

/// Classify only typed owner evidence, never diagnostic substrings or patch bodies.
pub(crate) fn patch_failure(error: &codex_apply_patch::ApplyPatchError) -> codex_protocol::tool_failure::ToolFailure {
    use codex_apply_patch::{ApplyPatchError, PatchContextMismatchKind};
    use codex_protocol::tool_failure::{ToolFailure, ToolFailureCode, ToolFieldError};
    let mismatch_code = |kind| match kind {
        PatchContextMismatchKind::StaleRange => ToolFailureCode::StaleInput,
        PatchContextMismatchKind::AmbiguousMatch => ToolFailureCode::AmbiguousAnchor,
        PatchContextMismatchKind::ContextNotFound | PatchContextMismatchKind::ExpectedLinesNotFound => ToolFailureCode::MissingAnchor,
        PatchContextMismatchKind::MalformedRange => ToolFailureCode::InvalidSyntax,
        _ => ToolFailureCode::InvalidInput,
    };
    let code = match error {
        ApplyPatchError::ParseError(_) => ToolFailureCode::InvalidSyntax,
        ApplyPatchError::RevisionGuardMismatch(_) => ToolFailureCode::StaleInput,
        ApplyPatchError::PatchContextMismatch(mismatch) => mismatch_code(mismatch.kind),
        ApplyPatchError::MultiplePatchConflicts { mismatches, .. } => mismatches.first()
            .map(|mismatch| mismatch_code(mismatch.kind)).unwrap_or(ToolFailureCode::InvalidInput),
        ApplyPatchError::IoError(error) => {
            if std::error::Error::source(error).and_then(|error| error.downcast_ref::<std::io::Error>())
                .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied) {
                ToolFailureCode::PermissionDenied
            } else { ToolFailureCode::Failed }
        }
        _ => ToolFailureCode::InvalidInput,
    };
    let (message, expected) = match code {
        ToolFailureCode::InvalidSyntax => ("Patch syntax is invalid.", "the advertised patch grammar and valid range syntax"),
        ToolFailureCode::StaleInput => ("Patch source revision is stale.", "the current source revision after reconciling intended changes"),
        ToolFailureCode::MissingAnchor => ("Patch anchor or contiguous old lines were not found.", "an existing anchor followed immediately by the complete old lines"),
        ToolFailureCode::AmbiguousAnchor => ("Patch anchor matches more than one location.", "one uniquely identified source location"),
        ToolFailureCode::PermissionDenied => ("Patch filesystem access was denied.", "access permitted by the active filesystem policy"),
        ToolFailureCode::Failed => ("Patch filesystem operation failed.", "an available target through the selected filesystem owner"),
        _ => ("Patch input does not match the source contract.", "valid paths and ordered, exact source preimages"),
    };
    let mut failure = if matches!(error, ApplyPatchError::IoError(_)) {
        ToolFailure::new(code, message).with_completed_execution()
    } else {
        ToolFailure::no_effects(code, message)
    };
    failure.field_errors.push(ToolFieldError {
        path: String::new(), expected: expected.into(), correction: failure.safe_next_actions[0].clone(),
    });
    let mismatches = match error {
        ApplyPatchError::PatchContextMismatch(mismatch) => std::slice::from_ref(mismatch),
        ApplyPatchError::MultiplePatchConflicts { mismatches, .. } => mismatches.as_slice(),
        _ => &[],
    };
    if !mismatches.is_empty() {
        failure.details = Some(serde_json::json!({"conflicts": mismatches.iter().map(|mismatch| serde_json::json!({
            "code": mismatch_code(mismatch.kind), "kind": mismatch.kind,
            "path": mismatch.canonical_path, "hunk": mismatch.hunk_ordinal, "chunk": mismatch.chunk_ordinal,
            "current_content_sha256": mismatch.current_content_sha256,
            "expected_excerpt": mismatch.expected_excerpt,
            "expected_source_sha256": mismatch.expected_source_sha256,
            "current_line_start": mismatch.current_line_start, "current_line_end": mismatch.current_line_end,
            "current_excerpt": mismatch.current_excerpt,
            "candidate_locations": mismatch.candidate_locations,
            "edits_result_of_hunk": mismatch.edits_result_of_hunk,
        })).collect::<Vec<_>>() }));
    }
    if let ApplyPatchError::ParseError(codex_apply_patch::ParseError::InvalidHunkError { line_number, .. }) = error {
        failure.details = Some(serde_json::json!({"line": line_number}));
    }
    if let ApplyPatchError::RevisionGuardMismatch(diagnostic) = error {
        // The owner already bounds each available source excerpt; preserve its
        // expected/current identities on shell interception as well as direct calls.
        failure.details = Some(serde_json::json!({"revision_guard_diagnostic": diagnostic}));
    }
    if let ApplyPatchError::ComputeReplacements(diagnostic) = error {
        failure.details = Some(serde_json::json!({"owner_diagnostic": diagnostic}));
    }
    failure
}
