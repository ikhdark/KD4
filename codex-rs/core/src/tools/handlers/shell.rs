use codex_agent_task_store::ValidationCallStatus;
use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::models::ShellCommandToolCallParams;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::exec::ExecParams;
use crate::exec_policy::ExecApprovalRequest;
use crate::function_tool::FunctionCallError;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellType;
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::command_output_artifact::create_raw_output_artifact;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::events::ToolEventCtx;
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

fn shell_command_payload_command(payload: &ToolPayload) -> Option<String> {
    let ToolPayload::Function { arguments } = payload else {
        return None;
    };

    parse_arguments::<ShellCommandToolCallParams>(arguments)
        .ok()
        .and_then(|params| {
            CommandInvocation::from_parts(
                "shell_command",
                "command",
                params.command.as_deref(),
                params.kind.as_deref(),
                params.program.as_deref(),
                params.args.as_deref(),
                params.script_body.as_deref(),
            )
            .ok()
            .map(|command| command.display_command())
        })
}

pub(super) struct RunExecLikeArgs {
    pub(super) tool_name: ToolName,
    pub(super) exec_params: ExecParams,
    pub(super) cancellation_token: CancellationToken,
    pub(super) hook_command: String,
    pub(super) safety_command: Vec<String>,
    pub(super) shell_type: Option<ShellType>,
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
    pub(super) capture_exec_output: bool,
}

pub(super) struct RunExecLikeResult {
    pub(super) output: FunctionToolOutput,
    pub(super) exit_code: Option<i32>,
    pub(super) exec_output: Option<ExecToolCallOutput>,
}

pub(super) async fn run_exec_like(
    args: RunExecLikeArgs,
) -> Result<FunctionToolOutput, FunctionCallError> {
    Ok(run_exec_like_with_exit_code(args).await?.output)
}

pub(super) async fn run_exec_like_with_exit_code(
    args: RunExecLikeArgs,
) -> Result<RunExecLikeResult, FunctionCallError> {
    let coordinator = args
        .session
        .services
        .agent_control
        .task_coordinator()
        .clone();
    let session_source = args.turn.session_source.clone();
    let typed_binding = coordinator.binding_for_source(&session_source);
    if typed_binding.is_some()
        && args.tool_name.name != "verify_local"
        && !is_known_safe_command(&args.safety_command)
    {
        return Err(FunctionCallError::RespondToModel(
            "typed assignments may run only shell commands proven read-only; use apply_patch for scoped source changes or verify_local for repository validation"
                .to_string(),
        ));
    }
    if typed_binding.is_some()
        && (args
            .exec_params
            .sandbox_permissions
            .requests_sandbox_override()
            || args.additional_permissions.is_some())
    {
        return Err(FunctionCallError::RespondToModel(
            "typed assignments cannot request shell sandbox overrides or additional permissions"
                .to_string(),
        ));
    }
    let call_id = args.call_id.clone();
    let command_summary = args.hook_command.clone();
    let result = run_exec_like_with_exit_code_inner(args).await;
    if typed_binding.is_none() {
        return result;
    }
    let status = match &result {
        Ok(result) if result.exit_code == Some(0) => ValidationCallStatus::Succeeded,
        Ok(_) => ValidationCallStatus::Failed,
        Err(FunctionCallError::RespondToModel(message)) if message.contains("rejected by user") => {
            ValidationCallStatus::Cancelled
        }
        Err(_) => ValidationCallStatus::Failed,
    };
    let record_result = coordinator
        .record_validation_call_for_source(&session_source, call_id, command_summary, status)
        .await;
    match (result, record_result) {
        (Ok(result), Ok(_)) => Ok(result),
        (Ok(_), Err(error)) => Err(FunctionCallError::RespondToModel(format!(
            "shell validation result could not be persisted for the typed assignment: {error}"
        ))),
        (Err(error), Ok(_)) => Err(error),
        (Err(error), Err(record_error)) => {
            tracing::warn!(%record_error, "failed to persist typed shell validation result");
            Err(error)
        }
    }
}

async fn run_exec_like_with_exit_code_inner(
    args: RunExecLikeArgs,
) -> Result<RunExecLikeResult, FunctionCallError> {
    let RunExecLikeArgs {
        tool_name,
        exec_params,
        cancellation_token,
        hook_command,
        safety_command,
        shell_type,
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
        capture_exec_output,
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
            exec_output: None,
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
    let exec_output = capture_exec_output
        .then(|| clone_output_bearing_result(&out))
        .flatten();
    let exit_code = out
        .as_ref()
        .ok()
        .map(|output| output.exit_code)
        .or_else(|| exec_output.as_ref().map(|output| output.exit_code));
    let retry_exit_code = retry_exit_code(&out);
    if let (Some(attempt_key), Some(retry_exit_code)) = (attempt_key.as_ref(), retry_exit_code) {
        session
            .services
            .command_execution
            .record_exit(attempt_key, retry_exit_code)
            .await;
    }
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, event_tracker);
    let post_tool_use_response = out
        .as_ref()
        .ok()
        .map(|output| {
            crate::tools::format_exec_output_str(output, turn.model_info.truncation_policy.into())
        })
        .map(JsonValue::String);
    let advisory = out.as_ref().ok().and_then(|output| {
        powershell_script_failure_advisory(
            shell_type,
            Some(output.exit_code),
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
    let mut content = match finish_result {
        Ok(content) => content,
        Err(err) if exec_output.is_some() => err.to_string(),
        Err(err) => return Err(err),
    };
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
        exec_output,
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
        Err(ToolError::Rejected(message)) if message == "rejected by user" => None,
        Err(ToolError::Rejected(_)) => Some(-1),
    }
}

fn clone_output_bearing_result(
    out: &Result<ExecToolCallOutput, ToolError>,
) -> Option<ExecToolCallOutput> {
    match out {
        Ok(output) => Some(output.clone()),
        Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Timeout { output })))
        | Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied { output, .. }))) => {
            Some((**output).clone())
        }
        Err(ToolError::Codex(_)) | Err(ToolError::Rejected(_)) => None,
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
