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
use crate::task_evidence::OwnerPacketBinding;
use crate::task_evidence::OwnerPacketChangeRegion;
use crate::task_evidence::OwnerPacketPostMutationPath;
use crate::task_evidence::TaskEvidenceLedger;
use crate::tools::command_execution::WorkspaceMutationScope;
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
use codex_agent_task_store::StoreError;
use codex_agent_task_store::WorkspaceActorKind;
use codex_agent_task_store::WorkspaceMutationLease;
use codex_agent_task_store::WorkspaceMutationRequest;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchFileChange;
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
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
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
    owner_packets: Option<(TaskEvidenceLedger, Vec<OwnerPacketBinding>)>,
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
        attempt: &SandboxAttempt<'_>,
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
        let (packet_ledger, _) = ctx
            .session
            .services
            .agent_control
            .completion_evidence_target(
                &ctx.turn.session_source,
                ctx.session.thread_id,
                &ctx.session.services.task_evidence,
            )
            .await;
        let owner_regions = owner_change_regions(req, &repo_root)?;
        let packet_bindings = packet_ledger
            .prepare_owner_patch(&owner_regions)
            .await
            .map_err(|reason| {
                let reason = serde_json::to_string(&reason)
                    .unwrap_or_else(|_| "{\"kind\":\"packet_not_ready\"}".to_string());
                ToolError::Rejected(format!("apply_patch: {reason}"))
            })?;
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
        let mutation_scope = WorkspaceMutationScope::exact_paths(normalized_repo_paths);
        #[cfg(test)]
        ctx.session
            .services
            .command_execution
            .record_workspace_mutation_scope(&mutation_scope, false);
        let mutation_paths = mutation_scope.into_paths().ok_or_else(|| {
            ToolError::Rejected(
                "apply_patch: exact mutation scope unexpectedly lacked paths".to_string(),
            )
        })?;
        let expected_manifest = store
            .supporting_read_manifest(&repo_root, actor_id.clone(), mutation_paths.clone())
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
                    actor_id: actor_id.clone(),
                    kind,
                    attempt_id: binding.as_ref().map(|binding| binding.attempt_id),
                    paths: mutation_paths,
                    contracts: task
                        .as_ref()
                        .map(|task| task.assignment.contract_claims.clone())
                        .unwrap_or_default(),
                    expected_manifest,
                },
            )
            .await;
        let lease = match lease {
            Ok(lease) => lease,
            Err(StoreError::WorkspaceCasMismatch { details }) => {
                let self_owned = !details.is_empty()
                    && details
                        .iter()
                        .all(|detail| detail.last_writer.as_deref() == Some(actor_id.as_str()));
                let paths = details
                    .iter()
                    .map(|detail| detail.path.clone())
                    .collect::<Vec<_>>();
                packet_ledger
                    .record_owner_cas_failure(&paths, self_owned)
                    .await;
                ctx.turn.session_telemetry.counter(
                    "codex.owner_packet.cas_failure",
                    1,
                    &[(("ownership"), if self_owned { "self" } else { "external" })],
                );
                let diagnostic =
                    cas_mismatch_diagnostic(req, attempt, &repo_root, &owner_regions, &details)
                        .await;
                return Err(ToolError::Rejected(format!(
                    "apply_patch: workspace_cas_mismatch {diagnostic}"
                )));
            }
            Err(error) => {
                return Err(ToolError::Rejected(format!(
                    "apply_patch: workspace mutation was rejected: {error}"
                )));
            }
        };
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
        self.owner_packets = Some((packet_ledger, packet_bindings));
        self.typed_mutations_started = true;
        Ok(())
    }

    async fn finish_workspace_mutation(
        &mut self,
        req: &ApplyPatchRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<(), ToolError> {
        let Some((store, repo_root, lease)) = self.workspace_mutation.take() else {
            self.typed_mutations_started = false;
            return Ok(());
        };
        let result = store
            .finish_workspace_mutation_with_receipt(&repo_root, lease)
            .await;
        self.typed_mutations_started = false;
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Some((ledger, bindings)) = self.owner_packets.take() {
                    let metrics = ledger
                        .record_owner_patch_finalization(&ctx.call_id, &bindings, &[])
                        .await;
                    emit_owner_packet_transition_metrics(&ctx.turn.session_telemetry, &metrics);
                }
                return Err(ToolError::Rejected(format!(
                    "apply_patch: workspace mutation finalization failed: {error}"
                )));
            }
        };
        #[cfg(test)]
        ctx.session
            .services
            .git_workspace
            .record_final_manifest_work(outcome.work());
        ctx.session
            .services
            .git_workspace
            .note_host_workspace_mutation_paths(&repo_root, &outcome.result().changed_paths);
        if let Some((ledger, bindings)) = self.owner_packets.take() {
            let post_paths = collect_post_mutation_paths(
                req,
                attempt,
                &repo_root,
                outcome
                    .final_manifest()
                    .map(|manifest| manifest.receipt().entries()),
            )
            .await;
            let metrics = ledger
                .record_owner_patch_finalization(&ctx.call_id, &bindings, &post_paths)
                .await;
            emit_owner_packet_transition_metrics(&ctx.turn.session_telemetry, &metrics);
        }
        Ok(())
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

