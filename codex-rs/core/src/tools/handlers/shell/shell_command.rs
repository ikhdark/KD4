use std::sync::Arc;

use codex_protocol::models::ShellCommandToolCallParams;
use codex_tools::ToolName;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathConvention;

use crate::FunctionCallError;
use crate::agent::task_capabilities::is_independent_review_source;
use crate::exec::DEFAULT_EXEC_COMMAND_TIMEOUT_MS;
use crate::exec::ExecCapturePolicy;
use crate::exec::ExecParams;
use crate::exec_env::create_env;
use crate::exec_env::inject_permission_profile_env;
use crate::maybe_emit_implicit_skill_invocation;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::Shell;
use crate::shell::ShellType;
use crate::shell::get_shell;
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::command_preflight::preflight_invocation_with_equivalent_repair_async;
use crate::tools::handlers::command_search::classify_rg_search_narrowing;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::tools::handlers::parse_arguments_with_base_path;
use crate::tools::handlers::resolve_repository_root;
use crate::tools::handlers::resolve_workdir_base_path;
use crate::tools::handlers::rewrite_function_command_invocation;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutionTiming;
use crate::tools::registry::ToolExecutor;
use crate::validation_admission::ValidationAdmission;
use crate::validation_admission::ValidationLaunchPlan;
use crate::validation_admission::admit_validation_invocations;
use codex_tools::ToolSpec;

use super::super::shell_spec::CommandToolOptions;
use super::super::shell_spec::create_shell_command_tool_for_policy;
use super::super::unified_exec::ExecCommandHandler;
use super::super::unified_exec::ExecCommandHandlerOptions;
use super::RunExecLikeArgs;
use super::ValidationLaunchPreparationArgs;
use super::parse_shell_command_hook_invocation;
use super::prepare_validation_launch;
use super::run_exec_like;
use super::validation_environment_hash;
use super::validation_structured_output;

pub(super) fn effective_stall_timeout_ms(
    timeout_ms: Option<u64>,
    requested_stall_timeout_ms: Option<u64>,
) -> Option<u64> {
    let hard_timeout_ms = timeout_ms.unwrap_or(DEFAULT_EXEC_COMMAND_TIMEOUT_MS);
    let stall_timeout_ms = match requested_stall_timeout_ms {
        Some(0) | None => return None,
        Some(stall_timeout_ms) => stall_timeout_ms,
    };

    (stall_timeout_ms < hard_timeout_ms).then_some(stall_timeout_ms)
}

pub struct ShellCommandHandler {
    options: ShellCommandHandlerOptions,
}

#[derive(Clone, Copy)]
pub(crate) struct ShellCommandHandlerOptions {
    pub(crate) allow_login_shell: bool,
    pub(crate) allow_escalated_sandbox_permissions: bool,
    pub(crate) exec_permission_approvals_enabled: bool,
}

