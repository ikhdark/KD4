//! Apply Patch runtime: executes verified patches under the orchestrator.
//!
//! Assumes `apply_patch` verification/approval happened upstream. Reuses the
//! selected turn environment filesystem for both local and remote turns.
//! Approval and source-write scope checks remain independent of full host access.
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
use codex_agent_task_store::AgentTaskAuthorization;
use codex_agent_task_store::AttemptState;
use codex_agent_task_store::SourceWriteOperation;
use codex_agent_task_store::StoreError;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchFileChange;
use codex_exec_server::FileSystemSandboxContext;
use codex_protocol::error::CodexErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::ReviewDecision;
use codex_utils_path_uri::PathUri;
use futures::future::BoxFuture;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
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
    pub permissions_preapproved: bool,
    pub cancellation_token: tokio_util::sync::CancellationToken,
}

struct AssignmentEndpointCheck {
    path: PathUri,
    operation: SourceWriteOperation,
    move_path: Option<PathUri>,
}

enum AssignmentEndpointError {
    Refused { endpoint: String, reason: String },
    Rejected(String),
}

#[derive(Default)]
pub struct ApplyPatchRuntime {
    committed_delta: AppliedPatchDelta,
    patch_mismatch: Option<codex_apply_patch::PatchContextMismatch>,
    revision_guard_mismatch: bool,
    failure: Option<codex_protocol::tool_failure::ToolFailure>,
    workspace_operation_permit: Option<Arc<crate::scoped_workspace_gate::WorkspaceLease>>,
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
            workspace_operation_permit: workspace_operation_permit.map(Arc::new),
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

    /// A write the assignment does not authorize is a capability refusal on every
    /// route. Shell-intercepted patches reach this owner without the router gate,
    /// so record it for the terminal assessment and report the router's envelope
    /// (no-effects `PermissionDenied`, no retry receipt) instead of a patch failure.
    fn capability_denied(&mut self, ctx: &ToolCtx, reason: String) -> ToolError {
        self.refused(ctx, None, reason)
    }

    /// One refused endpoint of an assignment that may otherwise patch: the same
    /// refusal, recorded with its path so the terminal gap names what was refused.
    fn endpoint_denied(&mut self, ctx: &ToolCtx, endpoint: &str, reason: String) -> ToolError {
        self.refused(ctx, Some(endpoint), reason)
    }

    fn refused(&mut self, ctx: &ToolCtx, endpoint: Option<&str>, reason: String) -> ToolError {
        use crate::agent::task_capabilities::{CapabilityDenied, CapabilityRefusals, TypedToolClass};
        let refusals = ctx.turn.extension_data.get_or_init(CapabilityRefusals::default);
        match endpoint {
            Some(endpoint) => refusals.record_target("apply_patch", TypedToolClass::StructuredEdit, endpoint),
            None => refusals.record("apply_patch", TypedToolClass::StructuredEdit),
        }
        let failure = CapabilityDenied::new("apply_patch", reason).tool_failure();
        let message = failure.message.clone();
        self.failure = Some(failure);
        ToolError::Rejected(message)
    }

    /// The assignment-wide refusal, if any, for a patch that has not reached this
    /// runtime. A shell-relayed patch passes the router as a shell call, so its
    /// handler asks here before safety assessment and approval, as the router's
    /// gate does for the apply_patch tool. That gate cannot see the selected
    /// environment, so the tool's own handler asks too once it is resolved.
    /// Recorded like every capability refusal.
    pub(crate) async fn assignment_refusal(ctx: &ToolCtx, environment: &TurnEnvironment)
        -> Option<codex_protocol::tool_failure::ToolFailure>
    {
        let mut runtime = Self::new();
        runtime.write_authority(ctx, environment).await.err()?;
        runtime.failure
    }

