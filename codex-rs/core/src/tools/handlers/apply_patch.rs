use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::FunctionCallError;
use crate::apply_patch;
use crate::apply_patch::InternalApplyPatchInvocation;
use crate::apply_patch::convert_apply_patch_to_protocol;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::command_execution::CommandExecutionLedger;
use crate::tools::command_execution::InputStateDetermined;
use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::context::ApplyPatchToolOutput;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::handlers::apply_patch_retries;
use crate::tools::handlers::apply_patch_spec::create_apply_patch_freeform_tool;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::updated_hook_command;
use crate::tools::hook_names::HookToolName;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::registry::ToolExecutionTiming;
use crate::tools::registry::ToolExecutor;
use crate::tools::runtimes::apply_patch::ApplyPatchRequest;
use crate::tools::runtimes::apply_patch::ApplyPatchRuntime;
use crate::tools::sandboxing::ToolCtx;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchFileChange;
use codex_apply_patch::Hunk;
use codex_apply_patch::ParseError;
use codex_apply_patch::StreamingPatchParser;
use codex_exec_server::ExecutorFileSystem;
use codex_features::Feature;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::FileSystemPermissions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyUpdatedEvent;
use codex_sandboxing::policy_transforms::effective_file_system_sandbox_policy;
use codex_sandboxing::policy_transforms::merge_uri_permission_profiles;
use codex_sandboxing::policy_transforms::normalize_additional_permissions;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;

const APPLY_PATCH_ARGUMENT_DIFF_BUFFER_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) struct ApplyPatchInterceptionError {
    error: FunctionCallError,
    proof: Option<InputStateDetermined>,
}

impl ApplyPatchInterceptionError {
    fn correctness(error: codex_apply_patch::ApplyPatchError) -> Self {
        let proof = match &error {
            codex_apply_patch::ApplyPatchError::ImplicitInvocation => {
                Some(InputStateDetermined::ApplyPatchImplicitInvocation)
            }
            codex_apply_patch::ApplyPatchError::EnvironmentIdMismatch { .. } => {
                Some(InputStateDetermined::ApplyPatchEnvironmentIdMismatch)
            }
            _ => None,
        };
        Self {
            error: FunctionCallError::RespondToModel(format!(
                "apply_patch verification failed: {error}"
            )),
            proof,
        }
    }

    pub(crate) async fn record_attempt_failure(
        &self,
        ledger: &CommandExecutionLedger,
        key: &CommandAttemptKey,
    ) {
        if let Some(proof) = self.proof {
            ledger
                .record_input_state_determined_failure(
                    key,
                    proof,
                    RawOutputArtifact::unavailable(proof.evidence_description()),
                    -1,
                )
                .await;
        } else {
            ledger.record_exit(key, -1).await;
        }
    }

    pub(crate) fn into_error(self) -> FunctionCallError {
        self.error
    }
}

impl From<FunctionCallError> for ApplyPatchInterceptionError {
    fn from(error: FunctionCallError) -> Self {
        Self { error, proof: None }
    }
}

/// Handles freeform `apply_patch` requests and routes verified patches to the
/// selected environment filesystem.
#[derive(Default)]
pub struct ApplyPatchHandler {
    multi_environment: bool,
}

impl ApplyPatchHandler {
    pub(crate) fn new(multi_environment: bool) -> Self {
        Self { multi_environment }
    }
}

#[derive(Default)]
struct ApplyPatchArgumentDiffConsumer {
    parser: StreamingPatchParser,
    input: String,
    parse_error: Option<ParseError>,
    last_sent_at: Option<Instant>,
    pending: Option<String>,
}

impl ToolArgumentDiffConsumer for ApplyPatchArgumentDiffConsumer {
    fn consume_diff(
        &mut self,
        turn: &TurnContext,
        call_id: String,
        diff: &str,
    ) -> Option<EventMsg> {
        if !turn
            .config
            .features
            .enabled(Feature::ApplyPatchStreamingEvents)
        {
            return None;
        }

        self.push_delta(call_id, diff)
            .map(EventMsg::PatchApplyUpdated)
    }

    fn finish(&mut self) -> Result<Option<EventMsg>, FunctionCallError> {
        self.finish_update_on_complete()
            .map(|event| event.map(EventMsg::PatchApplyUpdated))
    }
}

