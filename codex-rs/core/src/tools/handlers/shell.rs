use chrono::Utc;
use codex_agent_task_store::REPOSITORY_WIDE_PATH;
use codex_agent_task_store::ValidationCallStatus;
use codex_agent_task_store::ValidationEvidence;
use codex_agent_task_store::WorkspaceActorKind;
use codex_agent_task_store::WorkspaceMutationRequest;
use codex_features::Feature;
use codex_git_utils::get_git_repo_root;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use crate::agent::task_capabilities::is_independent_review_source;
use crate::agent::task_capabilities::validate_independent_review_shell;
use crate::exec::ExecExpiration;
use crate::exec::ExecParams;
use crate::exec_policy::ExecApprovalRequest;
use crate::function_tool::FunctionCallError;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellType;
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::command_execution::WorkspaceMutationAcquireError;
use crate::tools::command_execution::acquire_workspace_mutation_lease;
use crate::tools::command_output_artifact::create_raw_output_artifact;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::EffectiveAdditionalPermissions;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::handlers::apply_patch::intercept_apply_patch;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::tools::handlers::command_shape::powershell_script_failure_advisory;
use crate::tools::handlers::implicit_granted_permissions;
use crate::tools::handlers::normalize_and_validate_additional_permissions;
use crate::tools::handlers::parse_arguments;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::runtimes::shell::ShellRequest;
use crate::tools::runtimes::shell::ShellRuntime;
use crate::tools::runtimes::shell::ShellRuntimeBackend;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::ExecCommandSource;
use codex_shell_command::is_safe_command::is_known_safe_command;
use codex_tools::ToolName;
use codex_utils_path_uri::PathUri;

mod shell_command;

pub use shell_command::ShellCommandHandler;
pub(crate) use shell_command::ShellCommandHandlerOptions;

const MAX_FOCUSED_VALIDATION_TIMEOUT_MS: u64 = 60 * 60 * 1_000;

#[derive(Debug, Deserialize)]
struct ShellCommandHookArgs {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    program: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    script_body: Option<String>,
}

fn parse_shell_command_hook_invocation(
    arguments: &str,
) -> Result<CommandInvocation, FunctionCallError> {
    let args: ShellCommandHookArgs = parse_arguments(arguments)?;
    CommandInvocation::from_parts(
        "shell_command",
        "command",
        args.command.as_deref(),
        args.kind.as_deref(),
        args.program.as_deref(),
        args.args.as_deref(),
        args.script_body.as_deref(),
    )
}

fn shell_command_payload_command(payload: &ToolPayload) -> Option<String> {
    let ToolPayload::Function { arguments } = payload else {
        return None;
    };

    parse_shell_command_hook_invocation(arguments)
        .ok()
        .map(|command| command.display_command())
}

pub(super) struct RunExecLikeArgs {
    pub(super) tool_name: ToolName,
    pub(super) exec_params: ExecParams,
    pub(super) cancellation_token: CancellationToken,
    pub(super) hook_command: String,
    pub(super) safety_command: Vec<String>,
    pub(super) shell_type: Option<ShellType>,
    pub(super) is_powershell_script: bool,
    pub(super) additional_permissions: Option<AdditionalPermissionProfile>,
    pub(super) prefix_rule: Option<Vec<String>>,
    pub(super) session: Arc<crate::session::session::Session>,
    pub(super) turn: Arc<TurnContext>,
    pub(super) turn_environment: TurnEnvironment,
    pub(super) tracker: crate::tools::context::SharedTurnDiffTracker,
    pub(super) call_id: String,
    pub(super) shell_runtime_backend: ShellRuntimeBackend,
    pub(super) track_validation_freshness: bool,
    pub(super) attempt_key: Option<CommandAttemptKey>,
    pub(super) repair_notice: Option<String>,
}

pub(super) struct RunExecLikeResult {
    pub(super) output: FunctionToolOutput,
    pub(super) exit_code: Option<i32>,
}

pub(super) async fn run_exec_like(
    args: RunExecLikeArgs,
) -> Result<FunctionToolOutput, FunctionCallError> {
    Ok(run_exec_like_with_exit_code(args).await?.output)
}

