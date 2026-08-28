use codex_agent_task_store::ValidationCallStatus;
use codex_agent_task_store::ValidationEvidence;
use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::validation::ValidationResult;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::FunctionCallError;
use crate::agent::task_capabilities::validate_independent_review_shell;
use crate::agent::task_coordinator::AgentTaskCoordinator;
use crate::agent::task_coordinator::FocusedValidationToken;
use crate::exec::ExecParams;
use crate::exec_policy::ExecApprovalRequest;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellType;
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::command_output_artifact::create_raw_output_artifact;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolOutput;
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
use crate::tools::handlers::resolve_repository_root;
use crate::tools::known_delta_store;
use crate::tools::known_delta_store::KnownDeltaExecutionObservation;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::runtimes::prove_noprofile_powershell_direct_argv_async;
use crate::tools::runtimes::shell::ShellRequest;
use crate::tools::runtimes::shell::ShellRuntime;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;

use crate::tools::sandboxing::same_exec_authorization_envelope;
use crate::validation_admission::ValidationLaunchPlan;
use crate::validation_admission::ValidationSkippedToolOutput;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::plan_tool::ValidationRouteLeaf;
use codex_protocol::protocol::ExecCommandSource;
use codex_shell_command::is_safe_command::is_known_safe_command;

use codex_tools::CanonicalToolResult;
use codex_tools::ToolName;
use codex_tools::ToolOutputProjectionFragment;
use codex_tools::ToolOutputProjectionFragmentKind;
use codex_tools::ToolOutputProjectionMetadata;
use codex_tools::ToolOutputProjectionRange;
use codex_utils_path_uri::PathUri;

mod shell_command;

pub use shell_command::ShellCommandHandler;
pub(crate) use shell_command::ShellCommandHandlerOptions;

const MAX_VALIDATION_TIMEOUT_MS: u64 =
    codex_protocol::plan_tool::MAX_STRUCTURED_VALIDATION_TIMEOUT_MS;
const FOCUSED_VALIDATION_HEARTBEAT_INTERVAL: Duration =
    Duration::from_secs(codex_agent_task_store::MAX_VALIDATION_LEASE_SECONDS as u64 / 4);

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

pub(super) struct RunExecLikeArgs {
    pub(super) tool_name: ToolName,
    pub(super) exec_params: ExecParams,
    pub(super) stall_timeout_ms: Option<u64>,
    pub(super) cancellation_token: CancellationToken,
    pub(super) hook_command: String,
    pub(super) safety_command: Vec<String>,
    pub(super) shell_type: Option<ShellType>,
    pub(super) shell_wrapper_is_owned: bool,
    pub(super) is_powershell_script: bool,
    pub(super) additional_permissions: Option<AdditionalPermissionProfile>,
    pub(super) prefix_rule: Option<Vec<String>>,
    pub(super) session: Arc<crate::session::session::Session>,
    pub(super) turn: Arc<TurnContext>,
    pub(super) turn_environment: TurnEnvironment,
    pub(super) tracker: crate::tools::context::SharedTurnDiffTracker,
    pub(super) call_id: String,
    pub(super) track_command_mutations: bool,
    pub(super) attempt_key: Option<CommandAttemptKey>,
    pub(super) repair_notice: Option<String>,
    pub(super) command_repaired: bool,
    pub(super) force_fresh: bool,
    pub(super) validation_launch: Option<crate::validation_admission::ValidationLaunchPlan>,
}

pub(super) struct RunExecLikeResult {
    pub(super) output: FunctionToolOutput,
    pub(super) exit_code: Option<i32>,
    pub(super) validation_execution_outcome: ValidationExecutionOutcome,
    pub(super) canonical_output: Option<Vec<u8>>,
}

pub(super) fn shell_failure_sampling_signal(
    attempt_key: Option<&CommandAttemptKey>,
    command: &str,
    exit_code: Option<i32>,
) -> Option<JsonValue> {
    if exit_code == Some(0) {
        return None;
    }
    let action_fingerprint = attempt_key
        .map(CommandAttemptKey::fingerprint)
        .unwrap_or_else(|| format!("{:x}", Sha256::digest(command.as_bytes())));
    let outcome_fingerprint = exit_code
        .map(|code| format!("exit-{code}"))
        .unwrap_or_else(|| "timeout".to_string());
    Some(json!({
        "outcome": "failure",
        "failure": {
            "fingerprint": format!(
                "shell.{action_fingerprint}.{outcome_fingerprint}"
            ),
        },
    }))
}