impl ApplyPatchArgumentDiffConsumer {
    fn push_delta(&mut self, call_id: String, delta: &str) -> Option<PatchApplyUpdatedEvent> {
        self.input.push_str(delta);
        if apply_patch_retries::is_retry(&self.input) || self.parse_error.is_some() {
            return None;
        }
        match self.parser.push_delta_in_place(delta) {
            Ok(()) => {}
            Err(err) => {
                self.parse_error = Some(err);
                self.pending = None;
                return None;
            }
        }
        if self.parser.hunks().is_empty() {
            return None;
        }

        let now = Instant::now();
        match self.last_sent_at {
            Some(last_sent_at)
                if now.duration_since(last_sent_at) < APPLY_PATCH_ARGUMENT_DIFF_BUFFER_INTERVAL =>
            {
                self.pending = Some(call_id);
                None
            }
            Some(_) | None => {
                self.pending = None;
                self.last_sent_at = Some(now);
                Some(PatchApplyUpdatedEvent {
                    call_id,
                    changes: convert_apply_patch_hunks_to_protocol(self.parser.hunks()),
                })
            }
        }
    }

    fn finish_update_on_complete(
        &mut self,
    ) -> Result<Option<PatchApplyUpdatedEvent>, FunctionCallError> {
        if apply_patch_retries::is_retry(&self.input) {
            apply_patch_retries::validate_retry(&self.input)
                .map_err(FunctionCallError::RespondToModel)?;
            return Ok(None);
        }
        if let Some(err) = &self.parse_error {
            return Err(FunctionCallError::RespondToModel(format!(
                "failed to parse apply_patch: {err}"
            )));
        }

        if let Err(err) = self.parser.finish_in_place() {
            self.pending = None;
            let response =
                FunctionCallError::RespondToModel(format!("failed to parse apply_patch: {err}"));
            self.parse_error = Some(err);
            return Err(response);
        }

        let event = self.pending.take().map(|call_id| PatchApplyUpdatedEvent {
            call_id,
            changes: convert_apply_patch_hunks_to_protocol(self.parser.hunks()),
        });
        if event.is_some() {
            self.last_sent_at = Some(Instant::now());
        }
        Ok(event)
    }
}

fn convert_apply_patch_hunks_to_protocol(hunks: &[Hunk]) -> HashMap<PathBuf, FileChange> {
    hunks
        .iter()
        .map(|hunk| {
            let path = hunk_source_path(hunk).to_path_buf();
            let change = match hunk {
                Hunk::AddFile { contents, .. } => FileChange::Add {
                    content: contents.clone(),
                },
                Hunk::DeleteFile { .. } => FileChange::Delete {
                    content: String::new(),
                },
                Hunk::UpdateFile {
                    chunks, move_path, ..
                } => FileChange::Update {
                    unified_diff: format_update_chunks_for_progress(chunks),
                    move_path: move_path.clone(),
                },
            };
            (path, change)
        })
        .collect()
}

fn hunk_source_path(hunk: &Hunk) -> &Path {
    match hunk {
        Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } | Hunk::UpdateFile { path, .. } => {
            path
        }
    }
}

fn format_update_chunks_for_progress(chunks: &[codex_apply_patch::UpdateFileChunk]) -> String {
    let mut unified_diff = String::new();
    for chunk in chunks {
        match &chunk.change_context {
            Some(context) => {
                unified_diff.push_str("@@ ");
                unified_diff.push_str(context);
                unified_diff.push('\n');
            }
            None => {
                unified_diff.push_str("@@");
                unified_diff.push('\n');
            }
        }
        for line in &chunk.old_lines {
            unified_diff.push('-');
            unified_diff.push_str(line);
            unified_diff.push('\n');
        }
        for line in &chunk.new_lines {
            unified_diff.push('+');
            unified_diff.push_str(line);
            unified_diff.push('\n');
        }
        if chunk.is_end_of_file {
            unified_diff.push_str("*** End of File");
            unified_diff.push('\n');
        }
    }
    unified_diff
}

fn file_paths_for_action(action: &ApplyPatchAction) -> Vec<PathUri> {
    let mut keys = Vec::new();
    for (path, change) in action.changes() {
        keys.push(path.clone());

        if let ApplyPatchFileChange::Update { move_path, .. } = change
            && let Some(dest) = move_path
        {
            keys.push(dest.clone());
        }
    }

    keys
}