pub(super) async fn run_exec_like_with_exit_code(
    mut args: RunExecLikeArgs,
) -> Result<RunExecLikeResult, FunctionCallError> {
    let coordinator = args
        .session
        .services
        .agent_control
        .task_coordinator()
        .clone();
    let session_source = args.turn.session_source.clone();
    let typed_binding = coordinator.binding_for_source(&session_source);
    let independent_review = is_independent_review_source(&session_source);
    let inspection_command = is_known_safe_command(&args.safety_command);
    if (typed_binding.is_some() || independent_review)
        && (args
            .exec_params
            .sandbox_permissions
            .requests_sandbox_override()
            || args.additional_permissions.is_some())
    {
        return Err(FunctionCallError::RespondToModel(
            "typed assignments and independent reviewers cannot request shell sandbox overrides or additional permissions"
                .to_string(),
        ));
    }
    validate_independent_review_shell(
        &session_source,
        inspection_command,
        args.exec_params
            .sandbox_permissions
            .requests_sandbox_override(),
        args.additional_permissions.is_some(),
    )
    .map_err(|message| FunctionCallError::RespondToModel(message.to_string()))?;
    if args
        .exec_params
        .sandbox_permissions
        .requests_sandbox_override()
        && !matches!(
            args.turn.approval_policy.value(),
            codex_protocol::protocol::AskForApproval::OnRequest
        )
    {
        let effective_permissions = apply_granted_turn_permissions(
            args.session.as_ref(),
            &args.turn_environment.environment_id,
            args.exec_params.cwd.as_path(),
            args.exec_params.sandbox_permissions,
            args.additional_permissions.clone(),
        )
        .await;
        if !effective_permissions.permissions_preapproved {
            let approval_policy = args.turn.approval_policy.value();
            return Err(FunctionCallError::RespondToModel(format!(
                "approval policy is {approval_policy:?}; reject command — you should not ask for escalated permissions if the approval policy is {approval_policy:?}"
            )));
        }
    }
    let repo_root = get_git_repo_root(args.exec_params.cwd.as_path())
        .unwrap_or_else(|| args.exec_params.cwd.to_path_buf());
    let apply_patch_cwd = PathUri::from_abs_path(&args.exec_params.cwd);
    let intercepted_apply_patch = !matches!(
        codex_apply_patch::maybe_parse_apply_patch(&args.exec_params.command, &apply_patch_cwd),
        codex_apply_patch::MaybeApplyPatch::NotApplyPatch
    );
    let mut workspace_mutation = None;
    if typed_binding.is_none()
        && !independent_review
        && !inspection_command
        && !intercepted_apply_patch
    {
        let reservation = args
            .session
            .services
            .command_execution
            .reserve_workspace_mutation(&repo_root)
            .await;
        if coordinator.store().is_none() {
            coordinator
                .initialize_for_workspace_coordination(
                    args.session.services.state_db.clone(),
                    args.turn.config.sqlite_home.clone(),
                    args.turn.config.model_provider_id.clone(),
                    args.session.services.agent_control.session_id().to_string(),
                )
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "shell workspace coordination could not initialize: {error}"
                    ))
                })?;
        }
        let store = coordinator.store().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "shell workspace coordination store is unavailable".to_string(),
            )
        })?;
        let root_session_id = coordinator.root_session_id().ok_or_else(|| {
            FunctionCallError::RespondToModel("shell root task identity is unavailable".to_string())
        })?;
        let agent_path = session_source
            .get_agent_path()
            .map(|path| path.to_string())
            .unwrap_or_else(|| "/root".to_string());
        let kind = if session_source.is_non_root_agent() {
            WorkspaceActorKind::Legacy
        } else {
            WorkspaceActorKind::Root
        };
        let actor_id = match kind {
            WorkspaceActorKind::Root => format!("root:{root_session_id}"),
            WorkspaceActorKind::Legacy => format!("legacy:{root_session_id}:{agent_path}"),
            WorkspaceActorKind::Typed | WorkspaceActorKind::External => unreachable!(),
        };
        args.session
            .services
            .agent_control
            .reconcile_live_typed_actor_heartbeats()
            .await
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "shell typed-agent liveness could not be reconciled: {error}"
                ))
            })?;
        let request = WorkspaceMutationRequest {
            root_session_id,
            actor_id,
            kind,
            attempt_id: None,
            paths: vec![REPOSITORY_WIDE_PATH.to_string()],
            contracts: Vec::new(),
            expected_manifest: Vec::new(),
        };
        let lease = acquire_workspace_mutation_lease(
            store.as_ref(),
            &repo_root,
            &request,
            &args.cancellation_token,
        )
        .await
        .map_err(|error| match error {
            WorkspaceMutationAcquireError::Cancelled => FunctionCallError::RespondToModel(
                "shell command mutation-lease wait was cancelled".to_string(),
            ),
            WorkspaceMutationAcquireError::Store(error) => {
                FunctionCallError::RespondToModel(format!(
                    "shell command could not acquire the repository-wide mutation lease: {error}"
                ))
            }
        })?;
        workspace_mutation = Some((store, repo_root.clone(), lease, reservation));
    }
    let focused_validation_command = if typed_binding.is_some() && !inspection_command {
        let command_summary = focused_validation_command_summary(
            &args.safety_command,
            &args.hook_command,
            args.shell_type.is_none() && !args.is_powershell_script,
            args.exec_params.cwd.as_path(),
            repo_root.as_path(),
            &args.exec_params.expiration,
            args.exec_params
                .sandbox_permissions
                .requests_sandbox_override(),
            args.additional_permissions.is_some(),
            args.prefix_rule.is_some(),
        )
        .map_err(|reason| {
            FunctionCallError::RespondToModel(format!(
                "typed assignment shell command is not admitted focused validation: {reason}"
            ))
        })?;
        let resolved_executable = pin_focused_validation_executable(
            &mut args.exec_params.command,
            &args.safety_command,
            &args.exec_params.env,
            args.exec_params.cwd.as_path(),
            repo_root.as_path(),
        )
        .map_err(|reason| {
            FunctionCallError::RespondToModel(format!(
                "typed assignment validation executable is not trusted: {reason}"
            ))
        })?;
        Some((command_summary, resolved_executable))
    } else {
        None
    };
    let call_id = args.call_id.clone();
    let retained_output_ref = format!("tool-call:{}:{call_id}", args.session.thread_id);
    let focused_validation = if let Some((command_summary, resolved_executable)) =
        focused_validation_command
    {
        let lease_expires_at = Utc::now()
            + chrono::Duration::seconds(codex_agent_task_store::DEFAULT_WORKSPACE_LEASE_SECONDS);
        let toolchain = child_env_value(&args.exec_params.env, "RUSTUP_TOOLCHAIN")
            .map(|value| value.to_string_lossy().into_owned())
            .or_else(|| args.safety_command.first().cloned());
        let token = coordinator
            .begin_focused_validation_for_source_with_evidence(
                &session_source,
                call_id.clone(),
                command_summary,
                resolved_executable,
                ValidationEvidence {
                    cwd: Some(args.exec_params.cwd.to_string_lossy().into_owned()),
                    environment_hash: Some(validation_environment_hash(&args.exec_params.env)),
                    toolchain,
                    retained_output_ref: Some(retained_output_ref.clone()),
                    lease_expires_at: Some(lease_expires_at),
                    ..ValidationEvidence::default()
                },
            )
            .await
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "focused validation start could not be persisted: {error}"
                ))
            })?;
        let Some(token) = token else {
            return Err(FunctionCallError::RespondToModel(
                "focused validation lost its typed assignment binding before execution".to_string(),
            ));
        };
        if let Some(leader_call_id) = token.shared_from_call_id().map(str::to_string) {
            let leader = loop {
                if args.cancellation_token.is_cancelled() {
                    coordinator
                        .finish_focused_validation_with_output(
                            token,
                            ValidationCallStatus::Cancelled,
                            Some(retained_output_ref),
                            None,
                        )
                        .await
                        .map_err(|error| {
                            FunctionCallError::RespondToModel(format!(
                                "shared validation cancellation could not be persisted: {error}"
                            ))
                        })?;
                    return Err(FunctionCallError::RespondToModel(
                        "shared validation wait was cancelled".to_string(),
                    ));
                }
                let leader = coordinator
                    .get_validation_call(leader_call_id.clone())
                    .await
                    .map_err(|error| {
                        FunctionCallError::RespondToModel(format!(
                            "shared validation leader could not be read: {error}"
                        ))
                    })?
                    .ok_or_else(|| {
                        FunctionCallError::RespondToModel(format!(
                            "shared validation leader {leader_call_id} disappeared"
                        ))
                    })?;
                if leader.status.is_terminal() {
                    break leader;
                }
                if leader
                    .evidence
                    .lease_expires_at
                    .or_else(|| token.lease_expires_at())
                    .is_some_and(|deadline| deadline <= Utc::now())
                {
                    let mut expired = leader;
                    expired.status = ValidationCallStatus::Cancelled;
                    expired.recorded_at = Utc::now();
                    let recovery = coordinator
                        .store()
                        .ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "shared validation store became unavailable".to_string(),
                            )
                        })?
                        .record_validation_call(expired)
                        .await;
                    match recovery {
                        Ok(()) => {}
                        Err(codex_agent_task_store::StoreError::ValidationCallImmutable(_)) => {
                            let refreshed = coordinator
                                .get_validation_call(leader_call_id.clone())
                                .await
                                .map_err(|error| {
                                    FunctionCallError::RespondToModel(format!(
                                        "shared validation leader could not be reread after a recovery race: {error}"
                                    ))
                                })?
                                .ok_or_else(|| {
                                    FunctionCallError::RespondToModel(format!(
                                        "shared validation leader {leader_call_id} disappeared during recovery"
                                    ))
                                })?;
                            if refreshed.status.is_terminal() {
                                break refreshed;
                            }
                        }
                        Err(error) => {
                            return Err(FunctionCallError::RespondToModel(format!(
                                "expired shared validation lease could not be recovered: {error}"
                            )));
                        }
                    }
                    continue;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            };
            let status = leader.status;
            let leader_output_ref = leader.evidence.retained_output_ref.clone();
            let leader_output_summary = leader.evidence.output_summary.clone();
            coordinator
                .finish_focused_validation_with_output(
                    token,
                    status,
                    leader_output_ref.clone(),
                    leader_output_summary.clone(),
                )
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "shared validation result could not be persisted: {error}"
                    ))
                })?;
            let output_reference = leader_output_ref
                .as_deref()
                .unwrap_or("no retained output reference");
            let output_summary = leader_output_summary
                .as_deref()
                .unwrap_or("no retained output summary");
            return Ok(RunExecLikeResult {
                output: FunctionToolOutput {
                    body: vec![
                        codex_protocol::models::FunctionCallOutputContentItem::InputText {
                            text: format!(
                                "Validation singleflight reused leader {leader_call_id}; status: {status:?}; output: {output_reference}\n\n{output_summary}"
                            ),
                        },
                    ],
                    success: Some(status.is_success()),
                    post_tool_use_response: None,
                },
                exit_code: Some(if status.is_success() { 0 } else { 1 }),
            });
        }
        Some(token)
    } else {
        None
    };
    let heartbeat_stop = CancellationToken::new();
    let heartbeat_task = focused_validation.as_ref().map(|token| {
        let coordinator = coordinator.clone();
        let call_id = token.call_id().to_string();
        let heartbeat_stop = heartbeat_stop.clone();
        AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = heartbeat_stop.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                        let lease_expires_at = Utc::now()
                            + chrono::Duration::seconds(
                                codex_agent_task_store::DEFAULT_WORKSPACE_LEASE_SECONDS,
                            );
                        match coordinator
                            .heartbeat_validation_call(call_id.clone(), lease_expires_at)
                            .await
                        {
                            Ok(true) => {}
                            Ok(false) => break,
                            Err(error) => {
                                tracing::warn!(%error, %call_id, "validation heartbeat failed");
                                break;
                            }
                        }
                    }
                }
            }
        }))
    });
    let workspace_heartbeat_stop = CancellationToken::new();
    let workspace_heartbeat_task =
        workspace_mutation
            .as_ref()
            .map(|(store, repo_root, lease, _reservation)| {
                let store = store.clone();
                let repo_root = repo_root.clone();
                let lease_id = lease.lease_id.clone();
                let actor_id = lease.actor_id.clone();
                let workspace_heartbeat_stop = workspace_heartbeat_stop.clone();
                let command_cancellation = args.cancellation_token.clone();
                AbortOnDropHandle::new(tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = workspace_heartbeat_stop.cancelled() => break,
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
                                            "workspace mutation lease expired before heartbeat"
                                        );
                                        command_cancellation.cancel();
                                        break;
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            %error,
                                            %lease_id,
                                            "workspace mutation heartbeat failed"
                                        );
                                        command_cancellation.cancel();
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }))
            });
    let cancellation_token = args.cancellation_token.clone();
    let result = run_exec_like_with_exit_code_inner(args, focused_validation.is_some()).await;
    heartbeat_stop.cancel();
    if let Some(heartbeat_task) = heartbeat_task
        && let Err(error) = heartbeat_task.await
    {
        tracing::warn!(%error, "validation heartbeat task failed");
    }
    workspace_heartbeat_stop.cancel();
    if let Some(workspace_heartbeat_task) = workspace_heartbeat_task
        && let Err(error) = workspace_heartbeat_task.await
    {
        tracing::warn!(%error, "workspace mutation heartbeat task failed");
    }
    let workspace_record_result = match workspace_mutation {
        Some((store, repo_root, lease, _reservation)) => store
            .finish_workspace_mutation(&repo_root, lease)
            .await
            .map(|_| ())
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "shell workspace mutation could not be finalized: {error}"
                ))
            }),
        None => Ok(()),
    };
    let Some(token) = focused_validation else {
        return match (result, workspace_record_result) {
            (Ok(result), Ok(())) => Ok(result),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(record_error)) => {
                tracing::warn!(
                    %record_error,
                    "failed to finalize shell workspace mutation after command failure"
                );
                Err(error)
            }
        };
    };
    workspace_record_result?;
    let status = match (&result, cancellation_token.is_cancelled()) {
        (_, true) => ValidationCallStatus::Cancelled,
        (Ok(result), false) if result.exit_code == Some(0) => ValidationCallStatus::Succeeded,
        (Ok(_), false) => ValidationCallStatus::Failed,
        (Err(FunctionCallError::RespondToModel(message)), false)
            if message.contains("rejected by user") =>
        {
            ValidationCallStatus::Cancelled
        }
        (Err(_), false) => ValidationCallStatus::Failed,
    };
    let output_summary = validation_output_summary(&result);
    let record_result = coordinator
        .finish_focused_validation_with_output(
            token,
            status,
            Some(retained_output_ref),
            output_summary,
        )
        .await;
    match (result, record_result) {
        (Ok(result), Ok(())) => Ok(result),
        (Ok(_), Err(error)) => Err(FunctionCallError::RespondToModel(format!(
            "shell validation result could not be persisted for the typed assignment: {error}"
        ))),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(record_error)) => {
            tracing::warn!(%record_error, "failed to persist typed shell validation result");
            Err(error)
        }
    }
}