impl ShellCommandHandler {
    fn forward_arguments_to_unified_exec(
        arguments: &str,
        environment_id: &str,
        allow_login_shell: bool,
    ) -> Result<String, FunctionCallError> {
        let mut value: serde_json::Value = serde_json::from_str(arguments).map_err(|err| {
            FunctionCallError::RespondToModel(format!("invalid shell_command arguments: {err}"))
        })?;
        let object = value.as_object_mut().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "shell_command arguments must be a JSON object".to_string(),
            )
        })?;
        if let Some(command) = object.remove("command") {
            object.insert("cmd".to_string(), command);
        }
        object.insert(
            "environment_id".to_string(),
            serde_json::Value::String(environment_id.to_string()),
        );
        if !allow_login_shell && !object.contains_key("login") {
            object.insert("login".to_string(), serde_json::Value::Bool(false));
        }
        if !object.contains_key("yield_time_ms")
            && let Some(timeout_ms) = object.get("timeout_ms").cloned()
        {
            object.insert("yield_time_ms".to_string(), timeout_ms);
        }
        serde_json::to_string(&value).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to forward shell_command arguments: {err}"
            ))
        })
    }

    pub(super) fn effective_allow_login_shell(
        session_source: &codex_protocol::protocol::SessionSource,
        allow_login_shell: bool,
    ) -> bool {
        allow_login_shell && !is_independent_review_source(session_source)
    }

    pub(crate) fn new(options: ShellCommandHandlerOptions) -> Self {
        Self { options }
    }

    pub(super) fn resolve_use_login_shell(
        login: Option<bool>,
        allow_login_shell: bool,
    ) -> Result<bool, FunctionCallError> {
        if !allow_login_shell && login == Some(true) {
            return Err(FunctionCallError::RespondToModel(
                "login shell is disabled by config; omit `login` or set it to false.".to_string(),
            ));
        }

        Ok(login.unwrap_or(allow_login_shell))
    }

    #[cfg(test)]
    pub(super) fn base_command(shell: &Shell, command: &str, use_login_shell: bool) -> Vec<String> {
        shell
            .derive_exec_args(command, use_login_shell)
            .expect("test shell must be executable on Windows")
    }

    #[cfg(test)]
    pub(super) fn to_exec_params(
        params: &ShellCommandToolCallParams,
        invocation: &CommandInvocation,
        session: &crate::session::session::Session,
        turn_context: &TurnContext,
        turn_environment: &TurnEnvironment,
        cwd: AbsolutePathBuf,
        allow_login_shell: bool,
    ) -> Result<ExecParams, FunctionCallError> {
        let session_shell = session.user_shell();
        let shell = resolve_command_shell(invocation, turn_environment, session_shell.as_ref())?;
        Self::to_exec_params_with_shell(
            params,
            invocation,
            session,
            turn_context,
            turn_environment,
            cwd,
            allow_login_shell,
            &shell,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn to_exec_params_with_shell(
        params: &ShellCommandToolCallParams,
        invocation: &CommandInvocation,
        session: &crate::session::session::Session,
        turn_context: &TurnContext,
        turn_environment: &TurnEnvironment,
        cwd: AbsolutePathBuf,
        allow_login_shell: bool,
        shell: &Shell,
    ) -> Result<ExecParams, FunctionCallError> {
        let use_login_shell = Self::resolve_use_login_shell(params.login, allow_login_shell)?;
        let command = invocation.to_exec_args(shell, use_login_shell)?;

        let mut env = create_env(
            &turn_context.config.permissions.shell_environment_policy,
            Some(session.thread_id),
        );
        let active_permission_profile = turn_context.config.permissions.active_permission_profile();
        inject_permission_profile_env(&mut env, active_permission_profile.as_ref());

        Ok(ExecParams {
            command,
            codex_home: turn_context.config.codex_home.clone(),
            cwd,
            expiration: params.timeout_ms.into(),
            capture_policy: ExecCapturePolicy::ShellTool,
            env,
            network: turn_context.network.clone(),
            network_environment_id: Some(turn_environment.environment_id.clone()),
            sandbox_permissions: params.sandbox_permissions.unwrap_or_default(),
            windows_sandbox_level: turn_context.windows_sandbox_level,
            windows_sandbox_private_desktop: turn_context
                .config
                .permissions
                .windows_sandbox_private_desktop,
            justification: params.justification.clone(),
            arg0: None,
        })
    }
}

impl Default for ShellCommandHandler {
    fn default() -> Self {
        Self::new(ShellCommandHandlerOptions {
            allow_login_shell: false,
            allow_escalated_sandbox_permissions: false,
            exec_permission_approvals_enabled: false,
        })
    }
}

impl ToolExecutor<ToolInvocation> for ShellCommandHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(crate::tools::SHELL_COMMAND_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_shell_command_tool_for_policy(
            CommandToolOptions {
                allow_login_shell: self.options.allow_login_shell,
                exec_permission_approvals_enabled: self.options.exec_permission_approvals_enabled,
            },
            self.options.allow_escalated_sandbox_permissions,
        )
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl ShellCommandHandler {
    pub(crate) async fn handle_call(
        &self,
        mut invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let Some(turn_environment) = invocation.step_context.environments.primary().cloned() else {
            return Err(FunctionCallError::RespondToModel(
                "shell is unavailable in this session".to_string(),
            ));
        };
        let cwd_uses_native_convention =
            turn_environment.cwd().infer_path_convention() == Some(PathConvention::native());
        if !cwd_uses_native_convention {
            let turn = &invocation.step_context.turn;
            let allow_login_shell = Self::effective_allow_login_shell(
                &turn.session_source,
                turn.config.permissions.allow_login_shell,
            );
            let ToolPayload::Function { arguments } = &mut invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "unsupported payload for shell_command handler".to_string(),
                ));
            };
            *arguments = Self::forward_arguments_to_unified_exec(
                arguments,
                &turn_environment.environment_id,
                allow_login_shell,
            )?;
            return ExecCommandHandler::new(ExecCommandHandlerOptions {
                allow_login_shell: self.options.allow_login_shell,
                allow_escalated_sandbox_permissions: self
                    .options
                    .allow_escalated_sandbox_permissions,
                exec_permission_approvals_enabled: self.options.exec_permission_approvals_enabled,
                include_environment_id: true,
                include_shell_parameter: true,
            })
            .handle_call(invocation)
            .await;
        }

        let ToolInvocation {
            session,
            step_context,
            cancellation_token,
            tracker,
            call_id,
            payload,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);

        let tool_name = self.tool_name();
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(format!(
                "unsupported payload for shell_command handler: {tool_name}"
            )));
        };

        let environment_cwd = turn_environment.cwd().to_abs_path().map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "shell_command cwd `{}` is not native to the Codex host: {err}",
                turn_environment.cwd()
            ))
        })?;
        let cwd = resolve_workdir_base_path(&arguments, &environment_cwd)?;
        let params: ShellCommandToolCallParams = parse_arguments_with_base_path(&arguments, &cwd)?;
        let original_invocation = CommandInvocation::from_parts(
            "shell_command",
            "command",
            params.command.as_deref(),
            params.kind.as_deref(),
            params.program.as_deref(),
            params.args.as_deref(),
            params.script_body.as_deref(),
        )?;
        let prefix_rule = params.prefix_rule.clone();
        let allow_login_shell = Self::effective_allow_login_shell(
            &turn.session_source,
            turn.config.permissions.allow_login_shell,
        );
        let use_login_shell = Self::resolve_use_login_shell(params.login, allow_login_shell)?;
        let session_shell = session.user_shell();
        let original_safety_shell = resolve_command_shell(
            &original_invocation,
            &turn_environment,
            session_shell.as_ref(),
        )?;
        let original_safety_command =
            original_invocation.to_safety_args(&original_safety_shell, use_login_shell)?;
        let original_shell_type = if original_invocation.is_argv() {
            None
        } else {
            Some(original_safety_shell.shell_type)
        };
        let preflight = preflight_invocation_with_equivalent_repair_async(
            &original_invocation,
            &original_safety_command,
            original_shell_type,
        )
        .await
        .map_err(|issue| {
            FunctionCallError::RespondToModel(format!(
                "{issue}\nRegenerate the command and call `shell_command` again."
            ))
        })?;
        let command_repaired = preflight.repaired();
        let validation_invocations = preflight.validation_invocations;
        let command_invocation = preflight.invocation;
        let mut repair_notice = preflight.repair_notice;
        let validation_admission = admit_validation_invocations(
            &turn.validation_authorization,
            &validation_invocations,
            params.validation.is_some(),
        )
        .await;
        let mut validation_launch = match validation_admission {
            ValidationAdmission::Skip(skipped) => {
                if matches!(
                    skipped.skip_disposition,
                    codex_tools::ToolOutputSkipDisposition::Suppressed
                ) {
                    turn.turn_timing_state.record_suppressed_validation_output();
                }
                tracing::info!(reason = ?skipped.reason, "validation command skipped");
                return Ok(boxed_tool_output(validation_structured_output(
                    serde_json::to_value(skipped).unwrap_or_default(),
                )));
            }
            ValidationAdmission::Execute {
                authorization_revision,
                is_validation,
                classification,
            } => is_validation.then(|| ValidationLaunchPlan {
                classification,
                authorization_revision,
                explicitly_tagged: params.validation.is_some(),
                structured_route: None,
                bound_plan_step: None,
                bound_work_unit: None,
                validation_call_id: None,
                turn_timing_state: Some(Arc::clone(&turn.turn_timing_state)),
                focused_validation_token: None,
            }),
        };
        super::downgrade_unattributed_validation(
            &mut validation_launch,
            params.validation.is_some(),
            &mut repair_notice,
        );
        let direct_validation_route = if validation_launch.is_some() {
            let context = params.validation.as_ref().ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "validation commands require direct argv and `validation.covered_paths`"
                        .to_string(),
                )
            })?;
            let validation_repository = super::validation_repository_root_if_needed(
                true,
                cwd.as_path(),
                turn.config.cwd.as_path(),
            );
            let repository = validation_repository.as_ref().ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "validation effective cwd must be inside a repository".to_string(),
                )
            })?;
            Some(
                super::direct_validation_route(
                    context,
                    &command_invocation,
                    repository,
                    params.timeout_ms.unwrap_or(10_000),
                )
                .map_err(FunctionCallError::RespondToModel)?,
            )
        } else {
            None
        };
        let hook_command = command_invocation.display_command();
        maybe_emit_implicit_skill_invocation(session.as_ref(), turn.as_ref(), &hook_command, &cwd)
            .await;
        let safety_shell = if command_repaired {
            resolve_command_shell(
                &command_invocation,
                &turn_environment,
                session_shell.as_ref(),
            )?
        } else {
            original_safety_shell
        };
        let safety_command = if command_repaired {
            command_invocation.to_safety_args(&safety_shell, use_login_shell)?
        } else {
            original_safety_command.clone()
        };
        let shell_wrapper_is_owned = !command_invocation.is_argv();
        let shell_type = if shell_wrapper_is_owned {
            Some(safety_shell.shell_type)
        } else {
            None
        };
        let is_powershell_script = command_invocation.is_powershell_script();
        let exec_params = Self::to_exec_params_with_shell(
            &params,
            &command_invocation,
            session.as_ref(),
            turn.as_ref(),
            &turn_environment,
            cwd,
            allow_login_shell,
            &safety_shell,
        )?;
        let sandbox_context = (
            params.sandbox_permissions.unwrap_or_default(),
            &params.additional_permissions,
            turn.approval_policy.value(),
            turn.permission_profile(),
            turn.windows_sandbox_level,
            exec_params.windows_sandbox_private_desktop,
        );
        let stall_timeout_ms =
            effective_stall_timeout_ms(params.timeout_ms, params.stall_timeout_ms);
        let runtime_context = format!(
            "shell={shell_type:?};login={use_login_shell};capture={:?};network_environment={:?};network={:?};stall_timeout_ms={:?}",
            exec_params.capture_policy,
            exec_params.network_environment_id,
            exec_params.network,
            stall_timeout_ms,
        );
        let environment_hash = validation_environment_hash(&exec_params.env);
        let observed_mutation_revision = tracker.lock().await.current_mutation_revision();
        let repository_epoch = session
            .services
            .command_execution
            .observe_repository_revision(&turn.sub_id, observed_mutation_revision)
            .await;
        let workspace_identity = session
            .services
            .command_execution
            .current_workspace_identity_hash(
                &turn_environment.environment_id,
                exec_params.cwd.as_path(),
            )
            .await;
        let validation_cwd = exec_params.cwd.to_string_lossy().into_owned();
        prepare_validation_launch(ValidationLaunchPreparationArgs {
            session: session.as_ref(),
            validation_launch: &mut validation_launch,
            direct_validation_route: direct_validation_route.as_ref(),
            call_id: &call_id,
        })
        .await?;
        let attempt_key = if validation_launch.is_none() {
            let repository = resolve_repository_root(exec_params.cwd.as_path());
            let attempt_key = CommandAttemptKey::new(
                tool_name.name.as_str(),
                &turn_environment.environment_id,
                validation_cwd,
                &exec_params.command,
            )
            .with_environment_fingerprint(&environment_hash)
            .with_timeout_ms(exec_params.expiration.timeout_ms())
            .with_sandbox_context(&sandbox_context)
            .with_input_context(&prefix_rule)
            .with_runtime_context(&runtime_context)
            .with_repository_epoch(repository_epoch)
            .with_workspace_identity(workspace_identity.as_deref());
            let search = classify_rg_search_narrowing(
                &safety_command,
                shell_type,
                exec_params.cwd.as_path(),
                &repository,
            )
            .map_err(FunctionCallError::RespondToModel)?;
            let attempt_key = attempt_key.with_search_narrowing(
                &turn.sub_id,
                repository.to_string_lossy().as_ref(),
                search,
            );
            session
                .services
                .command_execution
                .admit_search_narrowing(&attempt_key)
                .await
                .map_err(FunctionCallError::RespondToModel)?;
            Some(attempt_key)
        } else {
            None
        };
        let run_args = RunExecLikeArgs {
            tool_name,
            exec_params,
            stall_timeout_ms,
            cancellation_token,
            hook_command,
            safety_command,
            shell_type,
            shell_wrapper_is_owned,
            is_powershell_script,
            additional_permissions: params.additional_permissions.clone(),
            prefix_rule,
            session,
            turn,
            turn_environment,
            tracker,
            call_id,
            track_command_mutations: true,
            attempt_key,
            repair_notice,
            command_repaired,
            force_fresh: params.force_fresh.unwrap_or(false),
            validation_launch,
        };
        run_exec_like(run_args).await.map(boxed_tool_output)
    }
}