fn write_permissions_for_paths(
    file_paths: &[AbsolutePathBuf],
    file_system_sandbox_policy: &codex_protocol::permissions::FileSystemSandboxPolicy,
    cwd: &AbsolutePathBuf,
) -> Option<AdditionalPermissionProfile> {
    let write_paths = file_paths
        .iter()
        .map(|path| {
            path.parent()
                .unwrap_or_else(|| path.clone())
                .into_path_buf()
        })
        .filter(|path| {
            !file_system_sandbox_policy.can_write_path_with_cwd(path.as_path(), cwd.as_path())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(AbsolutePathBuf::from_absolute_path)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;

    let permissions = (!write_paths.is_empty()).then_some(AdditionalPermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(write_paths),
        )),
        ..Default::default()
    })?;

    normalize_additional_permissions(permissions).ok()
}

/// Extracts the raw patch text used as the command-shaped hook input for apply_patch.
fn apply_patch_payload_command(payload: &ToolPayload) -> Option<String> {
    match payload {
        ToolPayload::Custom { input } => Some(input.clone()),
        _ => None,
    }
}

async fn effective_patch_permissions(
    session: &Session,
    turn: &TurnContext,
    approval_scope_id: &str,
    action: &ApplyPatchAction,
    cwd: &PathUri,
) -> std::io::Result<(
    Vec<PathUri>,
    crate::tools::handlers::EffectiveAdditionalPermissions,
    codex_protocol::permissions::FileSystemSandboxPolicy,
)> {
    let file_paths = file_paths_for_action(action);
    let native_cwd = match cwd.to_abs_path() {
        Ok(native_cwd) => native_cwd,
        Err(error) => {
            return external_patch_permissions(turn, file_paths).ok_or(error);
        }
    };
    let granted_permissions = merge_uri_permission_profiles(
        session
            .granted_session_permissions(approval_scope_id)
            .await
            .as_ref(),
        session
            .granted_turn_permissions(approval_scope_id)
            .await
            .as_ref(),
    );
    let granted_permissions = granted_permissions
        .map(AdditionalPermissionProfile::try_from)
        .transpose()?;
    let base_file_system_sandbox_policy = turn.file_system_sandbox_policy();
    let file_system_sandbox_policy = effective_file_system_sandbox_policy(
        &base_file_system_sandbox_policy,
        granted_permissions.as_ref(),
    );
    let native_file_paths = match file_paths
        .iter()
        .map(PathUri::to_abs_path)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(native_file_paths) => native_file_paths,
        Err(error) => {
            return external_patch_permissions(turn, file_paths).ok_or(error);
        }
    };
    let effective_additional_permissions = apply_granted_turn_permissions(
        session,
        approval_scope_id,
        native_cwd.as_path(),
        crate::sandboxing::SandboxPermissions::UseDefault,
        write_permissions_for_paths(&native_file_paths, &file_system_sandbox_policy, &native_cwd),
    )
    .await;

    Ok((
        file_paths,
        effective_additional_permissions,
        file_system_sandbox_policy,
    ))
}

fn external_patch_permissions(
    turn: &TurnContext,
    file_paths: Vec<PathUri>,
) -> Option<(
    Vec<PathUri>,
    crate::tools::handlers::EffectiveAdditionalPermissions,
    codex_protocol::permissions::FileSystemSandboxPolicy,
)> {
    matches!(
        turn.permission_profile(),
        PermissionProfile::External { .. }
    )
    .then(|| {
        (
            file_paths,
            crate::tools::handlers::EffectiveAdditionalPermissions {
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None,
                additional_permissions_uri: None,
                permissions_preapproved: false,
            },
            codex_protocol::permissions::FileSystemSandboxPolicy::external_sandbox(),
        )
    })
}

impl ToolExecutor<ToolInvocation> for ApplyPatchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("apply_patch")
    }

    fn spec(&self) -> ToolSpec {
        create_apply_patch_freeform_tool(self.multi_environment)
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let cancellation = invocation.cancellation_token.clone();
            match self.handle_call(invocation).await {
                Err(FunctionCallError::RespondToModel(text)) if !cancellation.is_cancelled() => {
                    Ok(boxed_tool_output(ApplyPatchToolOutput::from_delta(
                        text,
                        false,
                        &Default::default(),
                        None,
                    )))
                }
                result => result,
            }
        })
    }
}