fn validation_environment_hash(env: &HashMap<String, String>) -> String {
    let mut entries = env.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(name, _)| *name);
    let mut digest = Sha256::new();
    for (name, value) in entries {
        digest.update(name.as_bytes());
        digest.update([0]);
        digest.update(value.as_bytes());
        digest.update([0xff]);
    }
    format!("{:x}", digest.finalize())
}

fn validation_output_summary(
    result: &Result<RunExecLikeResult, FunctionCallError>,
) -> Option<String> {
    const MAX_SUMMARY_CHARS: usize = 4_096;
    let text = match result {
        Ok(result) => result
            .output
            .body
            .iter()
            .filter_map(|item| match item {
                codex_protocol::models::FunctionCallOutputContentItem::InputText { text } => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Err(FunctionCallError::RespondToModel(message)) => message.clone(),
        Err(_) => return None,
    };
    if text.is_empty() {
        return None;
    }
    let mut chars = text.chars();
    let summary = chars.by_ref().take(MAX_SUMMARY_CHARS).collect::<String>();
    Some(if chars.next().is_some() {
        format!("{summary}\n[validation output truncated]")
    } else {
        summary
    })
}

fn pin_focused_validation_executable(
    execution_command: &mut [String],
    nominal_command: &[String],
    child_env: &HashMap<String, String>,
    cwd: &Path,
    repo_root: &Path,
) -> Result<String, String> {
    if execution_command != nominal_command {
        return Err("execution argv does not match the admitted nominal argv".to_string());
    }
    let program = nominal_command
        .first()
        .ok_or_else(|| "validation command argv cannot be empty".to_string())?;
    let resolved = resolve_focused_validation_executable(program, child_env, cwd, repo_root)?;
    let resolved_text = resolved
        .to_str()
        .ok_or_else(|| "resolved executable path is not valid UTF-8".to_string())?
        .to_string();
    execution_command[0] = resolved_text.clone();
    Ok(resolved_text)
}

fn resolve_focused_validation_executable(
    program: &str,
    child_env: &HashMap<String, String>,
    cwd: &Path,
    repo_root: &Path,
) -> Result<std::path::PathBuf, String> {
    let path_value = child_env_value(child_env, "PATH")
        .ok_or_else(|| "child PATH is unavailable".to_string())?;
    for path_entry in std::env::split_paths(path_value) {
        let Ok(resolved) = which::which_in(program, Some(path_entry.as_os_str()), cwd) else {
            continue;
        };
        if !path_entry.is_absolute() {
            return Err(format!(
                "executable {program} resolves through a relative PATH entry: {}",
                path_entry.display()
            ));
        }
        if !resolved.is_absolute() {
            return Err(format!(
                "executable {program} resolved to a relative path: {}",
                resolved.display()
            ));
        }
        let canonical_executable = std::fs::canonicalize(&resolved).map_err(|error| {
            format!(
                "executable {program} could not be canonicalized ({}): {error}",
                resolved.display()
            )
        })?;
        let canonical_repo_root = std::fs::canonicalize(repo_root)
            .map_err(|error| format!("repository root could not be canonicalized: {error}"))?;
        if canonical_executable.starts_with(&canonical_repo_root) {
            return Err(format!(
                "executable {program} resolves inside the repository: {}",
                canonical_executable.display()
            ));
        }
        return Ok(canonical_executable);
    }
    Err(format!(
        "executable {program} could not be resolved from child PATH"
    ))
}

#[cfg(windows)]
fn child_env_value<'a>(
    env: &'a HashMap<String, String>,
    name: &str,
) -> Option<&'a std::ffi::OsStr> {
    env.iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| std::ffi::OsStr::new(value))
}

