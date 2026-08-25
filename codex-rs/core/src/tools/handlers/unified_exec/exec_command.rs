use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use crate::FunctionCallError;
use crate::agent::task_capabilities::validate_independent_review_shell;
use crate::maybe_emit_implicit_skill_invocation;
use crate::shell::Shell;
use crate::shell::ShellType;
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::command_execution::CompletionApplyResult;
use crate::tools::command_output_artifact::create_raw_output_artifact;
use crate::tools::command_output_artifact::replace_raw_output_artifact;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::handlers::apply_patch::intercept_apply_patch;
use crate::tools::handlers::command_preflight::preflight_invocation_with_equivalent_repair;
use crate::tools::handlers::command_search::classify_rg_search_narrowing;
use crate::tools::handlers::command_search::reject_rg_search_without_native_scope;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::tools::handlers::command_shape::powershell_script_failure_advisory;
use crate::tools::handlers::implicit_granted_permissions;
use crate::tools::handlers::normalize_and_validate_additional_permissions;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::parse_arguments_with_base_path;
use crate::tools::handlers::resolve_repository_root;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::rewrite_function_command_invocation;
use crate::tools::hook_names::HookToolName;
use crate::tools::known_delta_store;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutionTiming;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::generate_chunk_id;
use crate::validation_admission::ValidationAdmission;
use crate::validation_admission::ValidationLaunchPlan;
use crate::validation_admission::ValidationRegistration;
use crate::validation_admission::admit_validation;
use codex_features::Feature;
use codex_otel::SessionTelemetry;
use codex_otel::TOOL_CALL_UNIFIED_EXEC_METRIC;
use codex_sandboxing::SandboxType;
use codex_sandboxing::SandboxablePreference;
use codex_sandboxing::select_initial;
use codex_shell_command::is_safe_command::is_known_safe_command;
use codex_shell_command::shell_detect::detect_shell_type;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_path_uri::PathConvention;
use serde::Deserialize;

use super::super::shell::ValidationProofPreparation;
use super::super::shell::ValidationProofPreparationArgs;
use super::super::shell::joined_validation_structured_output;
use super::super::shell::prepare_validation_proof;
use super::super::shell::validation_environment_hash;
use super::super::shell::validation_structured_output;
use super::super::shell_spec::CommandToolOptions;
use super::super::shell_spec::create_exec_command_tool_with_environment_id;
use super::ExecCommandArgs;
use super::ExecCommandEnvironmentArgs;
use super::get_command;
use super::post_unified_exec_tool_use_payload;

pub(super) fn completed_validation_not_applicable_output(
    response: &ExecCommandToolOutput,
    validation_result: Option<codex_protocol::validation::ValidationResult>,
) -> FunctionToolOutput {
    let value = serde_json::json!({
        "text": response.response_text(),
        "success": null,
        "execution_outcome": "executed_not_applicable",
        "command_was_executed": true,
        "exit_code": response.exit_code,
        "skip_disposition": codex_tools::ToolOutputSkipDisposition::NotApplicable,
        "validation_result": validation_result,
    });
    validation_structured_output(value)
        .with_skip_disposition(codex_tools::ToolOutputSkipDisposition::NotApplicable)
}

#[derive(Debug, Deserialize)]
struct ExecCommandHookArgs {
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    program: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    script_body: Option<String>,
}

impl ExecCommandHookArgs {
    fn command_invocation(&self) -> Result<CommandInvocation, FunctionCallError> {
        CommandInvocation::from_parts(
            "exec_command",
            "cmd",
            self.cmd.as_deref(),
            self.kind.as_deref(),
            self.program.as_deref(),
            self.args.as_deref(),
            self.script_body.as_deref(),
        )
    }
}

pub(super) fn exec_command_hook_input(arguments: &str) -> Option<serde_json::Value> {
    parse_arguments::<ExecCommandHookArgs>(arguments)
        .ok()?
        .command_invocation()
        .ok()
        .map(|invocation| invocation.hook_input())
}

#[derive(Clone, Copy)]
pub(crate) struct ExecCommandHandlerOptions {
    pub(crate) allow_login_shell: bool,
    pub(crate) exec_permission_approvals_enabled: bool,
    pub(crate) include_environment_id: bool,
    pub(crate) include_shell_parameter: bool,
}