impl ApplyPatchHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            tracker,
            call_id,
            tool_name,
            payload,
            cancellation_token,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);

        let ToolPayload::Custom { input: patch_input } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "apply_patch handler received unsupported payload".to_string(),
            ));
        };
        let retry = if apply_patch_retries::is_retry(&patch_input) {
            Some(
                session
                    .services
                    .retained_patches
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .prepare(&patch_input)
                    .map_err(FunctionCallError::RespondToModel)?,
            )
        } else {
            None
        };
        let (retry_id, retry_scope, parsed) = match retry {
            Some(retry) => (
                Some(retry.id),
                Some((retry.environment_id, retry.cwd)),
                Ok(retry.args),
            ),
            None => (None, None, codex_apply_patch::parse_patch(&patch_input)),
        };
        let args = match parsed {
            Ok(args) => args,
            Err(parse_error) => {
                session.services.session_telemetry.counter(
                    "codex.apply_patch.verification_failed",
                    1,
                    &[("failure_kind", "parse")],
                );
                return Err(FunctionCallError::RespondToModel(format!(
                    "apply_patch verification failed: {parse_error}"
                )));
            }
        };
        let files_requested = args
            .hunks
            .iter()
            .map(|hunk| match hunk {
                Hunk::AddFile { path, .. }
                | Hunk::DeleteFile { path }
                | Hunk::UpdateFile { path, .. } => path,
            })
            .collect::<BTreeSet<_>>()
            .len() as i64;
        let chunks_requested = args
            .hunks
            .iter()
            .map(|hunk| match hunk {
                // A whole-file add/delete or a move without a text chunk is one operation.
                Hunk::UpdateFile { chunks, .. } => chunks.len().max(1) as i64,
                _ => 1,
            })
            .sum();
        let retained_input = args.patch.clone();
        let cancellation = cancellation_token.clone();
        let result = async {
            let selected_environment_id =
                require_environment_id(args.environment_id.as_deref(), self.multi_environment)?;

            // Verify the parsed patch against the selected environment filesystem.
            let Some(turn_environment) = resolve_tool_environment(
                &step_context.environments,
                selected_environment_id.as_deref(),
            )?
            else {
                return Err(FunctionCallError::RespondToModel(
                    "apply_patch requires a ready execution environment. If an environment is starting, call wait_for_environment with its id first.".to_string(),
                ));
            };
            if retry_scope.as_ref().is_some_and(|(id, cwd)| {
                id != &turn_environment.environment_id || cwd != turn_environment.cwd()
            }) {
                return Err(FunctionCallError::RespondToModel(
                    "retained patch belongs to a different execution environment or working directory".into(),
                ));
            }
            let fs = turn_environment.environment.get_filesystem();
            let sandbox = turn.file_system_sandbox_context(
                /*additional_permissions*/ None,
                turn_environment.cwd(),
            );
            let workspace_operation_permit = acquire_patch_workspace(
                turn_environment,
                turn_environment.cwd(),
                &cancellation_token,
            )
            .await?;
            if let Some(id) = &retry_id {
                session.services.retained_patches.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .consume(id).map_err(FunctionCallError::RespondToModel)?;
            }
            match codex_apply_patch::verify_apply_patch_args(
                args,
                turn_environment.cwd(),
                fs.as_ref(),
                Some(&sandbox),
            )
            .await
            {
                codex_apply_patch::MaybeApplyPatchVerified::Body(changes) => {
                    let (file_paths, effective_additional_permissions, file_system_sandbox_policy) =
                        effective_patch_permissions(
                            session.as_ref(),
                            turn.as_ref(),
                            turn_environment.environment.approval_scope_id(),
                            &changes,
                            turn_environment.cwd(),
                        )
                        .await
                        .map_err(|error| {
                            FunctionCallError::RespondToModel(format!(
                                "apply_patch cannot enforce filesystem permissions for the selected environment: {error}"
                            ))
                        })?;
                    let invocation =
                        apply_patch::apply_patch(turn.as_ref(), &file_system_sandbox_policy, changes)
                            .await;
                    match invocation {
                        InternalApplyPatchInvocation::Output(item) => {
                            let content = item?;
                            Ok(boxed_tool_output(ApplyPatchToolOutput::from_text(content)))
                        }
                        InternalApplyPatchInvocation::DelegateToRuntime(apply) => {
                            let changes = convert_apply_patch_to_protocol(&apply.action);
                            let emitter = ToolEmitter::apply_patch_for_environment(
                                changes.clone(),
                                apply.auto_approved,
                                turn_environment.environment_id.clone(),
                            );
                            let req = ApplyPatchRequest {
                                cancellation_token,
                                turn_environment: turn_environment.clone(),
                                action: apply.action,
                                file_paths,
                                changes,
                                exec_approval_requirement: apply.exec_approval_requirement,
                                additional_permissions: effective_additional_permissions
                                    .additional_permissions,
                                permissions_preapproved: effective_additional_permissions
                                    .permissions_preapproved,
                            };

                            let tool_ctx = ToolCtx {
                                session: session.clone(),
                                turn: turn.clone(),
                                call_id: call_id.clone(),
                                tool_name: tool_name.clone(),
                            };
                            let output = run_owned_patch(
                                req,
                                tool_ctx,
                                Some(tracker),
                                emitter,
                                workspace_operation_permit,
                            )
                            .await?;
                            Ok(boxed_tool_output(output))
                        }
                    }
                }
                codex_apply_patch::MaybeApplyPatchVerified::CorrectnessError(parse_error) => {
                    session.services.session_telemetry.counter(
                        "codex.apply_patch.verification_failed",
                        1,
                        &[(
                            "failure_kind",
                            crate::tools::runtimes::apply_patch::patch_failure_kind(&parse_error),
                        )],
                    );
                    let observed_source = match &parse_error {
                        codex_apply_patch::ApplyPatchError::PatchContextMismatch(mismatch) => Some(serde_json::json!({
                            "path": mismatch.canonical_path, "sha256": mismatch.current_content_sha256,
                            "hunk": mismatch.hunk_ordinal, "chunk": mismatch.chunk_ordinal,
                        })),
                        _ => None,
                    };
                    let retry = session.services.retained_patches.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .retain(&retained_input, &turn_environment.environment_id,
                            turn_environment.cwd(), &Default::default(), observed_source);
                    Ok(boxed_tool_output(ApplyPatchToolOutput::from_delta(
                        format!("apply_patch verification failed: {parse_error}"), false,
                        &Default::default(), Some(turn_environment.environment_id.clone()),
                    ).with_retry(retry)))
                }
                codex_apply_patch::MaybeApplyPatchVerified::ShellParseError(error) => {
                    tracing::trace!("Failed to parse apply_patch input, {error:?}");
                    Err(FunctionCallError::RespondToModel(format!(
                        "apply_patch received invalid patch input: {error:?}"
                    )))
                }
                codex_apply_patch::MaybeApplyPatchVerified::NotApplyPatch => {
                    Err(FunctionCallError::RespondToModel(
                        "apply_patch expects a patch beginning with *** Begin Patch and ending with *** End Patch".to_string(),
                    ))
                }
            }
        }
        .await;
        let outcome = if cancellation.is_cancelled() {
            "cancelled"
        } else if result
            .as_ref()
            .is_ok_and(|output| output.success_for_logging())
        {
            "success"
        } else {
            "failed"
        };
        for (name, value) in [
            ("codex.apply_patch.files_requested", files_requested),
            ("codex.apply_patch.chunks_requested", chunks_requested),
        ] {
            session
                .services
                .session_telemetry
                .histogram(name, value, &[("outcome", outcome)]);
        }
        result
    }
}