#[cfg(not(windows))]
fn child_env_value<'a>(
    env: &'a HashMap<String, String>,
    name: &str,
) -> Option<&'a std::ffi::OsStr> {
    env.get(name).map(|value| std::ffi::OsStr::new(value))
}

#[allow(clippy::too_many_arguments)]
fn focused_validation_command_summary(
    command: &[String],
    command_summary: &str,
    direct_argv: bool,
    cwd: &Path,
    repo_root: &Path,
    expiration: &ExecExpiration,
    sandbox_override: bool,
    additional_permissions: bool,
    prefix_rule: bool,
) -> Result<String, String> {
    if !direct_argv {
        return Err("the command must use direct argv mode".to_string());
    }
    if cwd != repo_root {
        return Err("the command cwd must be the repository root".to_string());
    }
    let timeout_ms = match expiration {
        ExecExpiration::Timeout(timeout) => {
            u64::try_from(timeout.as_millis()).map_err(|_| "timeout is too large".to_string())?
        }
        ExecExpiration::DefaultTimeout => {
            return Err("timeout_ms must be supplied explicitly".to_string());
        }
        ExecExpiration::Cancellation(_) | ExecExpiration::TimeoutOrCancellation { .. } => {
            return Err("focused validation requires an explicit bounded timeout".to_string());
        }
    };
    if timeout_ms == 0 || timeout_ms > MAX_FOCUSED_VALIDATION_TIMEOUT_MS {
        return Err(format!(
            "timeout_ms must be between 1 and {MAX_FOCUSED_VALIDATION_TIMEOUT_MS}"
        ));
    }
    if sandbox_override || additional_permissions || prefix_rule {
        return Err(
            "sandbox overrides, additional permissions, and prefix rules are not allowed"
                .to_string(),
        );
    }
    let Some((program, args)) = command.split_first() else {
        return Err("the command argv cannot be empty".to_string());
    };
    validate_focused_validation_argv(program, args, repo_root)?;
    let canonical = CommandInvocation::Argv {
        program: program.clone(),
        args: args.to_vec(),
    }
    .display_command();
    if canonical != command_summary {
        return Err("command summary is not the canonical direct-argv rendering".to_string());
    }
    Ok(canonical)
}

fn validate_focused_validation_argv(
    program: &str,
    args: &[String],
    repo_root: &Path,
) -> Result<(), String> {
    if args.iter().any(|arg| forbidden_control_argument(arg)) {
        return Err("shell chaining, wrappers, and redirection are not allowed".to_string());
    }
    match program {
        "cargo" => validate_cargo_validation(args),
        "just" => validate_just_validation(args),
        "python" | "python3" => validate_python_validation(args, repo_root),
        _ => Err("only direct cargo, just, or python validation is allowed".to_string()),
    }
}