    /// Whether this turn's assignment may patch source at all. `Ok(None)` is a turn
    /// no typed assignment governs; a bound turn returns the authorization its
    /// per-endpoint checks need.
    async fn write_authority(&mut self, ctx: &ToolCtx, environment: &TurnEnvironment)
        -> Result<Option<AgentTaskAuthorization>, ToolError>
    {
        let coordinator = ctx.session.services.agent_control.task_coordinator();
        let Some(binding) = coordinator.binding_for_turn(&ctx.turn) else {
            if matches!(&ctx.turn.session_source, codex_protocol::protocol::SessionSource::SubAgent(_)) {
                return Err(self.capability_denied(ctx,
                    "unbound subagents are read-only; admit an explicit typed writable assignment".into()));
            }
            return Ok(None);
        };
        let authorization = coordinator.get_agent_task_authorization(binding.assignment_id).await
            .map_err(|error| ToolError::Rejected(format!("apply_patch: typed assignment unavailable: {error}")))?;
        if authorization.current_attempt.attempt_id != binding.attempt_id
            || authorization.current_attempt.state != AttemptState::Active
        {
            return Err(ToolError::Rejected("apply_patch: bound assignment attempt is no longer active".into()));
        }
        let assignment = &authorization.assignment;
        let amendment = authorization.current_attempt.amendment.as_ref();
        if let Err(error) = codex_agent_task_store::require_write_contract(assignment, amendment) {
            return Err(self.capability_denied(ctx, error.to_string()));
        }
        // Read-only attempts intentionally carry no source baseline; report the
        // authority gap rather than a missing baseline (shell-intercepted patches
        // reach this owner without the router's StructuredEdit check).
        if assignment.write_scope.is_empty() {
            return Err(self.capability_denied(ctx,
                "message-only/read-only assignments cannot write source".into()));
        }
        if let Err(error) = codex_agent_task_store::dispatch_manifest(assignment, amendment) {
            return Err(self.capability_denied(ctx, error.to_string()));
        }
        // A writable contract is bound to its local repository, so no retry can
        // patch a remote environment. Checked last: a read-only assignment is
        // refused for its own reason above, whatever its environment. Recorded
        // against that environment: the assignment may still patch locally.
        if environment.environment.is_remote() {
            let target = format!("remote environment {}", environment.environment_id);
            return Err(self.refused(ctx, Some(&target),
                "typed source contracts require the bound local repository".into()));
        }
        Ok(Some(authorization))
    }

    async fn validate_assignment(&mut self, ctx: &ToolCtx, req: &ApplyPatchRequest) -> Result<(), ToolError> {
        let Some(authorization) = self.write_authority(ctx, &req.turn_environment).await? else {
            return Ok(());
        };
        // Keep the verified action's iteration order without copying patch contents.
        let endpoints = req
            .action
            .changes()
            .iter()
            .map(|(path, change)| {
                let (operation, move_path) = match change {
                    ApplyPatchFileChange::Add { .. } => (SourceWriteOperation::Create, None),
                    ApplyPatchFileChange::Delete { .. } => (SourceWriteOperation::Delete, None),
                    ApplyPatchFileChange::Update { move_path, .. } => {
                        (SourceWriteOperation::Update, move_path.clone())
                    }
                };
                AssignmentEndpointCheck { path: path.clone(), operation, move_path }
            })
            .collect::<Vec<_>>();
        let permit = self.workspace_operation_permit.clone();
        let result = tokio::task::spawn_blocking(move || {
            // The patch owner awaits these read-only checks before mutation.
            // Keep the same admission alive even if its waiter is dropped.
            let _permit = permit;
            Self::check_assignment_endpoints(&authorization, endpoints)
        })
        .await
        .map_err(|error| ToolError::Rejected(format!("apply_patch: endpoint validation task failed: {error}")))?;
        match result {
            Ok(()) => Ok(()),
            Err(AssignmentEndpointError::Refused { endpoint, reason }) => {
                Err(self.endpoint_denied(ctx, &endpoint, reason))
            }
            Err(AssignmentEndpointError::Rejected(reason)) => Err(ToolError::Rejected(reason)),
        }
    }