impl CoreToolRuntime for ApplyPatchHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }

    fn cancellation_requires_commit_barrier(&self) -> bool {
        true
    }

    fn tool_execution_timing(&self) -> ToolExecutionTiming {
        ToolExecutionTiming::NestedRuntime
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Custom { .. })
    }

    fn create_diff_consumer(&self) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        Some(Box::<ApplyPatchArgumentDiffConsumer>::default())
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let mut command = apply_patch_payload_command(&invocation.payload)?;
        if apply_patch_retries::is_retry(&command) {
            if let Ok(retry) = invocation
                .session
                .services
                .retained_patches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .prepare(&command)
            {
                command = retry.args.patch;
            }
        }
        Some(PreToolUsePayload {
            tool_name: HookToolName::apply_patch(),
            tool_input: serde_json::json!({ "command": command }),
        })
    }

    fn post_tool_use_hook_name(&self, invocation: &ToolInvocation) -> Option<HookToolName> {
        matches!(&invocation.payload, ToolPayload::Custom { .. }).then(HookToolName::apply_patch)
    }

    fn with_updated_hook_input(
        &self,
        mut invocation: ToolInvocation,
        updated_input: serde_json::Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        let patch = updated_hook_command(&updated_input)?;
        if let ToolPayload::Custom { input } = &invocation.payload
            && apply_patch_retries::is_retry(input)
        {
            codex_apply_patch::parse_patch(patch)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let mut retained = invocation
                .session
                .services
                .retained_patches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let retry = retained
                .prepare(input)
                .map_err(FunctionCallError::RespondToModel)?;
            let environment = resolve_tool_environment(
                &invocation.step_context.environments,
                Some(&retry.environment_id),
            )?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel("retained patch environment is not ready".into())
            })?;
            if environment.cwd() != &retry.cwd {
                return Err(FunctionCallError::RespondToModel(
                    "retained patch working directory changed".into(),
                ));
            }
            retained
                .consume(&retry.id)
                .map_err(FunctionCallError::RespondToModel)?;
        }
        invocation.payload = match invocation.payload {
            ToolPayload::Custom { .. } => ToolPayload::Custom {
                input: patch.to_string(),
            },
            payload => payload,
        };
        Ok(invocation)
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        let tool_response =
            result.post_tool_use_response(&invocation.call_id, &invocation.payload)?;
        Some(PostToolUsePayload {
            tool_name: HookToolName::apply_patch(),
            tool_use_id: invocation.call_id.clone(),
            tool_input: serde_json::json!({
                "command": apply_patch_payload_command(&invocation.payload)?,
            }),
            tool_response,
        })
    }
}

