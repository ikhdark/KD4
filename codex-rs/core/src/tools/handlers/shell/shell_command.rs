use codex_git_utils::get_git_repo_root;
use codex_protocol::models::ShellCommandToolCallParams;
use codex_tools::ShellCommandBackendConfig;
use codex_tools::ToolName;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::agent::task_capabilities::is_independent_review_source;
use crate::exec::ExecCapturePolicy;
use crate::exec::ExecParams;
use crate::exec_env::create_env;
use crate::exec_env::inject_permission_profile_env;
use crate::function_tool::FunctionCallError;
use crate::maybe_emit_implicit_skill_invocation;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::Shell;
use crate::shell::ShellType;
use crate::shell::get_shell;
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::command_preflight::preflight_invocation_with_equivalent_repair;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::tools::handlers::parse_arguments_with_base_path;
use crate::tools::handlers::resolve_workdir_base_path;
use crate::tools::handlers::rewrite_function_command_invocation;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutionTiming;
use crate::tools::registry::ToolExecutor;
use crate::tools::runtimes::shell::ShellRuntimeBackend;
use crate::validation_admission::ValidationAdmission;
use crate::validation_admission::ValidationLaunchPlan;
use crate::validation_admission::ValidationLeader;
use crate::validation_admission::ValidationLeaderOwnership;
use crate::validation_admission::ValidationRegistration;
use crate::validation_admission::admit_validation;
use crate::validation_admission::register_if_absent;
use crate::validation_admission::validation_identity;
use codex_tools::ToolSpec;

use super::super::shell_spec::CommandToolOptions;
use super::super::shell_spec::create_shell_command_tool;
use super::RunExecLikeArgs;
use super::parse_shell_command_hook_invocation;
use super::run_exec_like;
use super::shell_command_payload_command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellCommandBackend {
    Classic,
    ZshFork,
}

#[derive(Default)]
pub(super) struct ValidationRegistrationRoles {
    pub(super) execution: Option<ValidationLeaderOwnership>,
    pub(super) worker_waiter: Option<ValidationLeader>,
    pub(super) owner_waiter: Option<ValidationLeader>,
}

pub(super) fn validation_registration_roles(
    registration: ValidationRegistration,
) -> ValidationRegistrationRoles {
    match registration {
        ValidationRegistration::Leader { execution, waiter } => ValidationRegistrationRoles {
            execution: Some(execution),
            worker_waiter: None,
            owner_waiter: Some(waiter),
        },
        ValidationRegistration::Follower(waiter) => ValidationRegistrationRoles {
            execution: None,
            worker_waiter: Some(waiter),
            owner_waiter: None,
        },
    }
}

pub(super) async fn await_validation_execution<T>(
    task: tokio::task::JoinHandle<T>,
    owner_waiter: Option<ValidationLeader>,
) -> Result<T, tokio::task::JoinError> {
    let result = task.await;
    drop(owner_waiter);
    result
}

pub struct ShellCommandHandler {
    backend: ShellCommandBackend,
    options: ShellCommandHandlerOptions,
}

#[derive(Clone, Copy)]
pub(crate) struct ShellCommandHandlerOptions {
    pub(crate) backend_config: ShellCommandBackendConfig,
    pub(crate) allow_login_shell: bool,
    pub(crate) exec_permission_approvals_enabled: bool,
}

impl ShellCommandHandler {
    pub(super) fn effective_allow_login_shell(
        session_source: &codex_protocol::protocol::SessionSource,
        allow_login_shell: bool,
    ) -> bool {
        allow_login_shell && !is_independent_review_source(session_source)
    }

    pub(crate) fn new(options: ShellCommandHandlerOptions) -> Self {
        let backend = match options.backend_config {
            ShellCommandBackendConfig::Classic => ShellCommandBackend::Classic,
            ShellCommandBackendConfig::ZshFork => ShellCommandBackend::ZshFork,
        };
        Self { backend, options }
    }