    fn check_assignment_endpoints(
        authorization: &AgentTaskAuthorization,
        endpoints: Vec<AssignmentEndpointCheck>,
    ) -> Result<(), AssignmentEndpointError> {
        use SourceWriteOperation as Op;

        let assignment = &authorization.assignment;
        let amendment = authorization.current_attempt.amendment.as_ref();
        let root = Path::new(&authorization.repo_root);
        let normalize = |absolute: &Path| {
            // Scope violations are capability refusals; filesystem errors are not.
            codex_agent_task_store::normalize_absolute_source_path(root, absolute).map_err(|error| {
                match error {
                    error @ StoreError::InvalidScope(_) => AssignmentEndpointError::Refused {
                        endpoint: absolute.display().to_string(),
                        reason: error.to_string(),
                    },
                    error => AssignmentEndpointError::Rejected(format!("apply_patch: {error}")),
                }
            })
        };
        let authorize = |path: &str, operation| {
            codex_agent_task_store::authorize_source_write(assignment, amendment, path, operation)
                .map_err(|error| AssignmentEndpointError::Refused {
                    endpoint: path.to_owned(),
                    reason: error.to_string(),
                })
        };

        for AssignmentEndpointCheck { path, operation, move_path } in endpoints {
            let absolute = path.to_abs_path()
                .map_err(|error| AssignmentEndpointError::Rejected(error.to_string()))?;
            let relative = normalize(&absolute)?;
            authorize(&relative, operation)?;
            if let Some(destination) = move_path {
                let absolute = destination.to_abs_path()
                    .map_err(|error| AssignmentEndpointError::Rejected(error.to_string()))?;
                let destination = normalize(&absolute)?;
                let destination_operation = if absolute.exists() { Op::Update } else { Op::Create };
                for (path, operation) in [(&relative, Op::Move), (&relative, Op::Delete),
                    (&destination, Op::Move), (&destination, destination_operation)]
                {
                    authorize(path, operation)?;
                }
            }
        }
        Ok(())
    }


    fn file_system_sandbox_context_for_attempt(
        _req: &ApplyPatchRequest,
        _attempt: &SandboxAttempt<'_>,
    ) -> Option<FileSystemSandboxContext> {
        // Compatibility permission hints never install a sandbox on either
        // local or remote executors. Source-write scope checks remain separate.
        None
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
                self.workspace_operation_permit = Some(Arc::new(tokio::select! {
                    biased;
                    _ = req.cancellation_token.cancelled() => {
                        return Err(ToolError::Codex(CodexErr::TurnAborted));
                    }
                    permit = crate::workspace_operation_gate::acquire_patch_operation_with_timeout(
                        &req.turn_environment.environment,
                        &req.action.cwd,
                    ) => permit.map_err(ToolError::Denied)?,
                }));
            }
            self.validate_assignment(ctx, req).await?;
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
            let (delta, failure_kind) = match result {
                Ok(delta) => (delta, "none"),
                Err(failure) => {
                    let (error, delta) = failure.into_parts();
                    self.failure = Some(patch_failure(&error));
                    self.revision_guard_mismatch = error.is_revision_guard_mismatch();
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
                        _ => {}
                    }
                    let kind = patch_failure_kind(&error);
                    (delta, kind)
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
        _ if error.is_revision_guard_mismatch() => ToolFailureCode::StaleInput,
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
    if let ApplyPatchError::ParseError(parse_error) = error {
        // The typed parser error names the failed rule and line. Keep the stable
        // message and `line` detail; add the owner's bounded text beside them.
        failure.details.get_or_insert_with(|| serde_json::json!({}))["owner_diagnostic"] =
            serde_json::Value::String(codex_utils_string::truncate_middle_chars(&parse_error.to_string(), 1_024));
    }
    if let ApplyPatchError::RevisionGuardMismatch(diagnostic) = error {
        // The owner already bounds each available source excerpt; preserve its
        // expected/current identities on shell interception as well as direct calls.
        failure.details = Some(serde_json::json!({"revision_guard_diagnostic": diagnostic}));
    }
    if let ApplyPatchError::MultiplePatchConflicts {
        report,
        revision_guard_mismatch: true,
        ..
    } = error {
        failure.details.get_or_insert_with(|| serde_json::json!({}))["revision_guard_diagnostic"] =
            serde_json::Value::String(report.clone());
    }
    if let ApplyPatchError::ComputeReplacements(diagnostic) = error {
        failure.details = Some(serde_json::json!({"owner_diagnostic": diagnostic}));
    }
    failure
}