pub(super) fn shell_sampling_signal(
    attempt_key: Option<&CommandAttemptKey>,
    command: &str,
    exit_code: Option<i32>,
    canonical_output: Option<&[u8]>,
) -> Option<JsonValue> {
    shell_failure_sampling_signal(attempt_key, command, exit_code).or_else(|| {
        (exit_code == Some(0)).then(|| {
            crate::tools::context::semantic_evidence_sampling_signal(serde_json::json!(
                crate::tools::context::semantic_evidence_for_command_output(
                    canonical_output.unwrap_or_default(),
                )
            ))
        })
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ValidationExecutionOutcome {
    ExecutedSuccess,
    ExecutedFailure,
    NotExecuted,
}

fn focused_validation_status(
    result: &Result<RunExecLikeResult, FunctionCallError>,
    cancellation_requested: bool,
    has_structured_validation_result: bool,
) -> ValidationCallStatus {
    if cancellation_requested {
        return ValidationCallStatus::Cancelled;
    }
    match result {
        Ok(result) => match result.validation_execution_outcome() {
            ValidationExecutionOutcome::ExecutedSuccess => ValidationCallStatus::Succeeded,
            ValidationExecutionOutcome::ExecutedFailure => ValidationCallStatus::Failed,
            ValidationExecutionOutcome::NotExecuted => ValidationCallStatus::NotExecuted,
        },
        Err(FunctionCallError::DeniedToModel(_)) => ValidationCallStatus::Cancelled,
        Err(FunctionCallError::RespondToModel(message)) if message.contains("rejected by user") => {
            ValidationCallStatus::Cancelled
        }
        Err(_) if has_structured_validation_result => ValidationCallStatus::Failed,
        Err(_) => ValidationCallStatus::NotExecuted,
    }
}

impl ValidationExecutionOutcome {
    pub(super) fn success(self) -> Option<bool> {
        match self {
            Self::ExecutedSuccess => Some(true),
            Self::ExecutedFailure => Some(false),
            Self::NotExecuted => None,
        }
    }

    pub(super) fn from_value(value: &serde_json::Value) -> Option<Self> {
        match value.get("execution_outcome")?.as_str()? {
            "executed_success" => Some(Self::ExecutedSuccess),
            "executed_failure" => Some(Self::ExecutedFailure),
            "not_executed" => Some(Self::NotExecuted),
            _ => None,
        }
    }

    pub(super) fn tool_outcome(self) -> codex_tools::ToolOutputOutcome {
        match self {
            Self::ExecutedSuccess => codex_tools::ToolOutputOutcome::Success,
            Self::ExecutedFailure => codex_tools::ToolOutputOutcome::Failure,
            Self::NotExecuted => codex_tools::ToolOutputOutcome::Skipped,
        }
    }
}

pub(super) fn validation_structured_output(value: serde_json::Value) -> FunctionToolOutput {
    let text = value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string());
    let execution_outcome = ValidationExecutionOutcome::from_value(&value)
        .unwrap_or(ValidationExecutionOutcome::NotExecuted);
    let skip_disposition = value
        .get("skip_disposition")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    let mut output = FunctionToolOutput::from_text(text, execution_outcome.success())
        .with_outcome(execution_outcome.tool_outcome());
    if let Some(skip_disposition) = skip_disposition {
        output = output.with_skip_disposition(skip_disposition);
    } else if value
        .get("execution_outcome")
        .and_then(serde_json::Value::as_str)
        == Some("not_executed")
    {
        output = output.with_outcome(codex_tools::ToolOutputOutcome::Skipped);
    }
    output.post_tool_use_response = Some(value);
    output
}

pub(super) struct ValidationLaunchPreparationArgs<'a> {
    pub(super) session: &'a Session,
    pub(super) validation_launch: &'a mut Option<ValidationLaunchPlan>,
    pub(super) direct_validation_route: Option<&'a DirectValidationRoute>,
    pub(super) call_id: &'a str,
}

const UNATTRIBUTED_VALIDATION_ADVISORY: &str = "Validation metadata was omitted; this invocation is treated as an ordinary command and cannot be recorded as direct validation proof.";

pub(super) fn downgrade_unattributed_validation(
    validation_launch: &mut Option<ValidationLaunchPlan>,
    validation_metadata_present: bool,
    repair_notice: &mut Option<String>,
) {
    if validation_launch.is_none() || validation_metadata_present {
        return;
    }

    *validation_launch = None;
    *repair_notice = Some(match repair_notice.take() {
        Some(existing) => format!("{existing}\n\n{UNATTRIBUTED_VALIDATION_ADVISORY}"),
        None => UNATTRIBUTED_VALIDATION_ADVISORY.to_string(),
    });
}

pub(super) async fn prepare_validation_launch(
    args: ValidationLaunchPreparationArgs<'_>,
) -> Result<(), FunctionCallError> {
    if let (Some(launch), Some(route)) = (
        args.validation_launch.as_mut(),
        args.direct_validation_route,
    ) {
        let leaf = route.leaf();
        let (bound_plan_step, bound_work_unit) = args
            .session
            .services
            .task_evidence
            .direct_validation_bindings_for_leaf(leaf)
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        launch.bound_plan_step = bound_plan_step;
        launch.bound_work_unit = bound_work_unit;
        launch.structured_route = Some(route.route().clone());
        launch.validation_call_id = Some(args.call_id.to_string());
    }
    Ok(())
}

pub(super) struct LegacyShellToolOutput {
    pub(super) inner: FunctionToolOutput,
    pub(super) canonical_output: Option<Vec<u8>>,
    pub(super) exit_code: Option<i32>,
    pub(super) call_id: String,
    pub(super) validation_failure: bool,
}

impl ToolOutput for LegacyShellToolOutput {
    fn log_preview(&self) -> String {
        self.inner.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.outcome_for_logging() == codex_tools::ToolOutputOutcome::Success
    }

    fn outcome_for_logging(&self) -> codex_tools::ToolOutputOutcome {
        let outcome = self.inner.outcome_for_logging();
        if outcome != codex_tools::ToolOutputOutcome::Success {
            outcome
        } else if self.exit_code.is_some_and(|code| code != 0) {
            codex_tools::ToolOutputOutcome::Failure
        } else {
            codex_tools::ToolOutputOutcome::Success
        }
    }

    fn outcome_context(&self) -> codex_tools::ToolOutputOutcomeContext {
        let inner = self.inner.outcome_context();
        if inner.outcome != codex_tools::ToolOutputOutcome::Success {
            inner
        } else {
            codex_tools::ToolOutputOutcomeContext::new(self.outcome_for_logging())
        }
    }

    fn sampling_request_signal(&self) -> Option<JsonValue> {
        self.inner.sampling_request_signal()
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt> {
        self.inner.deterministic_continuation_receipts()
    }

    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        self.canonical_output
            .clone()
            .map(CanonicalToolResult::bytes)
            .or_else(|| self.inner.canonical_result(payload))
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        let mut metadata = self.inner.projection_metadata()?;
        let contextual_output = metadata.spillable_text.join("\n");
        metadata.fragments.insert(
            0,
            ToolOutputProjectionFragment::new(
                ToolOutputProjectionFragmentKind::ProcessFinalStatus,
                format!("process final status: exit_code={:?}", self.exit_code),
            ),
        );
        if !contextual_output.is_empty() {
            metadata.fragments.push(ToolOutputProjectionFragment::new(
                ToolOutputProjectionFragmentKind::ContextualSpillableText,
                contextual_output,
            ));
        }
        metadata.essential_inline["exit_code"] = serde_json::json!(self.exit_code);
        metadata.essential_inline["call_id"] = serde_json::json!(&self.call_id);
        if self.validation_failure
            && let Some(canonical_output) = self.canonical_output.as_deref()
            && let Ok(diagnostics) = std::str::from_utf8(canonical_output)
            && !diagnostics.is_empty()
        {
            const VALIDATION_DIAGNOSTICS_ID: &str = "validation:diagnostics";
            metadata.fragments.insert(
                1,
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::ValidationFailureOrFinalSummary,
                    diagnostics,
                )
                .with_id(VALIDATION_DIAGNOSTICS_ID),
            );
            if let Some(range) =
                validation_diagnostic_range(VALIDATION_DIAGNOSTICS_ID, canonical_output)
            {
                metadata.predetermined_ranges.push(range);
            }
        }
        Some(metadata)
    }

    fn to_response_item(
        &self,
        call_id: &str,
        payload: &ToolPayload,
    ) -> codex_protocol::models::ResponseInputItem {
        self.inner.to_response_item(call_id, payload)
    }

    fn post_tool_use_response(&self, call_id: &str, payload: &ToolPayload) -> Option<JsonValue> {
        self.inner.post_tool_use_response(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> JsonValue {
        self.inner.code_mode_result(payload)
    }
}

impl RunExecLikeResult {
    pub(super) fn validation_execution_outcome(&self) -> ValidationExecutionOutcome {
        self.validation_execution_outcome
    }
}

pub(super) async fn run_exec_like(
    args: RunExecLikeArgs,
) -> Result<LegacyShellToolOutput, FunctionCallError> {
    let call_id = args.call_id.clone();
    let validation_output_owned = args.validation_launch.is_some();
    let result = run_exec_like_with_exit_code(args).await?;
    let validation_failure = validation_output_owned
        && result.validation_execution_outcome == ValidationExecutionOutcome::ExecutedFailure;
    Ok(LegacyShellToolOutput {
        inner: result.output,
        canonical_output: result.canonical_output,
        exit_code: result.exit_code,
        call_id,
        validation_failure,
    })
}

fn validation_diagnostic_range(
    id: &str,
    canonical_output: &[u8],
) -> Option<ToolOutputProjectionRange> {
    const MAX_DIAGNOSTIC_BYTES: usize = 12 * 1024;
    const MAX_DIAGNOSTIC_LINES: usize = 200;

    if id.is_empty() || canonical_output.is_empty() || canonical_output.len() > MAX_DIAGNOSTIC_BYTES
    {
        return None;
    }
    let text = std::str::from_utf8(canonical_output).ok()?;
    let line_count = text.lines().count();
    if line_count == 0 || line_count > MAX_DIAGNOSTIC_LINES {
        return None;
    }
    Some(ToolOutputProjectionRange {
        id: id.to_string(),
        start_line: 1,
        end_line: line_count,
    })
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
    let inspection_command = is_known_safe_command(&args.safety_command);
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
            args.turn_environment.environment.approval_scope_id(),
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

    let repository_root = resolve_repository_root(args.exec_params.cwd.as_path());
    let call_id = args.call_id.clone();
    let cancellation_token = args.cancellation_token.clone();
    let retained_output_ref = format!("tool-call:{}:{call_id}", args.session.thread_id);
    let is_validation = args.validation_launch.is_some();
    let focused_validation = if is_validation {
        let command_summary = args.hook_command.clone();
        match coordinator
            .begin_focused_validation_for_source_with_evidence(
                &session_source,
                call_id.clone(),
                command_summary,
                ValidationEvidence {
                    retained_output_ref: Some(retained_output_ref.clone()),
                    ..ValidationEvidence::default()
                },
            )
            .await
        {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(%error, %call_id, "focused validation start evidence could not be persisted");
                None
            }
        }
    } else {
        None
    };

    let mut terminal_validation_result = None;
    let operation = run_exec_like_with_exit_code_inner(
        args,
        is_validation,
        inspection_command,
        repository_root,
        &mut terminal_validation_result,
    );
    let result = if let Some(token) = focused_validation.as_ref() {
        run_with_focused_validation_heartbeat(&coordinator, token, &call_id, operation).await
    } else {
        operation.await
    };

    if let Some(token) = focused_validation {
        let status = focused_validation_status(
            &result,
            cancellation_token.is_cancelled(),
            terminal_validation_result.is_some(),
        );
        let output_summary = validation_output_summary(&result);
        let structured_validation_result =
            terminal_validation_result.and_then(|result| serde_json::to_value(result).ok());
        if let Err(error) = coordinator
            .finish_focused_validation_with_result(
                token,
                status,
                Some(retained_output_ref),
                output_summary,
                structured_validation_result,
            )
            .await
        {
            tracing::warn!(%error, %call_id, "focused validation result evidence could not be persisted");
        }
    }

    result
}

async fn run_with_focused_validation_heartbeat<F>(
    coordinator: &AgentTaskCoordinator,
    token: &FocusedValidationToken,
    call_id: &str,
    operation: F,
) -> F::Output
where
    F: Future,
{
    run_with_periodic_heartbeat(operation, FOCUSED_VALIDATION_HEARTBEAT_INTERVAL, || async {
        match coordinator.heartbeat_focused_validation(token).await {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                %call_id,
                "focused validation heartbeat was rejected"
            ),
            Err(error) => tracing::warn!(
                %error,
                %call_id,
                "focused validation heartbeat could not be persisted"
            ),
        }
    })
    .await
}