fn emit_owner_packet_transition_metrics(
    telemetry: &codex_otel::SessionTelemetry,
    metrics: &crate::task_evidence::OwnerPacketTransitionMetrics,
) {
    for (name, value) in [
        ("codex.owner_packet.edits", metrics.edits),
        (
            "codex.owner_packet.same_region_revisits",
            metrics.same_region_revisits,
        ),
        ("codex.owner_packet.caller_repairs", metrics.caller_repairs),
        (
            "codex.owner_packet.acceptance_repairs",
            metrics.acceptance_repairs,
        ),
        (
            "codex.owner_packet.lifecycle_repairs",
            metrics.lifecycle_repairs,
        ),
        (
            "codex.owner_packet.formatting_only_repairs",
            metrics.formatting_only_repairs,
        ),
        ("codex.owner_packet.other_repairs", metrics.other_repairs),
        (
            "codex.owner_packet.identity_refreshes",
            metrics.identity_refreshes,
        ),
        (
            "codex.owner_packet.interval_invalidations",
            metrics.interval_invalidations,
        ),
        ("codex.owner_packet.span_refreshes", metrics.span_refreshes),
    ] {
        if value > 0 {
            telemetry.counter(name, i64::try_from(value).unwrap_or(i64::MAX), &[]);
        }
    }
}

fn owner_change_regions(
    req: &ApplyPatchRequest,
    repo_root: &std::path::Path,
) -> Result<Vec<OwnerPacketChangeRegion>, ToolError> {
    let mut regions = Vec::new();
    for (path, change) in req.action.changes() {
        let absolute = path.to_abs_path().map_err(|error| {
            ToolError::Rejected(format!(
                "apply_patch: packet path is not local and cannot be bound: {error}"
            ))
        })?;
        let normalized =
            normalize_absolute_repo_path(repo_root, absolute.as_path()).map_err(|error| {
                ToolError::Rejected(format!("apply_patch: packet path is invalid: {error}"))
            })?;
        let (start_line, end_line) = match change {
            ApplyPatchFileChange::Add { content } | ApplyPatchFileChange::Delete { content } => {
                (1, content.lines().count().max(1))
            }
            ApplyPatchFileChange::Update {
                unified_diff,
                new_content,
                ..
            } => changed_line_range(unified_diff)
                .unwrap_or_else(|| (1, new_content.lines().count().max(1))),
        };
        regions.push(OwnerPacketChangeRegion {
            path: normalized,
            start_line,
            end_line,
        });
        if let ApplyPatchFileChange::Update {
            move_path: Some(move_path),
            new_content,
            ..
        } = change
        {
            let absolute = move_path.to_abs_path().map_err(|error| {
                ToolError::Rejected(format!(
                    "apply_patch: moved packet path is not local: {error}"
                ))
            })?;
            let normalized =
                normalize_absolute_repo_path(repo_root, absolute.as_path()).map_err(|error| {
                    ToolError::Rejected(format!(
                        "apply_patch: moved packet path is invalid: {error}"
                    ))
                })?;
            regions.push(OwnerPacketChangeRegion {
                path: normalized,
                start_line: 1,
                end_line: new_content.lines().count().max(1),
            });
        }
    }
    regions.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.start_line.cmp(&right.start_line))
    });
    Ok(regions)
}

fn changed_line_range(unified_diff: &str) -> Option<(usize, usize)> {
    let mut first = None::<usize>;
    let mut last = 0usize;
    for line in unified_diff.lines().filter(|line| line.starts_with("@@ ")) {
        let new_range = line.split_whitespace().nth(2)?.strip_prefix('+')?;
        let (start, count) = new_range
            .split_once(',')
            .map_or((new_range, "1"), |(start, count)| (start, count));
        let start = start.parse::<usize>().ok()?.max(1);
        let count = count.parse::<usize>().ok()?;
        first = Some(first.map_or(start, |current| current.min(start)));
        last = last.max(start.saturating_add(count.saturating_sub(1)).max(start));
    }
    first.map(|first| (first, last.max(first)))
}