async fn acquire_patch_workspace(
    environment: &TurnEnvironment,
    cwd: &PathUri,
    cancellation_token: &tokio_util::sync::CancellationToken,
) -> Result<Option<tokio::sync::OwnedMutexGuard<()>>, FunctionCallError> {
    tokio::select! {
        biased;
        _ = cancellation_token.cancelled() => Err(FunctionCallError::RespondToModel(
            "apply_patch cancelled while waiting for the workspace".to_string(),
        )),
        permit = crate::workspace_operation_gate::acquire_patch_operation(&environment.environment, cwd) => {
            Ok(Some(permit))
        }
    }
}

// An admitted patch owns its filesystem operations, mutation evidence and diff
// publication until they settle, even if the dispatch waiter is force-aborted.
async fn run_owned_patch(
    req: ApplyPatchRequest,
    tool_ctx: ToolCtx,
    tracker: Option<SharedTurnDiffTracker>,
    emitter: ToolEmitter,
    workspace_operation_permit: Option<tokio::sync::OwnedMutexGuard<()>>,
) -> Result<ApplyPatchToolOutput, FunctionCallError> {
    let terminal_tasks = tool_ctx.session.terminal_tasks.clone();
    let timing = crate::tools::tool_dispatch_trace::active_tool_dispatch_timing();
    let operation = async move {
        let started = std::time::Instant::now();
        let event_ctx = ToolEventCtx::new(
            tool_ctx.session.as_ref(),
            tool_ctx.turn.as_ref(),
            &tool_ctx.call_id,
            tracker.as_ref(),
        );
        emitter
            .begin(event_ctx)
            .await
            .map_err(|error| FunctionCallError::Fatal(error.to_string()))?;
        let mut orchestrator = ToolOrchestrator::new();
        let mut runtime =
            ApplyPatchRuntime::with_workspace_operation_permit(workspace_operation_permit);
        let mutation_in_progress = runtime.mutation_in_progress();
        let out = {
            let execution = orchestrator.run(
                &mut runtime,
                &req,
                &tool_ctx,
                tool_ctx.turn.as_ref(),
                tool_ctx.turn.approval_policy.value(),
            );
            tokio::pin!(execution);
            let cancelled = req.cancellation_token.cancelled();
            tokio::pin!(cancelled);
            // Approval/hook waits remain cancellable. Once run() enters
            // mutation evidence or filesystem work, drive it through finalization.
            // Check again after polling: an attempt can finish and enter a retry
            // approval in the same poll, which must not strand a cancelled task.
            let mut cancellation_seen = false;
            std::future::poll_fn(|cx| {
                cancellation_seen = cancellation_seen
                    || std::future::Future::poll(cancelled.as_mut(), cx).is_ready();
                let cancelled = cancellation_seen;
                if cancelled && !mutation_in_progress.load(std::sync::atomic::Ordering::Acquire) {
                    return std::task::Poll::Ready(Err(
                        crate::tools::sandboxing::ToolError::Denied(
                            "apply_patch cancelled before mutation".to_string(),
                        ),
                    ));
                }
                let result = std::future::Future::poll(execution.as_mut(), cx);
                if result.is_pending()
                    && cancelled
                    && !mutation_in_progress.load(std::sync::atomic::Ordering::Acquire)
                {
                    std::task::Poll::Ready(Err(crate::tools::sandboxing::ToolError::Denied(
                        "apply_patch cancelled before mutation".to_string(),
                    )))
                } else {
                    result
                }
            })
            .await
            .map(|result| result.output)
        };
        // A retry can also end through denial or a hook error. Every terminal
        // path must finalize evidence retained by the preceding attempt.
        runtime.finish_pending_mutation_evidence(&tool_ctx).await;
        let (out, delta) = match out {
            Ok(output) => (Ok(output.exec_output), Some(output.delta)),
            Err(_)
                if req.cancellation_token.is_cancelled()
                    && (!runtime.committed_delta().is_empty()
                        || !runtime.committed_delta().is_exact()) =>
            {
                // Declined means no mutation to the event consumer. A cancelled
                // retry can follow real writes, so publish a failed output and
                // retain the delta for invalidation and the visible turn diff.
                let message = format!(
                    "apply_patch cancelled after mutation\n{}",
                    runtime.committed_delta().failure_summary(),
                );
                let output = codex_protocol::exec_output::ExecToolCallOutput {
                    exit_code: 1,
                    stdout: codex_protocol::exec_output::StreamOutput::new(String::new()),
                    stderr: codex_protocol::exec_output::StreamOutput::new(message.clone()),
                    aggregated_output: codex_protocol::exec_output::StreamOutput::new(message),
                    duration: started.elapsed(),
                    timed_out: false,
                };
                (Ok(output), Some(runtime.committed_delta().clone()))
            }
            Err(error) => (Err(error), Some(runtime.committed_delta().clone())),
        };
        let event_ctx = ToolEventCtx::new(
            tool_ctx.session.as_ref(),
            tool_ctx.turn.as_ref(),
            &tool_ctx.call_id,
            tracker.as_ref(),
        );
        let result = emitter.finish(event_ctx, out, delta.as_ref()).await;
        let result = match (result, runtime.mutation_evidence_warning()) {
            (Ok(output), Some(warning)) => Ok(format!("{output}\n{warning}")),
            (Err(FunctionCallError::RespondToModel(error)), Some(warning)) => Err(
                FunctionCallError::RespondToModel(format!("{error}\n{warning}")),
            ),
            (result, _) => result,
        };
        let result = match result {
            Ok(text) => Ok(ApplyPatchToolOutput::from_delta(
                text,
                true,
                runtime.committed_delta(),
                Some(req.turn_environment.environment_id.clone()),
            )),
            Err(FunctionCallError::RespondToModel(text))
                if !req.cancellation_token.is_cancelled() =>
            {
                // Shell interception can verify relative paths after `cd`.
                // The dedicated tool cannot select that working directory, so
                // never offer a receipt that would replay those paths elsewhere.
                let retry = (req.action.cwd == *req.turn_environment.cwd())
                    .then(|| {
                        tool_ctx
                            .session
                            .services
                            .retained_patches
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .retain(
                                &req.action.patch,
                                &req.turn_environment.environment_id,
                                &req.action.cwd,
                                runtime.committed_delta(),
                                None,
                            )
                    })
                    .flatten();
                Ok(ApplyPatchToolOutput::from_delta(
                    text,
                    false,
                    runtime.committed_delta(),
                    Some(req.turn_environment.environment_id.clone()),
                )
                .with_retry(retry))
            }
            Err(error) => Err(error),
        };
        // Release the workspace gate after the committed delta reaches consumers.
        drop(runtime);
        result
    };
    terminal_tasks
        .spawn(async move {
            match timing {
                Some(timing) => {
                    crate::tools::tool_dispatch_trace::scope_tool_dispatch_timing(timing, operation)
                        .await
                }
                None => operation.await,
            }
        })
        .await
        .map_err(|error| {
            FunctionCallError::Fatal(format!("apply_patch completion task failed: {error}"))
        })?
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn intercept_apply_patch(
    is_validation: bool,
    command: &[String],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    turn_environment: TurnEnvironment,
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    tracker: Option<&SharedTurnDiffTracker>,
    call_id: &str,
    tool_name: &str,
    cancellation_token: tokio_util::sync::CancellationToken,
) -> Result<Option<FunctionToolOutput>, ApplyPatchInterceptionError> {
    if is_validation {
        return Ok(None);
    }
    // Identify patch commands without reading files so ordinary shell calls do not
    // wait for the patch gate. Verification below must run while holding the permit.
    let workspace_operation_permit =
        if let Some(patch_cwd) = codex_apply_patch::apply_patch_command_cwd(command, cwd) {
            acquire_patch_workspace(&turn_environment, &patch_cwd, &cancellation_token).await?
        } else {
            None
        };
    let sandbox = turn.file_system_sandbox_context(/*additional_permissions*/ None, cwd);
    match codex_apply_patch::maybe_parse_apply_patch_verified_for_environment(
        command,
        cwd,
        fs,
        Some(&sandbox),
        &turn_environment.environment_id,
    )
    .await
    {
        codex_apply_patch::MaybeApplyPatchVerified::Body(changes) => {
            let (approval_keys, effective_additional_permissions, file_system_sandbox_policy) =
                effective_patch_permissions(
                    session.as_ref(),
                    turn.as_ref(),
                    turn_environment.environment.approval_scope_id(),
                    &changes,
                    cwd,
                )
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "apply_patch cannot enforce filesystem permissions for the selected environment: {error}"
                    ))
                })?;
            let invocation =
                apply_patch::apply_patch(turn.as_ref(), &file_system_sandbox_policy, changes).await;
            match invocation {
                InternalApplyPatchInvocation::Output(item) => {
                    let content = item?;
                    Ok(Some(FunctionToolOutput::from_text(content, Some(true))))
                }
                InternalApplyPatchInvocation::DelegateToRuntime(apply) => {
                    let changes = convert_apply_patch_to_protocol(&apply.action);
                    let emitter = ToolEmitter::apply_patch_for_environment(
                        changes.clone(),
                        apply.auto_approved,
                        turn_environment.environment_id.clone(),
                    );
                    let req = ApplyPatchRequest {
                        cancellation_token,
                        turn_environment,
                        action: apply.action,
                        file_paths: approval_keys,
                        changes,
                        exec_approval_requirement: apply.exec_approval_requirement,
                        additional_permissions: effective_additional_permissions
                            .additional_permissions,
                        permissions_preapproved: effective_additional_permissions
                            .permissions_preapproved,
                    };

                    let tool_ctx = ToolCtx {
                        session: session.clone(),
                        turn: turn.clone(),
                        call_id: call_id.to_string(),
                        tool_name: ToolName::plain(tool_name),
                    };
                    let output = run_owned_patch(
                        req,
                        tool_ctx,
                        tracker.cloned(),
                        emitter,
                        workspace_operation_permit,
                    )
                    .await?;
                    if !output.success {
                        return Err(FunctionCallError::RespondToModel(output.text).into());
                    }
                    Ok(Some(FunctionToolOutput::from_text(output.text, Some(true))))
                }
            }
        }
        codex_apply_patch::MaybeApplyPatchVerified::CorrectnessError(parse_error) => {
            Err(ApplyPatchInterceptionError::correctness(parse_error))
        }
        codex_apply_patch::MaybeApplyPatchVerified::ShellParseError(error) => {
            tracing::trace!("Failed to parse apply_patch input, {error:?}");
            Ok(None)
        }
        codex_apply_patch::MaybeApplyPatchVerified::NotApplyPatch => Ok(None),
    }
}

fn require_environment_id(
    parsed_environment_id: Option<&str>,
    allow_environment_id: bool,
) -> Result<Option<String>, FunctionCallError> {
    match parsed_environment_id {
        Some(_) if !allow_environment_id => Err(FunctionCallError::RespondToModel(
            "apply_patch environment selection is unavailable for this turn".to_string(),
        )),
        Some(environment_id) => Ok(Some(environment_id.to_string())),
        None => Ok(None),
    }
}

#[cfg(test)]
#[path = "apply_patch_tests.rs"]
mod tests;