fn validate_cargo_validation(args: &[String]) -> Result<(), String> {
    if !matches!(args.first().map(String::as_str), Some("check" | "test")) {
        return Err("cargo validation is limited to check or test subcommands".to_string());
    }
    if args
        .iter()
        .skip(1)
        .any(|arg| cargo_path_or_config_override(arg))
    {
        return Err(
            "cargo path, configuration, and unstable overrides are not allowed".to_string(),
        );
    }
    Ok(())
}

fn validate_just_validation(args: &[String]) -> Result<(), String> {
    let Some(recipe) = args.first().map(String::as_str) else {
        return Err("just validation requires an explicit recipe".to_string());
    };
    match recipe {
        "source-map-check" | "fmt-check" if args.len() == 1 => Ok(()),
        "source-map-check" | "fmt-check" => Err(format!("just {recipe} does not accept arguments")),
        "test-fast" | "test-compile" | "test-lane-main" => {
            validate_nextest_forwarded_args(&args[1..])
        }
        "test-lane" | "test-lane-fast" | "test-lane-package" => {
            let Some(identifier) = args.get(1) else {
                return Err(format!(
                    "just {recipe} requires a lane or package identifier"
                ));
            };
            if !safe_just_identifier(identifier) {
                return Err(
                    "just lane and package identifiers must be simple safe names".to_string(),
                );
            }
            validate_nextest_forwarded_args(&args[2..])
        }
        "check-lane" => {
            let Some(identifier) = args.get(1) else {
                return Err("just check-lane requires a package identifier".to_string());
            };
            if !safe_just_identifier(identifier) {
                return Err(
                    "just lane and package identifiers must be simple safe names".to_string(),
                );
            }
            validate_cargo_check_forwarded_args(&args[2..])
        }
        _ => Err("just recipe is not an admitted nonmutating validation recipe".to_string()),
    }
}

fn validate_nextest_forwarded_args(args: &[String]) -> Result<(), String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let (option, inline_value) = split_option(arg);
        match option {
            "-p" | "--package" => {
                let value = take_option_value(args, &mut index, inline_value, option)?;
                if !safe_just_identifier(value) {
                    return Err(format!("nextest {option} requires a simple package name"));
                }
            }
            "-E" | "--filterset" | "--filter-expr" => {
                require_nonempty_option_value(args, &mut index, inline_value, option)?;
            }
            "--features" => {
                let value = take_option_value(args, &mut index, inline_value, option)?;
                if !safe_feature_list(value) {
                    return Err("nextest --features value is not admitted".to_string());
                }
            }
            "--lib"
            | "--bins"
            | "--tests"
            | "--benches"
            | "--all-targets"
            | "--workspace"
            | "--all"
            | "--no-fail-fast"
            | "--fail-fast"
            | "--no-capture"
            | "--nocapture"
            | "--locked"
            | "--offline"
            | "--frozen"
            | "--release"
            | "--all-features"
            | "--no-default-features"
                if inline_value.is_none() => {}
            _ if arg.starts_with('-') => {
                return Err(format!("nextest forwarded option is not admitted: {arg}"));
            }
            _ if !safe_nextest_filter(arg) => {
                return Err(format!("nextest test filter is not admitted: {arg}"));
            }
            _ => {}
        }
        index += 1;
    }
    Ok(())
}

fn validate_cargo_check_forwarded_args(args: &[String]) -> Result<(), String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let (option, inline_value) = split_option(arg);
        match option {
            "--features" => {
                let value = take_option_value(args, &mut index, inline_value, option)?;
                if !safe_feature_list(value) {
                    return Err("cargo check --features value is not admitted".to_string());
                }
            }
            "--lib"
            | "--bins"
            | "--tests"
            | "--benches"
            | "--examples"
            | "--all-targets"
            | "--workspace"
            | "--locked"
            | "--offline"
            | "--frozen"
            | "--release"
            | "--all-features"
            | "--no-default-features"
                if inline_value.is_none() => {}
            _ => {
                return Err(format!(
                    "cargo check forwarded option is not admitted: {arg}"
                ));
            }
        }
        index += 1;
    }
    Ok(())
}

fn safe_feature_list(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+' | b',' | b'/')
        })
}