async fn cas_mismatch_diagnostic(
    req: &ApplyPatchRequest,
    attempt: &SandboxAttempt<'_>,
    repo_root: &std::path::Path,
    regions: &[OwnerPacketChangeRegion],
    details: &[codex_agent_task_store::WorkspaceCasMismatchDetail],
) -> String {
    let fs = req.turn_environment.environment.get_filesystem();
    let sandbox = ApplyPatchRuntime::file_system_sandbox_context_for_attempt(req, attempt);
    let root_uri = PathUri::from_host_native_path(repo_root).ok();
    let action_paths = req
        .action
        .changes()
        .keys()
        .filter_map(|path| {
            let absolute = path.to_abs_path().ok()?;
            let normalized = normalize_absolute_repo_path(repo_root, absolute.as_path()).ok()?;
            Some((normalized, path.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut rendered = Vec::new();
    for detail in details.iter().take(8) {
        let excerpt = match (action_paths.get(&detail.path), root_uri.as_ref()) {
            (Some(path), Some(root_uri)) => fs
                .read_file_bounded_confined(path, root_uri, 4 * 1024 * 1024, sandbox.as_ref())
                .await
                .ok()
                .flatten()
                .and_then(|bytes| {
                    let region = regions.iter().find(|region| region.path == detail.path)?;
                    current_excerpt(&bytes, region.start_line, region.end_line)
                }),
            _ => None,
        };
        rendered.push(serde_json::json!({
            "path": detail.path,
            "expected": detail.expected.as_ref().map(|entry| serde_json::json!({
                "exists": entry.existed,
                "sha256": entry.content_hash,
            })),
            "current": detail.current.as_ref().map(|entry| serde_json::json!({
                "exists": entry.existed,
                "sha256": entry.content_hash,
            })),
            "current_epoch": detail.current_epoch,
            "last_writer": detail.last_writer,
            "current_excerpt": excerpt,
        }));
    }
    serde_json::to_string(&serde_json::json!({ "details": rendered }))
        .unwrap_or_else(|_| "{\"details\":[]}".to_string())
}

fn current_excerpt(bytes: &[u8], start_line: usize, end_line: usize) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let lines = text.lines().collect::<Vec<_>>();
    let excerpt_start = start_line.saturating_sub(5).max(1);
    let excerpt_end = end_line.saturating_add(5).min(lines.len());
    if excerpt_start > excerpt_end {
        return None;
    }
    let mut excerpt = lines[excerpt_start - 1..excerpt_end].join("\n");
    if excerpt.len() > 4096 {
        excerpt.truncate(4096);
    }
    Some(excerpt)
}

async fn collect_post_mutation_paths(
    req: &ApplyPatchRequest,
    attempt: &SandboxAttempt<'_>,
    repo_root: &std::path::Path,
    entries: Option<&[codex_agent_task_store::WorkspaceManifestEntry]>,
) -> Vec<OwnerPacketPostMutationPath> {
    let Some(entries) = entries else {
        return Vec::new();
    };
    let fs = req.turn_environment.environment.get_filesystem();
    let sandbox = ApplyPatchRuntime::file_system_sandbox_context_for_attempt(req, attempt);
    let root_uri = PathUri::from_host_native_path(repo_root).ok();
    let action_paths = req
        .file_paths
        .iter()
        .filter_map(|path| {
            let absolute = path.to_abs_path().ok()?;
            let normalized = normalize_absolute_repo_path(repo_root, absolute.as_path()).ok()?;
            Some((normalized, path.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut post_paths = Vec::with_capacity(entries.len());
    for entry in entries {
        let content = if entry.existed {
            match (action_paths.get(&entry.path), root_uri.as_ref()) {
                (Some(path), Some(root_uri)) => fs
                    .read_file_bounded_confined(path, root_uri, 4 * 1024 * 1024, sandbox.as_ref())
                    .await
                    .ok()
                    .flatten()
                    .filter(|bytes| {
                        entry.content_hash.as_deref()
                            == Some(format!("{:x}", Sha256::digest(bytes)).as_str())
                    }),
                _ => None,
            }
        } else {
            None
        };
        post_paths.push(OwnerPacketPostMutationPath {
            path: entry.path.clone(),
            existed: entry.existed,
            file_sha256: entry.content_hash.clone(),
            content,
        });
    }
    post_paths
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
        self.begin_workspace_mutations(req, attempt, ctx).await?;
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
        self.finish_workspace_mutation(req, attempt, ctx).await?;
        if failed && is_likely_sandbox_denied(attempt.sandbox, &output) {
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
}

#[cfg(test)]
#[path = "apply_patch_tests.rs"]
mod tests;