pub(super) fn resolve_command_shell(
    invocation: &CommandInvocation,
    turn_environment: &TurnEnvironment,
    session_shell: &Shell,
) -> Result<Shell, FunctionCallError> {
    let environment_shell = turn_environment.shell.as_ref().unwrap_or(session_shell);
    if !invocation.is_powershell_script() {
        return Ok(environment_shell.clone());
    }
    if environment_shell.shell_type == ShellType::PowerShell {
        return Ok(environment_shell.clone());
    }
    if turn_environment.environment.is_remote() {
        return Err(FunctionCallError::RespondToModel(
            "`kind: \"powershell_script\"` requires the selected remote environment to report PowerShell."
                .to_string(),
        ));
    }

    get_shell(ShellType::PowerShell, /*path*/ None).ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "`kind: \"powershell_script\"` requires PowerShell in this environment; use `kind: \"script\"` with an available shell instead."
                .to_string(),
        )
    })
}

impl CoreToolRuntime for ShellCommandHandler {
    fn tool_execution_timing(&self) -> ToolExecutionTiming {
        ToolExecutionTiming::NestedRuntime
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };
        parse_shell_command_hook_invocation(arguments)
            .ok()
            .map(|command| PreToolUsePayload {
                tool_name: HookToolName::shell_command(),
                tool_input: command.hook_input(),
            })
    }

    fn post_tool_use_hook_name(&self, invocation: &ToolInvocation) -> Option<HookToolName> {
        matches!(&invocation.payload, ToolPayload::Function { .. })
            .then(HookToolName::shell_command)
    }

    fn with_updated_hook_input(
        &self,
        mut invocation: ToolInvocation,
        updated_input: serde_json::Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        let ToolPayload::Function { arguments } = invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "hook input rewrite received unsupported shell_command payload".to_string(),
            ));
        };
        let command_invocation = parse_shell_command_hook_invocation(&arguments)?;
        invocation.payload = ToolPayload::Function {
            arguments: rewrite_function_command_invocation(
                &arguments,
                "shell_command",
                "command",
                &command_invocation,
                &updated_input,
            )?,
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
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };
        let command = parse_shell_command_hook_invocation(arguments).ok()?;
        Some(PostToolUsePayload {
            tool_name: HookToolName::shell_command(),
            tool_use_id: invocation.call_id.clone(),
            tool_input: command.hook_input(),
            tool_response,
        })
    }
}