fn safe_nextest_filter(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('@')
        && !matches!(
            value,
            "archive" | "extract" | "remap" | "metadata" | "rerun" | "output"
        )
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn validate_python_validation(args: &[String], repo_root: &Path) -> Result<(), String> {
    let module = match args {
        [module_flag, module, ..] if module_flag == "-m" => module.as_str(),
        _ => {
            return Err(
                "python validation must be a direct `python -m unittest` or `python -m pytest` invocation"
                    .to_string(),
            );
        }
    };
    if !matches!(module, "unittest" | "pytest") {
        return Err("python validation module must be unittest or pytest".to_string());
    }
    match module {
        "unittest" => validate_unittest_args(&args[2..], repo_root),
        "pytest" => validate_pytest_args(&args[2..], repo_root),
        _ => unreachable!("module allowlist checked above"),
    }
}

fn validate_unittest_args(args: &[String], repo_root: &Path) -> Result<(), String> {
    let discovery = args.first().is_some_and(|arg| arg == "discover");
    let mut index = if discovery { 1 } else { 0 };
    while index < args.len() {
        let arg = &args[index];
        let (option, inline_value) = split_option(arg);
        match option {
            "-v" | "--verbose" | "-q" | "--quiet" | "--locals" | "-f" | "--failfast" | "-b"
            | "--buffer"
                if inline_value.is_none() => {}
            "--durations" => {
                let value = take_option_value(args, &mut index, inline_value, option)?;
                value
                    .parse::<u64>()
                    .map_err(|_| "unittest --durations requires an integer".to_string())?;
            }
            "-k" => {
                require_nonempty_option_value(args, &mut index, inline_value, option)?;
            }
            "-s" | "--start-directory" | "-t" | "--top-level-directory" if discovery => {
                let value = take_option_value(args, &mut index, inline_value, option)?;
                require_safe_repo_relative_path(value, option, repo_root)?;
            }
            "-p" | "--pattern" if discovery => {
                require_nonempty_option_value(args, &mut index, inline_value, option)?;
            }
            _ if arg.starts_with('-') => {
                return Err(format!("unittest option is not admitted: {arg}"));
            }
            _ if discovery => {
                return Err(format!(
                    "unittest discover positional is not admitted: {arg}"
                ));
            }
            _ => require_safe_unittest_selector(selector_path(arg), repo_root)?,
        }
        index += 1;
    }
    Ok(())
}

fn validate_pytest_args(args: &[String], repo_root: &Path) -> Result<(), String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let (option, inline_value) = split_option(arg);
        match option {
            "-q"
            | "--quiet"
            | "-v"
            | "--verbose"
            | "-s"
            | "-x"
            | "--exitfirst"
            | "--collect-only"
            | "--co"
            | "--fixtures"
            | "--fixtures-per-test"
            | "--lf"
            | "--last-failed"
            | "--ff"
            | "--failed-first"
            | "--nf"
            | "--new-first"
            | "--sw"
            | "--stepwise"
            | "--stepwise-skip"
            | "--strict-config"
            | "--strict-markers"
            | "--strict"
            | "--disable-warnings"
            | "--showlocals"
            | "--no-showlocals"
            | "--full-trace"
            | "--no-header"
            | "--no-summary"
            | "--setup-only"
            | "--setup-show"
            | "--setup-plan"
                if inline_value.is_none() => {}
            _ if admitted_pytest_short_cluster(arg) => {}
            "-k" | "-m" => {
                require_nonempty_option_value(args, &mut index, inline_value, option)?;
            }
            "--maxfail" | "--durations" => {
                let value = take_option_value(args, &mut index, inline_value, option)?;
                value
                    .parse::<u64>()
                    .map_err(|_| format!("pytest {option} requires an integer"))?;
            }
            "--durations-min" => {
                let value = take_option_value(args, &mut index, inline_value, option)?;
                value
                    .parse::<f64>()
                    .map_err(|_| "pytest --durations-min requires a number".to_string())?;
            }
            "--verbosity" => {
                let value = take_option_value(args, &mut index, inline_value, option)?;
                value
                    .parse::<i64>()
                    .map_err(|_| "pytest --verbosity requires an integer".to_string())?;
            }
            "--tb" => {
                require_option_choice(
                    args,
                    &mut index,
                    inline_value,
                    option,
                    &["auto", "long", "short", "line", "native", "no"],
                )?;
            }
            "--capture" => {
                require_option_choice(
                    args,
                    &mut index,
                    inline_value,
                    option,
                    &["fd", "sys", "no", "tee-sys"],
                )?;
            }
            "--color" => {
                require_option_choice(
                    args,
                    &mut index,
                    inline_value,
                    option,
                    &["yes", "no", "auto"],
                )?;
            }
            "--code-highlight" => {
                require_option_choice(args, &mut index, inline_value, option, &["yes", "no"])?;
            }
            "--show-capture" => {
                require_option_choice(
                    args,
                    &mut index,
                    inline_value,
                    option,
                    &["no", "stdout", "stderr", "log", "all"],
                )?;
            }
            "--ignore" | "--ignore-glob" => {
                let value = take_option_value(args, &mut index, inline_value, option)?;
                require_safe_repo_relative_path(value, option, repo_root)?;
            }
            "--deselect" => {
                let value = take_option_value(args, &mut index, inline_value, option)?;
                require_safe_repo_relative_path(selector_path(value), option, repo_root)?;
            }
            _ if arg.starts_with('-') => {
                return Err(format!("pytest option is not admitted: {arg}"));
            }
            _ if arg.starts_with('@') => {
                return Err("pytest argument files are not admitted".to_string());
            }
            _ => require_safe_repo_relative_path(selector_path(arg), "pytest selector", repo_root)?,
        }
        index += 1;
    }
    Ok(())
}

fn split_option(arg: &str) -> (&str, Option<&str>) {
    arg.split_once('=')
        .map_or((arg, None), |(option, value)| (option, Some(value)))
}

fn take_option_value<'a>(
    args: &'a [String],
    index: &mut usize,
    inline_value: Option<&'a str>,
    option: &str,
) -> Result<&'a str, String> {
    if let Some(value) = inline_value {
        return (!value.is_empty())
            .then_some(value)
            .ok_or_else(|| format!("{option} requires a value"));
    }
    *index += 1;
    args.get(*index)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{option} requires a value"))
}

fn require_nonempty_option_value<'a>(
    args: &'a [String],
    index: &mut usize,
    inline_value: Option<&'a str>,
    option: &str,
) -> Result<(), String> {
    take_option_value(args, index, inline_value, option).map(|_| ())
}

fn require_option_choice<'a>(
    args: &'a [String],
    index: &mut usize,
    inline_value: Option<&'a str>,
    option: &str,
    choices: &[&str],
) -> Result<(), String> {
    let value = take_option_value(args, index, inline_value, option)?;
    choices
        .contains(&value)
        .then_some(())
        .ok_or_else(|| format!("pytest {option} value is not admitted: {value}"))
}

fn admitted_pytest_short_cluster(arg: &str) -> bool {
    arg.len() > 2
        && arg.starts_with('-')
        && !arg.starts_with("--")
        && (arg[1..]
            .bytes()
            .all(|byte| matches!(byte, b'q' | b'v' | b's' | b'x'))
            || arg
                .strip_prefix("-r")
                .is_some_and(|flags| flags.bytes().all(|byte| b"fEsxXwPpA".contains(&byte))))
}

fn safe_just_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains("..")
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .copied()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn selector_path(value: &str) -> &str {
    value.split("::").next().unwrap_or(value)
}

