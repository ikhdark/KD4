use codex_features::Feature;
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
use crate::tools::events::ToolEmitter;
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
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::ExecCommandSource;
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
}

pub(super) async fn run_exec_like(
    args: RunExecLikeArgs,
) -> Result<FunctionToolOutput, FunctionCallError> {
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
                effective_additional_permissions.additional_permissions,
                effective_additional_permissions.permissions_preapproved,
                &exec_params.cwd,
            )
        },
        |permissions| Ok(Some(permissions)),
    )
    .map_err(FunctionCallError::RespondToModel)?;

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

    // Intercept apply_patch if present.
    let apply_patch_cwd = PathUri::from_abs_path(&exec_params.cwd);
    if let Some(output) = intercept_apply_patch(
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
    .await?
    {
        return Ok(output);
    }

    let source = ExecCommandSource::Agent;
    let emitter = ToolEmitter::shell(safety_command.clone(), exec_params.cwd.clone(), source);
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
    let raw_output_artifact = if let (Some(attempt_key), Ok(output)) = (&attempt_key, &out) {
        session
            .services
            .command_execution
            .record_exit(attempt_key, output.exit_code)
            .await;
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
    let mut content = emitter
        .finish(event_ctx, out, /*applied_patch_delta*/ None)
        .await?;
    if let Some(advisory) = advisory {
        content.push_str("\n\n");
        content.push_str(advisory);
    }
    if let Some(repair_notice) = repair_notice {
        content.push_str("\n\n");
        content.push_str(&repair_notice);
    }
    if let Some(raw_output_artifact) = raw_output_artifact {
        content.push_str("\n\n");
        content.push_str(&raw_output_artifact.render_for_model());
    }
    Ok(FunctionToolOutput {
        body: vec![
            codex_protocol::models::FunctionCallOutputContentItem::InputText { text: content },
        ],
        success: Some(true),
        post_tool_use_response,
    })
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