pub struct ExecCommandHandler {
    options: ExecCommandHandlerOptions,
}

impl Default for ExecCommandHandler {
    fn default() -> Self {
        Self {
            options: ExecCommandHandlerOptions {
                allow_login_shell: false,
                exec_permission_approvals_enabled: false,
                include_environment_id: false,
                include_shell_parameter: true,
            },
        }
    }
}

impl ExecCommandHandler {
    pub(crate) fn new(options: ExecCommandHandlerOptions) -> Self {
        Self { options }
    }
}

pub(super) fn validate_and_consume_remote_shell(
    args: &mut ExecCommandArgs,
    remote_shell: Option<&Shell>,
    environment_id: &str,
) -> Result<(), String> {
    let Some(requested_shell) = args.shell.take() else {
        return Ok(());
    };
    let Some(remote_shell) = remote_shell else {
        return Err(format!(
            "environment `{environment_id}` does not report a shell"
        ));
    };
    if detect_shell_type(Path::new(&requested_shell)) != Some(remote_shell.shell_type) {
        return Err(format!(
            "environment `{environment_id}` only supports `{}`",
            remote_shell.name()
        ));
    }
    Ok(())
}

pub(super) fn attach_powershell_failure_advisory(
    response: &mut ExecCommandToolOutput,
    shell_type: ShellType,
    is_powershell_script: bool,
) {
    if response.process_id.is_some() {
        return;
    }

    let advisory = {
        let output = String::from_utf8_lossy(&response.raw_output);
        powershell_script_failure_advisory(
            Some(shell_type),
            response.exit_code,
            is_powershell_script,
            output.as_ref(),
        )
    };
    if let Some(advisory) = advisory {
        response.repair_notice = Some(match response.repair_notice.take() {
            Some(repair_notice) => format!("{repair_notice}\n\n{advisory}"),
            None => advisory.to_string(),
        });
    }
}