async fn run_with_periodic_heartbeat<F, H, HFut>(
    operation: F,
    heartbeat_interval: Duration,
    mut heartbeat: H,
) -> F::Output
where
    F: Future,
    H: FnMut() -> HFut,
    HFut: Future<Output = ()>,
{
    tokio::pin!(operation);
    let mut ticker = tokio::time::interval(heartbeat_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        tokio::select! {
            result = &mut operation => return result,
            _ = ticker.tick() => heartbeat().await,
        }
    }
}

pub(in crate::tools::handlers) fn validation_environment_hash(
    env: &HashMap<String, String>,
) -> String {
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

/// Enforces the repository boundary shared by planned and directly tagged
/// validation commands. Runner flags are left to the ordinary command preflight.
#[cfg(test)]
pub(crate) fn validate_structured_validation_leaf(
    leaf: &ValidationRouteLeaf,
    repo_root: &Path,
) -> Result<String, String> {
    let leaf = normalize_structured_validation_leaf(leaf.clone(), repo_root)?;
    let program = &leaf.argv[0];
    let args = &leaf.argv[1..];
    let invocation = CommandInvocation::Argv {
        program: program.clone(),
        args: args.to_vec(),
    };
    Ok(invocation.display_command())
}

pub(crate) fn normalize_structured_validation_leaf(
    mut leaf: ValidationRouteLeaf,
    repo_root: &Path,
) -> Result<ValidationRouteLeaf, String> {
    if leaf.argv.is_empty() || leaf.argv.iter().any(|arg| arg.trim().is_empty()) {
        return Err("validation argv must contain non-empty direct arguments".to_string());
    }
    leaf.covered_paths = normalize_covered_paths(&leaf.covered_paths, repo_root)?;
    if leaf.timeout_ms == 0 || leaf.timeout_ms > MAX_VALIDATION_TIMEOUT_MS {
        return Err(format!(
            "validation timeout_ms must be between 1 and {MAX_VALIDATION_TIMEOUT_MS}"
        ));
    }
    Ok(leaf)
}

pub(crate) fn normalize_structured_validation_route(
    route: &mut codex_protocol::plan_tool::ValidationRoute,
    repo_root: &Path,
) -> Result<(), String> {
    for leaf in &mut route.leaves {
        *leaf = normalize_structured_validation_leaf(leaf.clone(), repo_root)?;
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct DirectValidationRoute {
    leaf: ValidationRouteLeaf,
    route: codex_protocol::plan_tool::ValidationRoute,
}

impl DirectValidationRoute {
    pub(crate) fn leaf(&self) -> &ValidationRouteLeaf {
        &self.leaf
    }

    pub(crate) fn route(&self) -> &codex_protocol::plan_tool::ValidationRoute {
        &self.route
    }
}

pub(crate) fn direct_validation_route(
    context: &codex_protocol::validation::ValidationCommandContext,
    invocation: &CommandInvocation,
    repo_root: &Path,
    timeout_ms: u64,
) -> Result<DirectValidationRoute, String> {
    let CommandInvocation::Argv { program, args } = invocation else {
        return Err(
            "validation commands must use direct argv mode and provide covered_paths".to_string(),
        );
    };
    if timeout_ms == 0 || timeout_ms > MAX_VALIDATION_TIMEOUT_MS {
        return Err(format!(
            "validation timeout_ms must be between 1 and {MAX_VALIDATION_TIMEOUT_MS}"
        ));
    }
    let leaf = normalize_structured_validation_leaf(
        ValidationRouteLeaf {
            argv: std::iter::once(program.clone())
                .chain(args.iter().cloned())
                .collect(),
            covered_paths: context.covered_paths.clone(),
            timeout_ms,
        },
        repo_root,
    )?;
    let route = codex_protocol::plan_tool::ValidationRoute {
        leaves: vec![leaf.clone()],
        ordering: codex_protocol::plan_tool::ValidationRouteOrdering::StopOnFailure,
    };
    Ok(DirectValidationRoute { leaf, route })
}

pub(crate) fn normalize_covered_paths(
    covered_paths: &[String],
    repo_root: &Path,
) -> Result<Vec<String>, String> {
    #[cfg(test)]
    VALIDATION_PATH_NORMALIZATION_COUNT.with(|count| count.set(count.get() + 1));
    if covered_paths.is_empty() {
        return Err("validation must declare non-empty covered_paths".to_string());
    }
    #[cfg(test)]
    VALIDATION_ROOT_CANONICALIZATION_COUNT.with(|count| count.set(count.get() + 1));
    let canonical_root = std::fs::canonicalize(repo_root)
        .map_err(|error| format!("repository root could not be canonicalized: {error}"))?;
    let mut normalized = covered_paths
        .iter()
        .map(|path| normalize_repo_relative_path(path, "validation covered path", &canonical_root))
        .collect::<Result<Vec<_>, _>>()?;
    normalized.sort_by_key(|path| {
        if cfg!(windows) {
            path.to_ascii_lowercase()
        } else {
            path.clone()
        }
    });
    normalized.dedup_by(|left, right| {
        if cfg!(windows) {
            left.eq_ignore_ascii_case(right)
        } else {
            left == right
        }
    });
    Ok(normalized)
}

fn normalize_repo_relative_path(
    value: &str,
    label: &str,
    repo_root: &Path,
) -> Result<String, String> {
    let value = value.trim();
    if !safe_repo_relative_path(value) {
        return Err(format!("{label} must stay within the repository: {value}"));
    }

    let canonical_root = repo_root;
    let candidate = canonical_root.join(value);
    let mut existing_ancestor = candidate.as_path();
    let mut missing_suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(existing_ancestor) {
            Ok(_) => {
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = existing_ancestor.file_name().ok_or_else(|| {
                    format!("{label} has no inspectable repository ancestor: {value}")
                })?;
                missing_suffix.push(component.to_os_string());
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
    let canonical_candidate = std::fs::canonicalize(existing_ancestor)
        .map_err(|error| format!("{label} could not be canonicalized safely ({value}): {error}"))?;
    if !path_has_component_prefix(&canonical_candidate, canonical_root) {
        return Err(format!("{label} resolves outside the repository: {value}"));
    }

    let root_component_count = canonical_root.components().count();
    let mut relative_components = canonical_candidate
        .components()
        .skip(root_component_count)
        .filter_map(|component| match component {
            std::path::Component::Normal(component) => Some(component.to_os_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    relative_components.extend(missing_suffix.into_iter().rev());
    if relative_components.is_empty() {
        return Ok(".".to_string());
    }
    Ok(relative_components
        .iter()
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn path_has_component_prefix(path: &Path, prefix: &Path) -> bool {
    let mut path_components = path.components();
    prefix.components().all(|expected| {
        let Some(actual) = path_components.next() else {
            return false;
        };
        if cfg!(windows) {
            actual
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy())
        } else {
            actual == expected
        }
    })
}

async fn finish_validation_skip_after_begin(
    emitter: &ToolEmitter,
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    call_id: &str,
    event_tracker: Option<&SharedTurnDiffTracker>,
    skipped: ValidationSkippedToolOutput,
) -> Result<RunExecLikeResult, FunctionCallError> {
    let skip_disposition = skipped.skip_disposition;
    if matches!(
        skip_disposition,
        codex_tools::ToolOutputSkipDisposition::Suppressed
    ) {
        turn.turn_timing_state.record_suppressed_validation_output();
    }
    let value = serde_json::to_value(&skipped).unwrap_or_default();
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), call_id, event_tracker);
    let content = emitter
        .finish(event_ctx, Err(ToolError::ValidationSkipped(skipped)), None)
        .await?;
    let mut output =
        FunctionToolOutput::from_text(content, None).with_skip_disposition(skip_disposition);
    output.post_tool_use_response = Some(value);
    Ok(RunExecLikeResult {
        output,
        exit_code: None,
        validation_execution_outcome: ValidationExecutionOutcome::NotExecuted,
        canonical_output: None,
    })
}

fn unexecuted_validation_skip(
    out: &Result<ExecToolCallOutput, ToolError>,
    validation_attempt_started: bool,
) -> Option<&ValidationSkippedToolOutput> {
    if validation_attempt_started {
        return None;
    }
    match out {
        Err(ToolError::ValidationSkipped(skipped)) => Some(skipped),
        Ok(_) | Err(_) => None,
    }
}

fn restore_retained_validation_attempt(
    out: Result<ExecToolCallOutput, ToolError>,
    retained_validation_attempt: Option<&ExecToolCallOutput>,
) -> Result<ExecToolCallOutput, ToolError> {
    match (&out, retained_validation_attempt) {
        (
            Err(ToolError::Denied(_) | ToolError::ValidationSkipped(_)),
            Some(retained_validation_attempt),
        ) => Ok(retained_validation_attempt.clone()),
        _ => out,
    }
}

fn record_retained_validation_skip(
    turn_timing_state: &crate::turn_timing::TurnTimingState,
    out: &Result<ExecToolCallOutput, ToolError>,
    retained_validation_attempt: Option<&ExecToolCallOutput>,
) {
    if retained_validation_attempt.is_some()
        && let Err(ToolError::ValidationSkipped(skipped)) = out
        && matches!(
            skipped.skip_disposition,
            codex_tools::ToolOutputSkipDisposition::Suppressed
        )
    {
        turn_timing_state.record_suppressed_validation_output();
    }
}

pub(crate) fn validation_repository_root(
    effective_cwd: &Path,
    repository_anchor: &Path,
) -> Option<std::path::PathBuf> {
    #[cfg(test)]
    VALIDATION_REPOSITORY_DISCOVERY_COUNT.with(|count| count.set(count.get() + 1));
    let canonical_anchor = std::fs::canonicalize(repository_anchor).ok()?;
    let canonical_root = std::fs::canonicalize(codex_git_utils::get_git_repo_root(
        canonical_anchor.as_path(),
    )?)
    .ok()?;
    let canonical_cwd = std::fs::canonicalize(effective_cwd).ok()?;
    path_has_component_prefix(&canonical_cwd, &canonical_root).then_some(canonical_root)
}

pub(crate) fn validation_repository_root_if_needed(
    validation_requested: bool,
    effective_cwd: &Path,
    repository_anchor: &Path,
) -> Option<std::path::PathBuf> {
    validation_requested
        .then(|| validation_repository_root(effective_cwd, repository_anchor))
        .flatten()
}

pub(crate) fn workspace_operation_root_if_needed(
    focused_validation: bool,
    inspection_command: bool,
    repository_root: std::path::PathBuf,
) -> Option<std::path::PathBuf> {
    (focused_validation || !inspection_command).then_some(repository_root)
}

#[cfg(test)]
thread_local! {
    static VALIDATION_PATH_NORMALIZATION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static VALIDATION_ROOT_CANONICALIZATION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static VALIDATION_REPOSITORY_DISCOVERY_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_validation_path_normalization_count() {
    VALIDATION_PATH_NORMALIZATION_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn validation_path_normalization_count() -> usize {
    VALIDATION_PATH_NORMALIZATION_COUNT.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_validation_root_canonicalization_count() {
    VALIDATION_ROOT_CANONICALIZATION_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn validation_root_canonicalization_count() -> usize {
    VALIDATION_ROOT_CANONICALIZATION_COUNT.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_validation_repository_discovery_count() {
    VALIDATION_REPOSITORY_DISCOVERY_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn validation_repository_discovery_count() -> usize {
    VALIDATION_REPOSITORY_DISCOVERY_COUNT.with(std::cell::Cell::get)
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

async fn run_exec_like_with_exit_code_inner(
    args: RunExecLikeArgs,
    focused_validation: bool,
    inspection_command: bool,
    repository_root: std::path::PathBuf,
    terminal_validation_result: &mut Option<ValidationResult>,
) -> Result<RunExecLikeResult, FunctionCallError> {
    let RunExecLikeArgs {
        tool_name,
        exec_params,
        stall_timeout_ms,
        cancellation_token,
        hook_command,
        safety_command,
        shell_type,
        shell_wrapper_is_owned,
        is_powershell_script,
        additional_permissions,
        prefix_rule,
        session,
        turn,
        turn_environment,
        tracker,
        call_id,
        track_command_mutations,
        attempt_key,
        repair_notice,
        command_repaired,
        force_fresh,
        validation_launch,
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
        turn_environment.environment.approval_scope_id(),
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

    let known_delta = if turn.config.features.enabled(Feature::KnownDeltaStore)
        && !focused_validation
        && !exec_params.command.is_empty()
        && known_delta_store::is_immutable_git_show_candidate(
            &exec_params.command[0],
            &exec_params.command[1..],
        )
        && let Some(authorization_scope) = known_delta_store::authorization_scope_fingerprint(
            &turn.file_system_sandbox_context(
                normalized_additional_permissions.clone(),
                &PathUri::from_abs_path(&exec_params.cwd),
            ),
            effective_additional_permissions.sandbox_permissions,
        ) {
        let metadata_source = turn
            .turn_metadata_state
            .git_metadata_source()
            .filter(|source| exec_params.cwd.starts_with(source.repo_root().as_path()));
        let project_namespace = match &metadata_source {
            Some(source) => source.project_namespace().await,
            None => None,
        };
        let project_namespace_hint = metadata_source
            .map_or(known_delta_store::ProjectNamespaceHint::Discover, |_| {
                known_delta_store::ProjectNamespaceHint::Resolved(project_namespace.as_deref())
            });
        known_delta_store::prepare_immutable_git_show_with_authorization_scope(
            turn.config.codex_home.as_path(),
            &session.thread_id.to_string(),
            &exec_params.cwd,
            &exec_params.command[0],
            &exec_params.command[1..],
            project_namespace_hint,
            &authorization_scope,
            force_fresh,
        )
        .await
    } else {
        None
    };
    let known_delta_hit = known_delta
        .as_ref()
        .is_some_and(known_delta_store::PreparedKnownDelta::is_hit);

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

    if !known_delta_hit && let Some(attempt_key) = attempt_key.as_ref() {
        session
            .services
            .command_execution
            .begin_attempt_with_freshness(attempt_key, command_repaired, force_fresh)
            .await
            .map_err(|blocked| FunctionCallError::RespondToModel(blocked.render_for_model()))?;
    }

    // Intercept apply_patch if present.
    let apply_patch_cwd = PathUri::from_abs_path(&exec_params.cwd);
    let intercepted = intercept_apply_patch(
        validation_launch.is_some(),
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
            if !known_delta_hit && let Some(attempt_key) = attempt_key.as_ref() {
                err.record_attempt_failure(&session.services.command_execution, attempt_key)
                    .await;
            }
            return Err(err.into_error());
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
            validation_execution_outcome: ValidationExecutionOutcome::ExecutedSuccess,
            canonical_output: None,
        });
    }

    let source = ExecCommandSource::Agent;
    let emitter = crate::tools::events::ToolEmitter::shell(
        safety_command.clone(),
        exec_params.cwd.clone(),
        source,
        turn_environment.environment_id.clone(),
    )
    .with_model_command_text(hook_command.clone());
    let event_tracker = track_command_mutations.then_some(&tracker);
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, event_tracker);
    emitter.begin(event_ctx).await;

    // This is a preliminary resolution used only for the policy compatibility check. The runtime
    // re-proves the inspectable safety projection against its final cwd and child environment
    // immediately before constructing the sandbox request.

    let proven_direct_argv = if is_powershell_script && !turn_environment.environment.is_remote() {
        prove_noprofile_powershell_direct_argv_async(
            &safety_command,
            exec_params.cwd.as_path(),
            &exec_params.env,
        )
        .await
    } else {
        None
    };

    let canonical_exec_approval_requirement = if let Some(proof) = proven_direct_argv.as_ref() {
        Some(
            session
                .services
                .exec_policy
                .create_exec_approval_requirement_for_command(ExecApprovalRequest {
                    command: proof.command_for_policy(),
                    command_for_safety: None,
                    approval_policy: turn.approval_policy.value(),
                    permission_profile: turn.permission_profile(),
                    windows_sandbox_level: turn.windows_sandbox_level,
                    sandbox_permissions: if effective_additional_permissions.permissions_preapproved
                    {
                        codex_protocol::models::SandboxPermissions::UseDefault
                    } else {
                        effective_additional_permissions.sandbox_permissions
                    },
                    prefix_rule: None,
                })
                .await,
        )
    } else {
        None
    };

    let exec_approval_request = ExecApprovalRequest {
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
    };
    let exec_approval_requirement = if shell_wrapper_is_owned {
        session
            .services
            .exec_policy
            .create_exec_approval_requirement_for_command(exec_approval_request)
            .await
    } else {
        session
            .services
            .exec_policy
            .create_exec_approval_requirement_for_direct_argv(exec_approval_request)
            .await
    };

    let approved_powershell_direct_argv = if let (Some(proof), Some(canonical_requirement)) =
        (proven_direct_argv, canonical_exec_approval_requirement)
        && same_exec_authorization_envelope(&exec_approval_requirement, &canonical_requirement)
        && let Some(command) = proof.into_command_for_state(
            &safety_command,
            exec_params.cwd.as_path(),
            &exec_params.env,
        ) {
        Some(command)
    } else {
        None
    };

    let workspace_operation_root =
        workspace_operation_root_if_needed(focused_validation, inspection_command, repository_root);
    let req = ShellRequest {
        command: exec_params.command.clone(),
        command_for_approval: safety_command,

        approved_powershell_direct_argv,
        turn_environment: turn_environment.clone(),
        shell_type,
        hook_command,
        cwd: exec_params.cwd.clone(),
        timeout_ms: exec_params.expiration.timeout_ms(),
        stall_timeout_ms,
        cancellation_token,
        env: exec_params.env.clone(),
        explicit_env_overrides,
        network: exec_params.network.clone(),
        sandbox_permissions: effective_additional_permissions.sandbox_permissions,
        additional_permissions: normalized_additional_permissions,
        justification: exec_params.justification.clone(),
        exec_approval_requirement,
        known_delta: known_delta.clone(),
        validation_launch,
        workspace_operation_root,
    };
    let mut orchestrator = ToolOrchestrator::new();
    let mut runtime = ShellRuntime::for_shell_command();
    let validation_started_at = tokio::time::Instant::now();
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
    let retained_validation_attempt = runtime.take_last_validation_attempt_output();
    let validation_attempt_started =
        retained_validation_attempt.is_some() || runtime.take_last_validation_attempt_started();
    if let Some(skipped) = unexecuted_validation_skip(&out, validation_attempt_started) {
        return finish_validation_skip_after_begin(
            &emitter,
            &session,
            &turn,
            &call_id,
            event_tracker,
            skipped.clone(),
        )
        .await;
    }
    record_retained_validation_skip(
        turn.turn_timing_state.as_ref(),
        &out,
        retained_validation_attempt.as_ref(),
    );
    let out = restore_retained_validation_attempt(out, retained_validation_attempt.as_ref());
    if !known_delta_hit && let Some(known_delta) = known_delta.as_ref() {
        let observation = match &out {
            Ok(output) if is_complete_success(output) => {
                KnownDeltaExecutionObservation::CompleteSuccess {
                    output: output.aggregated_output.text.as_bytes(),
                    executor_cost: output.duration,
                }
            }
            Ok(output)
                if output.aggregated_output.truncated_after_lines.is_some()
                    || output.aggregated_output.truncated =>
            {
                KnownDeltaExecutionObservation::Incomplete
            }
            Err(_) | Ok(_) => KnownDeltaExecutionObservation::CompleteFailure,
        };
        known_delta_store::record_execution(
            turn.config.codex_home.as_path(),
            known_delta,
            observation,
        )
        .await;
    }
    let source_capture_truncated = match &out {
        Ok(output) => output.aggregated_output.truncated,
        Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Timeout { output })))
        | Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied { output, .. }))) => {
            output.aggregated_output.truncated
        }
        Err(_) => false,
    };
    let exit_code = out.as_ref().ok().map(|output| output.exit_code);
    let retry_exit_code = retry_exit_code(&out);
    if !known_delta_hit
        && let (Some(attempt_key), Some(retry_exit_code)) = (attempt_key.as_ref(), retry_exit_code)
    {
        session
            .services
            .command_execution
            .record_exit(attempt_key, retry_exit_code)
            .await;
    }
    let execution_output = shell_validation_execution_output(&out, None);
    let model_projection = execution_output.map(|output| {
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
    let advisory = execution_output.and_then(|output| {
        powershell_script_failure_advisory(
            shell_type,
            Some(output.exit_code),
            is_powershell_script,
            &output.aggregated_output.text,
        )
    });
    let raw_output_artifact = if !known_delta_hit
        && let (Some(_attempt_key), Some(output)) = (&attempt_key, execution_output)
    {
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
    let validation_attempt_output =
        shell_validation_execution_output(&out, retained_validation_attempt.as_ref());
    let completed_validation = if let Some(launch) = req.validation_launch.as_ref()
        && shell_validation_was_executed(&out, validation_attempt_started)
    {
        let validation_exit_code = validation_attempt_output.map(|output| output.exit_code);
        let timed_out = validation_attempt_output.is_some_and(|output| output.timed_out);
        session
            .services
            .command_execution
            .complete_inline_validation(
                launch,
                raw_output_artifact.clone(),
                validation_started_at,
                validation_exit_code,
                timed_out,
                None,
            )
            .await
    } else {
        None
    };
    *terminal_validation_result = completed_validation
        .as_ref()
        .map(|completed| completed.result.clone());
    let canonical_output = canonical_exec_output_bytes(&out);
    let output_bearing_result = shell_result_has_execution_output(&out);
    let tool_outcome = shell_tool_outcome(&out);
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, event_tracker)
        .with_completed_validation(completed_validation.as_ref());
    let finish_result = emitter
        .finish(event_ctx, out, /*applied_patch_delta*/ None)
        .await;
    let observed_mutation_revision = tracker.lock().await.current_mutation_revision();
    session
        .services
        .command_execution
        .observe_repository_revision(&turn.sub_id, observed_mutation_revision)
        .await;
    let mut content = recover_output_bearing_shell_content(finish_result, output_bearing_result)?;
    if let Some(advisory) = advisory {
        content.push_str("\n\n");
        content.push_str(advisory);
    }
    if let Some(repair_notice) = repair_notice {
        content.push_str("\n\n");
        content.push_str(&repair_notice);
    }
    if let Some(raw_output_artifact) = raw_output_artifact {
        insert_metadata_before_output(
            &mut content,
            &raw_output_artifact.render_for_model_with_source_truncation(source_capture_truncated),
        );
        if model_projection.is_some_and(|projection| projection.reduced)
            && let Some(notice) = raw_output_artifact.reduction_notice()
        {
            content.push('\n');
            content.push_str(&notice);
        }
    }
    if source_capture_truncated {
        content.push_str(
            "\n\n[output capture truncated at execution retained-byte limit; omitted bytes are unavailable]",
        );
    }
    let validation_execution_outcome = match exit_code {
        Some(0) => ValidationExecutionOutcome::ExecutedSuccess,
        Some(_) | None => ValidationExecutionOutcome::ExecutedFailure,
    };
    let output = FunctionToolOutput {
        body: vec![
            codex_protocol::models::FunctionCallOutputContentItem::InputText { text: content },
        ],
        success: Some(tool_outcome == codex_tools::ToolOutputOutcome::Success),
        outcome: Some(tool_outcome),
        post_tool_use_response,
        sampling_request_signal: shell_sampling_signal(
            attempt_key.as_ref(),
            req.hook_command.as_str(),
            exit_code,
            canonical_output.as_deref(),
        ),
        deterministic_continuation_receipts: Vec::new(),
        deterministic_continuation_owner_key: None,
        skip_disposition: None,
    };
    Ok(RunExecLikeResult {
        output,
        exit_code,
        validation_execution_outcome,
        canonical_output,
    })
}

fn recover_output_bearing_shell_content(
    finish_result: Result<String, FunctionCallError>,
    output_bearing_result: bool,
) -> Result<String, FunctionCallError> {
    match finish_result {
        Err(FunctionCallError::RespondToModel(content)) if output_bearing_result => Ok(content),
        other => other,
    }
}

fn shell_result_has_execution_output(out: &Result<ExecToolCallOutput, ToolError>) -> bool {
    matches!(
        out,
        Ok(_)
            | Err(ToolError::Codex(CodexErr::Sandbox(
                SandboxErr::Timeout { .. }
            )))
            | Err(ToolError::Codex(CodexErr::Sandbox(
                SandboxErr::Denied { .. }
            )))
    )
}

fn shell_tool_outcome(
    out: &Result<ExecToolCallOutput, ToolError>,
) -> codex_tools::ToolOutputOutcome {
    match out {
        Ok(output) if output.exit_code == 0 => codex_tools::ToolOutputOutcome::Success,
        Ok(_) | Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied { .. }))) => {
            codex_tools::ToolOutputOutcome::Failure
        }
        Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Timeout { .. }))) => {
            codex_tools::ToolOutputOutcome::TimedOut
        }
        Err(ToolError::ValidationSkipped(_)) => codex_tools::ToolOutputOutcome::Skipped,
        Err(_) => codex_tools::ToolOutputOutcome::Failure,
    }
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
        Err(ToolError::ValidationSkipped(_)) => None,
    }
}

fn shell_validation_was_executed(
    out: &Result<ExecToolCallOutput, ToolError>,
    retained_validation_attempt: bool,
) -> bool {
    retained_validation_attempt || shell_validation_execution_output(out, None).is_some()
}

fn shell_validation_execution_output<'a>(
    out: &'a Result<ExecToolCallOutput, ToolError>,
    retained_validation_attempt: Option<&'a ExecToolCallOutput>,
) -> Option<&'a ExecToolCallOutput> {
    match out {
        Ok(output) => Some(output),
        Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Timeout { output })))
        | Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied { output, .. }))) => {
            Some(output.as_ref())
        }
        Err(_) => retained_validation_attempt,
    }
}

fn canonical_exec_output_bytes(out: &Result<ExecToolCallOutput, ToolError>) -> Option<Vec<u8>> {
    match out {
        Ok(output) => Some(output.aggregated_output.text.as_bytes().to_vec()),
        Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Timeout { output })))
        | Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied { output, .. }))) => {
            Some(output.aggregated_output.text.as_bytes().to_vec())
        }
        Err(_) => None,
    }
}

fn is_complete_success(output: &ExecToolCallOutput) -> bool {
    output.exit_code == 0
        && !output.timed_out
        && output.aggregated_output.truncated_after_lines.is_none()
        && !output.aggregated_output.truncated
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