    fn shell_runtime_backend(&self) -> ShellRuntimeBackend {
        match self.backend {
            ShellCommandBackend::Classic => ShellRuntimeBackend::ShellCommandClassic,
            ShellCommandBackend::ZshFork => ShellRuntimeBackend::ShellCommandZshFork,
        }
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
        shell.derive_exec_args(command, use_login_shell)
    }

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
        let use_login_shell = Self::resolve_use_login_shell(params.login, allow_login_shell)?;
        let command = invocation.to_exec_args(&shell, use_login_shell);

        let mut env = create_env(
            &turn_context.config.permissions.shell_environment_policy,
            Some(session.thread_id),
        );
        let active_permission_profile = turn_context.config.permissions.active_permission_profile();
        inject_permission_profile_env(&mut env, active_permission_profile.as_ref());

        Ok(ExecParams {
            command,
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

impl From<ShellCommandBackendConfig> for ShellCommandHandler {
    fn from(backend_config: ShellCommandBackendConfig) -> Self {
        Self::new(ShellCommandHandlerOptions {
            backend_config,
            allow_login_shell: false,
            exec_permission_approvals_enabled: false,
        })
    }
}

impl ToolExecutor<ToolInvocation> for ShellCommandHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("shell_command")
    }

    fn spec(&self) -> ToolSpec {
        create_shell_command_tool(CommandToolOptions {
            allow_login_shell: self.options.allow_login_shell,
            exec_permission_approvals_enabled: self.options.exec_permission_approvals_enabled,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl ShellCommandHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            cancellation_token,
            tracker,
            call_id,
            payload,
            ..
        } = invocation;

        let tool_name = self.tool_name();
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(format!(
                "unsupported payload for shell_command handler: {tool_name}"
            )));
        };

        let Some(turn_environment) = step_context.environments.primary().cloned() else {
            return Err(FunctionCallError::RespondToModel(
                "shell is unavailable in this session".to_string(),
            ));
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
            original_invocation.to_safety_args(&original_safety_shell, use_login_shell);
        let original_shell_type = if original_invocation.is_argv() {
            None
        } else {
            Some(original_safety_shell.shell_type)
        };
        let preflight = preflight_invocation_with_equivalent_repair(
            &original_invocation,
            &original_safety_command,
            original_shell_type,
        )
        .map_err(|issue| {
            FunctionCallError::RespondToModel(format!(
                "{issue}\nRegenerate the command and call `shell_command` again."
            ))
        })?;
        let command_invocation = preflight.invocation;
        let repair_notice = preflight.repair_notice;
        let repository = get_git_repo_root(cwd.as_path()).unwrap_or_else(|| cwd.to_path_buf());
        let repository_key = repository.to_string_lossy();
        let validation_launch = match admit_validation(
            &turn.validation_authorization,
            session.services.state_db.as_deref(),
            repository_key.as_bytes(),
            &command_invocation,
        )
        .await
        {
            ValidationAdmission::Skip(skipped) => {
                tracing::info!(reason = ?skipped.reason, "validation command skipped");
                return Ok(boxed_tool_output(validation_structured_output(
                    serde_json::to_value(skipped).unwrap_or_default(),
                )));
            }
            ValidationAdmission::Execute {
                authorization_revision,
                observation,
            } => observation.map(|observation| ValidationLaunchPlan {
                invocation: command_invocation.clone(),
                authorization_revision,
                observation: Some(observation),
            }),
        };
        let hook_command = command_invocation.display_command();
        maybe_emit_implicit_skill_invocation(session.as_ref(), turn.as_ref(), &hook_command, &cwd)
            .await;
        let safety_shell = resolve_command_shell(
            &command_invocation,
            &turn_environment,
            session_shell.as_ref(),
        )?;
        let safety_command = command_invocation.to_safety_args(&safety_shell, use_login_shell);
        let shell_type = if command_invocation.is_argv() {
            None
        } else {
            Some(safety_shell.shell_type)
        };
        let is_powershell_script = command_invocation.is_powershell_script();
        let exec_params = Self::to_exec_params(
            &params,
            &command_invocation,
            session.as_ref(),
            turn.as_ref(),
            &turn_environment,
            cwd,
            allow_login_shell,
        )?;
        let sandbox_context = format!(
            "requested={:?};additional={:?};approval={:?};profile={:?};windows={:?};private_desktop={}",
            params.sandbox_permissions.unwrap_or_default(),
            params.additional_permissions,
            turn.approval_policy.value(),
            turn.permission_profile(),
            turn.windows_sandbox_level,
            exec_params.windows_sandbox_private_desktop,
        );
        let runtime_context = format!(
            "backend={:?};shell={shell_type:?};login={use_login_shell};capture={:?};network_environment={:?};network={:?}",
            self.backend,
            exec_params.capture_policy,
            exec_params.network_environment_id,
            exec_params.network,
        );
        let observed_mutation_revision = tracker.lock().await.current_mutation_revision();
        let ValidationRegistrationRoles {
            execution: validation_leader,
            worker_waiter: validation_waiter,
            owner_waiter: validation_owner_waiter,
        } = if validation_launch.is_some() {
            let environment = super::validation_environment_hash(&exec_params.env);
            let toolchain = super::child_env_value(&exec_params.env, "RUSTUP_TOOLCHAIN")
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default();
            let identity = validation_identity(
                repository_key.as_bytes(),
                exec_params.cwd.to_string_lossy(),
                &command_invocation,
                environment,
                toolchain,
                observed_mutation_revision,
            );
            validation_registration_roles(
                register_if_absent(
                    &turn.validation_singleflight,
                    identity,
                    &call_id,
                    &cancellation_token,
                )
                .await,
            )
        } else {
            ValidationRegistrationRoles::default()
        };
        let repository_epoch = session
            .services
            .command_execution
            .observe_repository_revision(&turn.sub_id, observed_mutation_revision)
            .await;
        let attempt_key = CommandAttemptKey::new(
            tool_name.name.as_str(),
            &turn_environment.environment_id,
            exec_params.cwd.to_string_lossy().into_owned(),
            &original_safety_command,
        )
        .with_executed_command(&exec_params.command)
        .with_environment(&exec_params.env)
        .with_timeout_ms(exec_params.expiration.timeout_ms())
        .with_sandbox_context(&sandbox_context)
        .with_input_context(&prefix_rule)
        .with_runtime_context(&runtime_context)
        .with_repository_epoch(repository_epoch);
        let mut run_args = RunExecLikeArgs {
            tool_name,
            exec_params,
            cancellation_token,
            hook_command,
            safety_command,
            shell_type,
            is_powershell_script,
            additional_permissions: params.additional_permissions.clone(),
            prefix_rule,
            session,
            turn,
            turn_environment,
            tracker,
            call_id,
            shell_runtime_backend: self.shell_runtime_backend(),
            track_validation_freshness: true,
            attempt_key: Some(attempt_key),
            repair_notice,
            force_fresh: params.force_fresh.unwrap_or(false),
            validation_launch,
            validation_leader,
            validation_waiter,
        };
        if let Some(leader) = run_args.validation_leader.as_ref() {
            run_args.cancellation_token = leader.cancellation_token();
            await_validation_execution(
                tokio::spawn(async move { run_exec_like(run_args).await }),
                validation_owner_waiter,
            )
            .await
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "shared validation execution task failed: {error}"
                ))
            })?
            .map(boxed_tool_output)
        } else {
            run_exec_like(run_args).await.map(boxed_tool_output)
        }
    }
}

pub(super) fn validation_structured_output(value: serde_json::Value) -> FunctionToolOutput {
    let text = value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string());
    let success = value
        .get("success")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let mut output = FunctionToolOutput::from_text(text, Some(success));
    output.post_tool_use_response = Some(value);
    output
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
        let command = shell_command_payload_command(&invocation.payload)?;
        Some(PostToolUsePayload {
            tool_name: HookToolName::shell_command(),
            tool_use_id: invocation.call_id.clone(),
            tool_input: serde_json::json!({ "command": command }),
            tool_response,
        })
    }
}