impl ToolExecutor<ToolInvocation> for ExecCommandHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(crate::tools::EXEC_COMMAND_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_exec_command_tool_with_environment_id(
            CommandToolOptions {
                allow_login_shell: self.options.allow_login_shell,
                exec_permission_approvals_enabled: self.options.exec_permission_approvals_enabled,
            },
            self.options.include_environment_id,
            self.options.include_shell_parameter,
        )
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl ExecCommandHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            tracker,
            call_id,
            cancellation_token,
            payload,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);
        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "exec_command handler received unsupported payload".to_string(),
                ));
            }
        };

        let manager: &UnifiedExecProcessManager = &session.services.unified_exec_manager;
        let context = UnifiedExecContext::with_tracker(
            session.clone(),
            turn.clone(),
            call_id.clone(),
            tracker.clone(),
        );
        let environment_args: ExecCommandEnvironmentArgs = parse_arguments(&arguments)?;
        let Some(turn_environment) = resolve_tool_environment(
            &step_context.environments,
            environment_args.environment_id.as_deref(),
        )?
        else {
            return Err(FunctionCallError::RespondToModel(
                "unified exec is unavailable in this session".to_string(),
            ));
        };
        let native_environment_cwd = turn_environment.cwd().clone();
        let cwd = environment_args
            .workdir
            .as_deref()
            .filter(|workdir| !workdir.is_empty())
            .map_or_else(
                || Ok(native_environment_cwd.clone()),
                |workdir| native_environment_cwd.join(workdir),
            )
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let environment = Arc::clone(&turn_environment.environment);
        let fs = environment.get_filesystem();

        // A foreign cwd cannot seed the AbsolutePathBufGuard used to resolve relative paths in the
        // permissions config below. Consult the configured platform-sandbox requirement before
        // deciding whether parsing may continue without that base path.
        let sandbox = select_initial(
            &turn.file_system_sandbox_policy(),
            turn.network_sandbox_policy(),
            SandboxablePreference::Auto,
            turn.windows_sandbox_level,
            turn.network.is_some(),
        );
        // `to_abs_path()` alone cannot identify foreign drive paths: `file:///C:/repo` is
        // representable as `/C:/repo` on POSIX. Require the inferred convention to match too.
        let cwd_uses_native_convention =
            cwd.infer_path_convention() == Some(PathConvention::native());
        // TODO(anp): Remove this parsing split once sandboxing supports foreign paths.
        let native_cwd = match cwd.to_abs_path() {
            Ok(cwd) if cwd_uses_native_convention => Some(cwd),
            _ if sandbox == SandboxType::None => None,
            Err(err) => return Err(FunctionCallError::RespondToModel(err.to_string())),
            Ok(_) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "path URI `{cwd}` does not use the host's native {} path convention",
                    PathConvention::native()
                )));
            }
        };
        let mut args: ExecCommandArgs = match native_cwd.as_ref() {
            Some(native_cwd) => {
                // The base path only resolves paths nested in the permissions config types.
                parse_arguments_with_base_path(&arguments, native_cwd)?
            }
            None => {
                // Parsing without a base only skips relative-path resolution inside the
                // permissions config. That is safe only for a truly unsandboxed attempt;
                // sandboxed attempts fall through and return the conversion error below.
                parse_arguments(&arguments)?
            }
        };
        let original_invocation = args
            .command_invocation()
            .map_err(FunctionCallError::RespondToModel)?;
        let environment_is_remote = environment.is_remote();
        if environment_is_remote && !original_invocation.is_argv() {
            if turn_environment.shell.is_none() {
                return Err(FunctionCallError::RespondToModel(format!(
                    "environment `{}` does not report a shell",
                    turn_environment.environment_id
                )));
            }
            // TODO(anp): Resolve requested shells in remote environments instead of restricting
            // commands to the reported default shell.
            validate_and_consume_remote_shell(
                &mut args,
                turn_environment.shell.as_ref(),
                &turn_environment.environment_id,
            )
            .map_err(FunctionCallError::RespondToModel)?;
        }
        // A remote shell is required above for every shell-wrapped command. The local session
        // fallback can therefore only be reached by local commands or structured remote argv.
        let shell = turn_environment
            .shell
            .clone()
            .map(Arc::new)
            .unwrap_or_else(|| session.user_shell());
        let original_resolved_command = get_command(
            &args,
            Arc::clone(&shell),
            turn.config.permissions.allow_login_shell,
            environment_is_remote,
        )
        .map_err(FunctionCallError::RespondToModel)?;
        let original_safety_command = original_resolved_command.safety_command.clone();
        let preflight = preflight_invocation_with_equivalent_repair(
            &original_invocation,
            &original_safety_command,
            original_resolved_command.preflight_shell_type,
        )
        .map_err(|issue| {
            FunctionCallError::RespondToModel(format!(
                "{issue}\nRegenerate the command and call `exec_command` again."
            ))
        })?;
        let repaired = preflight.repaired();
        let command_invocation = preflight.invocation;
        let repair_notice = preflight.repair_notice;
        if repaired {
            args.replace_command_invocation(&command_invocation);
        }
        let resolved_command = if repair_notice.is_some() {
            get_command(
                &args,
                Arc::clone(&shell),
                turn.config.permissions.allow_login_shell,
                environment_is_remote,
            )
            .map_err(FunctionCallError::RespondToModel)?
        } else {
            original_resolved_command
        };
        let native_repository = native_cwd
            .as_ref()
            .map(|cwd| resolve_repository_root(cwd.as_path()));
        let repository_key = native_repository
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.to_string());
        let search_narrowing = if let Some(native_cwd) = native_cwd.as_ref() {
            let repository_root = native_repository.as_deref().unwrap_or(native_cwd.as_path());
            let search = classify_rg_search_narrowing(
                &resolved_command.safety_command,
                resolved_command.preflight_shell_type,
                native_cwd.as_path(),
                repository_root,
            )
            .map_err(FunctionCallError::RespondToModel)?;
            search.map(|search| (repository_root.to_string_lossy().into_owned(), search))
        } else {
            reject_rg_search_without_native_scope(
                &resolved_command.safety_command,
                resolved_command.preflight_shell_type,
            )
            .map_err(FunctionCallError::RespondToModel)?;
            None
        };
        let mut validation_launch = match admit_validation(
            &turn.validation_authorization,
            session.services.state_db.as_deref(),
            repository_key.as_bytes(),
            &command_invocation,
        )
        .await
        {
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
                observation,
            } => observation.map(|observation| ValidationLaunchPlan {
                invocation: command_invocation.clone(),
                authorization_revision,
                observation: Some(observation),
                proof_key: None,
                structured_route: None,
                bound_plan_step: None,
                validation_call_id: None,
                turn_timing_state: Some(Arc::clone(&turn.turn_timing_state)),
                force_fresh: args.force_fresh,
            }),
        };
        let direct_validation_route = if validation_launch.is_some() {
            let context = args.validation.as_ref().ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "validation commands require `validation` metadata stating the uncertainty, covered_paths, and covered_contracts"
                        .to_string(),
                )
            })?;
            let repository = native_repository.as_ref().ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "direct validation coverage requires a host-native repository path".to_string(),
                )
            })?;
            Some(
                super::super::shell::direct_validation_route(
                    context,
                    &command_invocation,
                    repository,
                    300_000,
                )
                .map_err(FunctionCallError::RespondToModel)?,
            )
        } else {
            None
        };
        // Admission authorizes the validation; it does not prove that a nonzero outcome is
        // deterministic. All outcomes remain ordinary retryable command records here.
        let validation_observation = Arc::new(StdMutex::new(None));
        validate_independent_review_shell(
            &turn.session_source,
            is_known_safe_command(&resolved_command.safety_command),
            args.sandbox_permissions.requests_sandbox_override(),
            args.additional_permissions.is_some(),
        )
        .map_err(|message| FunctionCallError::RespondToModel(message.to_string()))?;
        let hook_command = command_invocation.display_command();
        // Implicit skill detection requires a native path, so foreign PathUri
        // workdirs are intentionally skipped here.
        if let Some(native_cwd) = native_cwd.as_ref() {
            maybe_emit_implicit_skill_invocation(
                session.as_ref(),
                context.turn.as_ref(),
                &hook_command,
                native_cwd,
            )
            .await;
        }
        let command = resolved_command.command;
        let safety_command = resolved_command.safety_command;
        let shell_type = resolved_command.shell_type;
        let use_login_shell = resolved_command.use_login_shell;
        let is_powershell_script = command_invocation.is_powershell_script();
        let command_for_display = hook_command.clone();

        let ExecCommandArgs {
            tty,
            yield_time_ms,
            max_output_tokens,
            sandbox_permissions,
            additional_permissions,
            justification,
            prefix_rule,
            force_fresh,
            ..
        } = args;

        let exec_permission_approvals_enabled =
            session.features().enabled(Feature::ExecPermissionApprovals);
        let requested_additional_permissions = additional_permissions.clone();
        // TODO(anp): Make permission matching operate on PathUri for remote environments.
        let permission_cwd = native_cwd.as_ref().unwrap_or(&turn.config.cwd);
        let effective_additional_permissions = apply_granted_turn_permissions(
            context.session.as_ref(),
            &turn_environment.environment_id,
            permission_cwd.as_path(),
            sandbox_permissions,
            additional_permissions,
        )
        .await;
        let additional_permissions_allowed = exec_permission_approvals_enabled
            || (session.features().enabled(Feature::RequestPermissionsTool)
                && effective_additional_permissions.permissions_preapproved);

        // Sticky turn permissions have already been approved, so they should
        // continue through the normal exec approval flow for the command.
        if effective_additional_permissions
            .sandbox_permissions
            .requests_sandbox_override()
            && !effective_additional_permissions.permissions_preapproved
            && !matches!(
                context.turn.approval_policy.value(),
                codex_protocol::protocol::AskForApproval::OnRequest
            )
        {
            let approval_policy = context.turn.approval_policy.value();
            return Err(FunctionCallError::RespondToModel(format!(
                "approval policy is {approval_policy:?}; reject command — you cannot ask for escalated permissions if the approval policy is {approval_policy:?}"
            )));
        }

        let normalized_additional_permissions = match implicit_granted_permissions(
            sandbox_permissions,
            requested_additional_permissions.as_ref(),
            &effective_additional_permissions,
        )
        .map_or_else(
            || {
                normalize_and_validate_additional_permissions(
                    additional_permissions_allowed,
                    context.turn.approval_policy.value(),
                    effective_additional_permissions.sandbox_permissions,
                    effective_additional_permissions.additional_permissions,
                    effective_additional_permissions.permissions_preapproved,
                    permission_cwd,
                )
            },
            |permissions| Ok(Some(permissions)),
        ) {
            Ok(normalized) => normalized,
            Err(err) => {
                return Err(FunctionCallError::RespondToModel(err));
            }
        };

        let sandbox_context = (
            sandbox_permissions,
            effective_additional_permissions.sandbox_permissions,
            &normalized_additional_permissions,
            effective_additional_permissions.permissions_preapproved,
            context.turn.approval_policy.value(),
            context.turn.windows_sandbox_level,
        );
        let runtime_context = format!(
            "shell={shell_type:?};login={use_login_shell};tty={tty};network={:?}",
            context.turn.network,
        );
        let input_context = format!("prefix={prefix_rule:?}");
        let effective_environment = manager.effective_environment(&context);
        let environment_hash = validation_environment_hash(&effective_environment);
        let observed_mutation_revision = tracker.lock().await.current_mutation_revision();
        let repository_epoch = session
            .services
            .command_execution
            .observe_repository_revision(&turn.sub_id, observed_mutation_revision)
            .await;
        let workspace_identity = match cwd.to_abs_path() {
            Ok(cwd) => {
                session
                    .services
                    .command_execution
                    .current_workspace_identity_hash(
                        &turn_environment.environment_id,
                        cwd.as_path(),
                    )
                    .await
            }
            Err(_) => None,
        };
        let validation_cwd = cwd.to_string();
        let attempt_key = CommandAttemptKey::new(
            self.tool_name().name.as_str(),
            &turn_environment.environment_id,
            validation_cwd.clone(),
            &command,
        )
        .with_environment_fingerprint(&environment_hash)
        .with_timeout_ms(None)
        .with_sandbox_context(&sandbox_context)
        .with_permission_context(&sandbox_context)
        .with_input_context(&input_context)
        .with_runtime_context(&runtime_context)
        .with_repository_epoch(repository_epoch)
        .with_workspace_identity(workspace_identity.as_deref());
        let attempt_key = if let Some((repository_identity, search)) = search_narrowing {
            attempt_key.with_search_narrowing(&turn.sub_id, &repository_identity, Some(search))
        } else {
            attempt_key
        };
        session
            .services
            .command_execution
            .admit_search_narrowing(&attempt_key)
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let known_delta = if session.features().enabled(Feature::KnownDeltaStore)
            && !environment_is_remote
            && !tty
            && validation_launch.is_none()
            && let Some(native_cwd) = native_cwd.as_ref()
            && let CommandInvocation::Argv { program, args } = &command_invocation
            && known_delta_store::is_immutable_git_show_candidate(program, args)
        {
            let metadata_source = turn
                .turn_metadata_state
                .git_metadata_source()
                .filter(|source| native_cwd.starts_with(source.repo_root().as_path()));
            let project_namespace = match &metadata_source {
                Some(source) => source.project_namespace().await,
                None => None,
            };
            let project_namespace_hint = metadata_source
                .map_or(known_delta_store::ProjectNamespaceHint::Discover, |_| {
                    known_delta_store::ProjectNamespaceHint::Resolved(project_namespace.as_deref())
                });
            known_delta_store::prepare_immutable_git_show(
                turn.config.codex_home.as_path(),
                &session.thread_id.to_string(),
                native_cwd,
                program,
                args,
                project_namespace_hint,
                force_fresh,
            )
            .await
        } else {
            None
        };
        let known_delta_hit = known_delta
            .as_ref()
            .is_some_and(crate::tools::known_delta_store::PreparedKnownDelta::is_hit);
        let (validation_leader, validation_waiter) = loop {
            match prepare_validation_proof(ValidationProofPreparationArgs {
                session: session.as_ref(),
                turn: turn.as_ref(),
                validation_launch: &mut validation_launch,
                direct_validation_route: direct_validation_route.as_ref(),
                repository_key: repository_key.as_bytes(),
                cwd: &validation_cwd,
                command_invocation: &command_invocation,
                environment: &effective_environment,
                environment_hash: &environment_hash,
                execution_context: &runtime_context,
                repository_epoch,
                call_id: &call_id,
                cancellation_token: &cancellation_token,
                force_fresh,
            })
            .await?
            {
                ValidationProofPreparation::NotValidation => break (None, None),
                ValidationProofPreparation::Reused(output) => {
                    return Ok(boxed_tool_output(output));
                }
                ValidationProofPreparation::Registered(ValidationRegistration::Leader {
                    execution,
                    waiter,
                }) => break (Some(*execution), Some(waiter)),
                ValidationProofPreparation::Registered(ValidationRegistration::Follower(
                    waiter,
                )) => {
                    let shared_from_call_id = waiter.shared_from_call_id().to_string();
                    let joined = tokio::select! {
                        result = waiter.join() => result,
                        _ = cancellation_token.cancelled() => {
                            return Err(FunctionCallError::RespondToModel(
                                "shared validation wait was cancelled".to_string(),
                            ));
                        }
                    };
                    if let Some(result) = joined {
                        return Ok(boxed_tool_output(joined_validation_structured_output(
                            result.value,
                            &call_id,
                            &shared_from_call_id,
                        )));
                    }
                    tokio::task::yield_now().await;
                }
            }
        };
        if !known_delta_hit {
            session
                .services
                .command_execution
                .begin_attempt_with_freshness(&attempt_key, repair_notice.is_some(), force_fresh)
                .await
                .map_err(|blocked| FunctionCallError::RespondToModel(blocked.render_for_model()))?;
        }
        let validation_leader = Arc::new(StdMutex::new(validation_leader));
        let interception_started_at = std::time::Instant::now();
        let intercepted = intercept_apply_patch(
            &command,
            &cwd,
            fs.as_ref(),
            turn_environment.clone(),
            context.session.clone(),
            context.turn.clone(),
            Some(&tracker),
            &context.call_id,
            "exec_command",
        )
        .await;
        let interception_wall_time = interception_started_at.elapsed();
        let observed_mutation_revision = tracker.lock().await.current_mutation_revision();
        session
            .services
            .command_execution
            .observe_repository_revision(&turn.sub_id, observed_mutation_revision)
            .await;
        match intercepted {
            Ok(Some(output)) => {
                let raw_output = output.into_text().into_bytes();
                let raw_output_artifact = create_raw_output_artifact(
                    turn.config.codex_home.as_path(),
                    &session.thread_id.to_string(),
                    &raw_output,
                )
                .await;
                if !known_delta_hit {
                    session
                        .services
                        .command_execution
                        .record_exit(&attempt_key, 0)
                        .await;
                }
                return Ok(boxed_tool_output(ExecCommandToolOutput {
                    event_call_id: String::new(),
                    chunk_id: String::new(),
                    wall_time: interception_wall_time,
                    raw_output,
                    truncation_policy: turn.model_info.truncation_policy.into(),
                    max_output_tokens,
                    process_id: None,
                    exit_code: Some(0),
                    original_token_count: None,
                    hook_command: Some(hook_command),
                    raw_output_artifact: Some(raw_output_artifact),
                    repair_notice,
                }));
            }
            Ok(None) => {}
            Err(err) => {
                if !known_delta_hit {
                    err.record_attempt_failure(&session.services.command_execution, &attempt_key)
                        .await;
                }
                return Err(err.into_error());
            }
        }

        // Carry only an in-memory target through launch. The output task
        // materializes durable storage only after output exceeds the inline
        // projection threshold, so process startup never waits on artifact
        // creation, fsync, directory sync, or retention enforcement.
        let raw_output_artifact = crate::tools::command_output_artifact::RawOutputArtifact::pending(
            turn.config.codex_home.as_path(),
            &session.thread_id.to_string(),
        );
        emit_unified_exec_tty_metric(&turn.session_telemetry, tty);
        let process_id_reservation = manager.reserve_process_id().await;
        let process_id = process_id_reservation.process_id();
        let exec_result = manager
            .exec_command(
                ExecCommandRequest {
                    command,
                    command_for_safety: safety_command,
                    attempt_key: attempt_key.clone(),
                    raw_output_artifact: raw_output_artifact.clone(),
                    shell_type,
                    hook_command: hook_command.clone(),
                    process_id,
                    yield_time_ms,
                    max_output_tokens,
                    cwd,

                    normalization_cwd: if turn_environment.environment.is_remote() {
                        None
                    } else {
                        native_cwd.as_ref().map(|cwd| cwd.as_path().to_path_buf())
                    },
                    sandbox_cwd: native_environment_cwd,
                    turn_environment: turn_environment.clone(),
                    network: context.turn.network.clone(),
                    tty,
                    sandbox_permissions: effective_additional_permissions.sandbox_permissions,
                    additional_permissions: normalized_additional_permissions,
                    additional_permissions_preapproved: effective_additional_permissions
                        .permissions_preapproved,
                    justification,
                    prefix_rule,
                    validation_launch,
                    validation_observation,
                    validation_leader,
                    validation_waiter,
                    known_delta,
                },
                process_id_reservation,
                &context,
            )
            .await;
        let observed_mutation_revision = tracker.lock().await.current_mutation_revision();
        session
            .services
            .command_execution
            .observe_repository_revision(&turn.sub_id, observed_mutation_revision)
            .await;
        let tracked_execution = session
            .services
            .command_execution
            .process_execution_identity(process_id)
            .await;
        let mut background_process_expected = false;
        let result = match exec_result {
            Ok(mut response) => {
                background_process_expected = response.process_id.is_some();
                let finalized_artifact = response
                    .raw_output_artifact
                    .clone()
                    .unwrap_or_else(|| raw_output_artifact.clone());
                response.repair_notice = repair_notice;
                if !known_delta_hit {
                    if let Some(process_id) = response.process_id {
                        session
                            .services
                            .command_execution
                            .update_running_artifact(process_id, finalized_artifact)
                            .await;
                    } else if let Some(exit_code) = response.exit_code {
                        let tracked = if let Some((execution_id, parent_tool_execution_id)) =
                            tracked_execution.as_ref()
                        {
                            session
                                .services
                                .command_execution
                                .finish_running_process_with_execution_id(
                                    process_id,
                                    *execution_id,
                                    parent_tool_execution_id,
                                    Some(exit_code),
                                )
                                .await
                        } else {
                            CompletionApplyResult::Missing
                        };
                        if !matches!(
                            tracked,
                            CompletionApplyResult::Applied | CompletionApplyResult::AlreadyApplied
                        ) {
                            session
                                .services
                                .command_execution
                                .record_exit(&attempt_key, exit_code)
                                .await;
                        }
                    }
                }
                attach_powershell_failure_advisory(&mut response, shell_type, is_powershell_script);
                let skip_disposition = direct_validation_route.as_ref().and_then(|route| {
                    response.exit_code.and_then(|exit_code| {
                        crate::tools::command_execution::completed_validation_skip_disposition(
                            route.route(),
                            &response.raw_output,
                            exit_code,
                        )
                    })
                });
                if skip_disposition == Some(codex_tools::ToolOutputSkipDisposition::NotApplicable) {
                    let validation_result = session
                        .services
                        .command_execution
                        .validation_result_for_call(&call_id)
                        .await;
                    Ok(boxed_tool_output(
                        completed_validation_not_applicable_output(&response, validation_result),
                    ))
                } else {
                    Ok(boxed_tool_output(response))
                }
            }
            Err(UnifiedExecError::SandboxDenied { output, .. }) => {
                let output_text = output.aggregated_output.text;
                let finalized_artifact =
                    replace_raw_output_artifact(&raw_output_artifact, output_text.as_bytes()).await;
                if !known_delta_hit {
                    let tracked = if let Some((execution_id, parent_tool_execution_id)) =
                        tracked_execution.as_ref()
                    {
                        session
                            .services
                            .command_execution
                            .finish_running_process_with_execution_id(
                                process_id,
                                *execution_id,
                                parent_tool_execution_id,
                                Some(output.exit_code),
                            )
                            .await
                    } else {
                        CompletionApplyResult::Missing
                    };
                    if !matches!(
                        tracked,
                        CompletionApplyResult::Applied | CompletionApplyResult::AlreadyApplied
                    ) {
                        session
                            .services
                            .command_execution
                            .record_exit(&attempt_key, output.exit_code)
                            .await;
                    }
                }
                let original_token_count = approx_token_count(&output_text);
                let mut response = ExecCommandToolOutput {
                    event_call_id: context.call_id.clone(),
                    chunk_id: generate_chunk_id(),
                    wall_time: output.duration,
                    raw_output: output_text.into_bytes(),
                    truncation_policy: turn.model_info.truncation_policy.into(),
                    max_output_tokens,
                    // Sandbox denial is terminal, so there is no live
                    // process for write_stdin to resume.
                    process_id: None,
                    exit_code: Some(output.exit_code),
                    original_token_count: Some(original_token_count),
                    hook_command: Some(hook_command),
                    raw_output_artifact: Some(finalized_artifact),
                    repair_notice,
                };
                attach_powershell_failure_advisory(&mut response, shell_type, is_powershell_script);
                Ok(boxed_tool_output(response))
            }
            Err(UnifiedExecError::ValidationSkipped(skipped)) => {
                let skip_disposition = skipped.skip_disposition;
                Ok(boxed_tool_output(
                    validation_structured_output(serde_json::to_value(skipped).unwrap_or_default())
                        .with_skip_disposition(skip_disposition),
                ))
            }
            Err(err) => {
                let retry_failure = matches!(
                    &err,
                    UnifiedExecError::CreateProcess { .. } | UnifiedExecError::ProcessFailed { .. }
                );
                if retry_failure && !known_delta_hit {
                    let finalized_running_process =
                        if matches!(&err, UnifiedExecError::ProcessFailed { .. }) {
                            if let Some((execution_id, parent_tool_execution_id)) =
                                tracked_execution.as_ref()
                            {
                                session
                                    .services
                                    .command_execution
                                    .finish_running_process_with_execution_id(
                                        process_id,
                                        *execution_id,
                                        parent_tool_execution_id,
                                        Some(-1),
                                    )
                                    .await
                            } else {
                                CompletionApplyResult::Missing
                            }
                        } else {
                            CompletionApplyResult::Missing
                        };
                    if !matches!(
                        finalized_running_process,
                        CompletionApplyResult::Applied | CompletionApplyResult::AlreadyApplied
                    ) {
                        session
                            .services
                            .command_execution
                            .record_exit(&attempt_key, -1)
                            .await;
                    }
                }
                let repair = repair_notice
                    .as_deref()
                    .map_or(String::new(), |notice| format!("\n{notice}"));
                Err(FunctionCallError::RespondToModel(format!(
                    "exec_command failed for `{command_for_display}`: {err:?}{repair}"
                )))
            }
        };
        let running_process_after_cleanup = session
            .services
            .command_execution
            .running_process(process_id)
            .await
            .is_some();
        crate::tools::tool_dispatch_trace::record_exec_cleanup_state(
            background_process_expected,
            running_process_after_cleanup,
        );
        result
    }
}

impl CoreToolRuntime for ExecCommandHandler {
    fn tool_execution_timing(&self) -> ToolExecutionTiming {
        ToolExecutionTiming::NestedRuntime
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        exec_command_hook_input(arguments).map(|tool_input| PreToolUsePayload {
            tool_name: HookToolName::exec_command(),
            tool_input,
        })
    }

    fn with_updated_hook_input(
        &self,
        mut invocation: ToolInvocation,
        updated_input: serde_json::Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        let ToolPayload::Function { arguments } = invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "hook input rewrite received unsupported exec_command payload".to_string(),
            ));
        };
        let args: ExecCommandHookArgs = parse_arguments(&arguments)?;
        let command_invocation = args.command_invocation()?;
        invocation.payload = ToolPayload::Function {
            arguments: rewrite_function_command_invocation(
                &arguments,
                "exec_command",
                "cmd",
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
        post_unified_exec_tool_use_payload(invocation, result)
    }
}

fn emit_unified_exec_tty_metric(session_telemetry: &SessionTelemetry, tty: bool) {
    session_telemetry.counter(
        TOOL_CALL_UNIFIED_EXEC_METRIC,
        /*inc*/ 1,
        &[("tty", if tty { "true" } else { "false" })],
    );
}