fn require_safe_unittest_selector(value: &str, repo_root: &Path) -> Result<(), String> {
    require_safe_repo_relative_path(value, "unittest selector", repo_root)?;
    if value.contains(['/', '\\']) || value.ends_with(".py") {
        return Ok(());
    }

    let components = value.split('.').collect::<Vec<_>>();
    for end in (1..=components.len()).rev() {
        let module_path = components[..end]
            .iter()
            .fold(repo_root.to_path_buf(), |path, component| {
                path.join(component)
            });
        for candidate in [module_path.clone(), module_path.with_extension("py")] {
            match std::fs::symlink_metadata(&candidate) {
                Ok(_) => {
                    return require_canonical_repo_containment(
                        repo_root,
                        &candidate,
                        "unittest selector",
                        value,
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "unittest selector could not be inspected safely ({value}): {error}"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn require_safe_repo_relative_path(
    value: &str,
    label: &str,
    repo_root: &Path,
) -> Result<(), String> {
    if !safe_repo_relative_path(value) {
        return Err(format!("{label} must stay within the repository: {value}"));
    }

    let candidate = repo_root.join(value);
    let mut existing_ancestor = candidate.as_path();
    loop {
        match std::fs::symlink_metadata(existing_ancestor) {
            Ok(_) => {
                return require_canonical_repo_containment(
                    repo_root,
                    existing_ancestor,
                    label,
                    value,
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing_ancestor = existing_ancestor.parent().ok_or_else(|| {
                    format!("{label} has no inspectable repository ancestor: {value}")
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "{label} could not be inspected safely ({value}): {error}"
                ));
            }
        }
    }
}

fn require_canonical_repo_containment(
    repo_root: &Path,
    existing_candidate: &Path,
    label: &str,
    display_value: &str,
) -> Result<(), String> {
    let canonical_root = std::fs::canonicalize(repo_root)
        .map_err(|error| format!("repository root could not be canonicalized: {error}"))?;
    let canonical_candidate = std::fs::canonicalize(existing_candidate).map_err(|error| {
        format!("{label} could not be canonicalized safely ({display_value}): {error}")
    })?;
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(format!(
            "{label} resolves outside the repository: {display_value}"
        ));
    }
    Ok(())
}

fn safe_repo_relative_path(value: &str) -> bool {
    if value.is_empty()
        || value
            .as_bytes()
            .first()
            .is_some_and(|byte| matches!(byte, b'/' | b'\\' | b'~'))
        || value.as_bytes().get(1) == Some(&b':')
        || Path::new(value).is_absolute()
    {
        return false;
    }
    !value.split(['/', '\\']).any(|component| component == "..")
}

fn cargo_path_or_config_override(arg: &str) -> bool {
    matches!(
        arg,
        "--manifest-path"
            | "--config"
            | "--target-dir"
            | "--artifact-dir"
            | "--lockfile-path"
            | "-Z"
    ) || arg.starts_with("--manifest-path=")
        || arg.starts_with("--config=")
        || arg.starts_with("--target-dir=")
        || arg.starts_with("--artifact-dir=")
        || arg.starts_with("--lockfile-path=")
        || arg.starts_with("-Z")
}

fn forbidden_control_argument(arg: &str) -> bool {
    arg.contains(['\r', '\n', '\0'])
        || matches!(
            arg,
            "|" | "||" | "&&" | "&" | ";" | ">" | ">>" | "<" | "2>" | "2>>"
        )
}

fn reject_focused_effective_permissions(
    focused_validation: bool,
    effective_permissions: &EffectiveAdditionalPermissions,
) -> Result<(), String> {
    if focused_validation
        && (effective_permissions
            .sandbox_permissions
            .requests_sandbox_override()
            || effective_permissions.additional_permissions.is_some())
    {
        return Err(
            "focused validation cannot use inherited session or turn permission grants".to_string(),
        );
    }
    Ok(())
}

async fn run_exec_like_with_exit_code_inner(
    args: RunExecLikeArgs,
    focused_validation: bool,
) -> Result<RunExecLikeResult, FunctionCallError> {
    let RunExecLikeArgs {
        tool_name,
        exec_params,
        cancellation_token,
        hook_command,
        safety_command,
        shell_type,
        is_powershell_script,
        additional_permissions,
        prefix_rule,
        session,
        turn,
        turn_environment,
        tracker,
        call_id,
        shell_runtime_backend,
        track_validation_freshness,
        attempt_key,
        repair_notice,
    } = args;

    let fs = turn_environment.environment.get_filesystem();

    let explicit_env_overrides = turn
        .config
        .permissions
        .shell_environment_policy
        .r#set
        .clone();
    let exec_permission_approvals_enabled =
        session.features().enabled(Feature::ExecPermissionApprovals);
    let requested_additional_permissions = additional_permissions.clone();
    let effective_additional_permissions = apply_granted_turn_permissions(
        session.as_ref(),
        &turn_environment.environment_id,
        exec_params.cwd.as_path(),
        exec_params.sandbox_permissions,
        additional_permissions,
    )
    .await;
    reject_focused_effective_permissions(focused_validation, &effective_additional_permissions)
        .map_err(FunctionCallError::RespondToModel)?;
    let additional_permissions_allowed = exec_permission_approvals_enabled
        || (session.features().enabled(Feature::RequestPermissionsTool)
            && effective_additional_permissions.permissions_preapproved);
    let normalized_additional_permissions = implicit_granted_permissions(
        exec_params.sandbox_permissions,
        requested_additional_permissions.as_ref(),
        &effective_additional_permissions,
    )
    .map_or_else(
        || {
            normalize_and_validate_additional_permissions(
                additional_permissions_allowed,
                turn.approval_policy.value(),
                effective_additional_permissions.sandbox_permissions,
                effective_additional_permissions
                    .additional_permissions
                    .clone(),
                effective_additional_permissions.permissions_preapproved,
                &exec_params.cwd,
            )
        },
        |permissions| Ok(Some(permissions)),
    )
    .map_err(FunctionCallError::RespondToModel)?;

    let effective_permission_context = format!(
        "sandbox={:?};additional={:?};preapproved={};normalized={:?}",
        effective_additional_permissions.sandbox_permissions,
        effective_additional_permissions.additional_permissions,
        effective_additional_permissions.permissions_preapproved,
        normalized_additional_permissions,
    );
    let attempt_key =
        attempt_key.map(|key| key.with_permission_context(&effective_permission_context));

    // Approval policy guard for explicit escalation in non-OnRequest modes.
    // Sticky turn permissions have already been approved, so they should
    // continue through the normal exec approval flow for the command.
    if effective_additional_permissions
        .sandbox_permissions
        .requests_sandbox_override()
        && !effective_additional_permissions.permissions_preapproved
        && !matches!(
            turn.approval_policy.value(),
            codex_protocol::protocol::AskForApproval::OnRequest
        )
    {
        let approval_policy = turn.approval_policy.value();
        return Err(FunctionCallError::RespondToModel(format!(
            "approval policy is {approval_policy:?}; reject command — you should not ask for escalated permissions if the approval policy is {approval_policy:?}"
        )));
    }

    if let Some(attempt_key) = attempt_key.as_ref() {
        session
            .services
            .command_execution
            .begin_attempt(attempt_key, repair_notice.is_some())
            .await
            .map_err(|blocked| FunctionCallError::RespondToModel(blocked.render_for_model()))?;
    }

    // Intercept apply_patch if present.
    let apply_patch_cwd = PathUri::from_abs_path(&exec_params.cwd);
    let intercepted = intercept_apply_patch(
        &exec_params.command,
        &apply_patch_cwd,
        fs.as_ref(),
        turn_environment.clone(),
        session.clone(),
        turn.clone(),
        Some(&tracker),
        &call_id,
        tool_name.name.as_str(),
    )
    .await;
    let observed_mutation_revision = tracker.lock().await.current_mutation_revision();
    session
        .services
        .command_execution
        .observe_repository_revision(&turn.sub_id, observed_mutation_revision)
        .await;
    let intercepted = match intercepted {
        Ok(intercepted) => intercepted,
        Err(err) => {
            if let Some(attempt_key) = attempt_key.as_ref() {
                session
                    .services
                    .command_execution
                    .record_exit(attempt_key, -1)
                    .await;
            }
            return Err(err);
        }
    };
    if let Some(output) = intercepted {
        if let Some(attempt_key) = attempt_key.as_ref() {
            session
                .services
                .command_execution
                .record_exit(attempt_key, 0)
                .await;
        }
        return Ok(RunExecLikeResult {
            output,
            exit_code: Some(0),
        });
    }

    let source = ExecCommandSource::Agent;
    let emitter = crate::tools::events::ToolEmitter::shell(
        safety_command.clone(),
        exec_params.cwd.clone(),
        source,
        turn_environment.environment_id.clone(),
    );
    let event_tracker = track_validation_freshness.then_some(&tracker);
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, event_tracker);
    emitter.begin(event_ctx).await;

    let exec_approval_requirement = session
        .services
        .exec_policy
        .create_exec_approval_requirement_for_command(ExecApprovalRequest {
            command: &exec_params.command,
            command_for_safety: Some(&safety_command),
            approval_policy: turn.approval_policy.value(),
            permission_profile: turn.permission_profile(),
            windows_sandbox_level: turn.windows_sandbox_level,
            sandbox_permissions: if effective_additional_permissions.permissions_preapproved {
                codex_protocol::models::SandboxPermissions::UseDefault
            } else {
                effective_additional_permissions.sandbox_permissions
            },
            prefix_rule,
        })
        .await;

    let req = ShellRequest {
        command: exec_params.command.clone(),
        command_for_approval: safety_command,
        turn_environment: turn_environment.clone(),
        shell_type,
        hook_command,
        cwd: exec_params.cwd.clone(),
        timeout_ms: exec_params.expiration.timeout_ms(),
        cancellation_token,
        env: exec_params.env.clone(),
        explicit_env_overrides,
        network: exec_params.network.clone(),
        sandbox_permissions: effective_additional_permissions.sandbox_permissions,
        additional_permissions: normalized_additional_permissions,
        #[cfg(unix)]
        additional_permissions_preapproved: effective_additional_permissions
            .permissions_preapproved,
        justification: exec_params.justification.clone(),
        exec_approval_requirement,
    };
    let mut orchestrator = ToolOrchestrator::new();
    let mut runtime = ShellRuntime::for_shell_command(shell_runtime_backend);
    let tool_ctx = ToolCtx {
        session: session.clone(),
        turn: turn.clone(),
        call_id: call_id.clone(),
        tool_name,
    };
    let out = orchestrator
        .run(
            &mut runtime,
            &req,
            &tool_ctx,
            &turn,
            turn.approval_policy.value(),
        )
        .await
        .map(|result| result.output);
    let exit_code = out.as_ref().ok().map(|output| output.exit_code);
    let retry_exit_code = retry_exit_code(&out);
    if let (Some(attempt_key), Some(retry_exit_code)) = (attempt_key.as_ref(), retry_exit_code) {
        session
            .services
            .command_execution
            .record_exit(attempt_key, retry_exit_code)
            .await;
    }
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, event_tracker);
    let model_projection = out.as_ref().ok().map(|output| {
        crate::tools::project_exec_output_text_with_budget(
            output,
            turn.model_info.truncation_policy.into(),
            /*requested_limit*/ None,
            Some(req.hook_command.as_str()),
        )
    });
    let post_tool_use_response = model_projection
        .as_ref()
        .map(|projection| JsonValue::String(projection.text.clone()));
    let advisory = out.as_ref().ok().and_then(|output| {
        powershell_script_failure_advisory(
            shell_type,
            Some(output.exit_code),
            is_powershell_script,
            &output.aggregated_output.text,
        )
    });
    let raw_output_artifact = if let (Some(_attempt_key), Ok(output)) = (&attempt_key, &out) {
        Some(
            create_raw_output_artifact(
                turn.config.codex_home.as_path(),
                &session.thread_id.to_string(),
                output.aggregated_output.text.as_bytes(),
            )
            .await,
        )
    } else {
        None
    };
    let finish_result = emitter
        .finish(event_ctx, out, /*applied_patch_delta*/ None)
        .await;
    let observed_mutation_revision = tracker.lock().await.current_mutation_revision();
    session
        .services
        .command_execution
        .observe_repository_revision(&turn.sub_id, observed_mutation_revision)
        .await;
    let mut content = finish_result?;
    if let Some(advisory) = advisory {
        content.push_str("\n\n");
        content.push_str(advisory);
    }
    if let Some(repair_notice) = repair_notice {
        content.push_str("\n\n");
        content.push_str(&repair_notice);
    }
    if let Some(raw_output_artifact) = raw_output_artifact {
        insert_metadata_before_output(&mut content, &raw_output_artifact.render_for_model());
        if model_projection.is_some_and(|projection| projection.reduced)
            && let Some(notice) = raw_output_artifact.reduction_notice()
        {
            content.push('\n');
            content.push_str(&notice);
        }
    }
    Ok(RunExecLikeResult {
        output: FunctionToolOutput {
            body: vec![
                codex_protocol::models::FunctionCallOutputContentItem::InputText { text: content },
            ],
            success: Some(true),
            post_tool_use_response,
        },
        exit_code,
    })
}

fn retry_exit_code(out: &Result<ExecToolCallOutput, ToolError>) -> Option<i32> {
    match out {
        Ok(output) => Some(output.exit_code),
        Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Timeout { output })))
        | Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied { output, .. }))) => {
            Some(output.exit_code)
        }
        Err(ToolError::Codex(_)) => Some(-1),
        Err(ToolError::Denied(_)) => None,
        Err(ToolError::Rejected(_)) => Some(-1),
    }
}

fn insert_metadata_before_output(content: &mut String, metadata: &str) {
    const OUTPUT_SECTION: &str = "\nOutput:\n";

    if let Some(output_index) = content.find(OUTPUT_SECTION) {
        content.insert_str(output_index, &format!("\n{metadata}"));
    } else {
        content.push_str("\n\n");
        content.push_str(metadata);
    }
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
