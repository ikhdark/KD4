use chrono::Utc;
use codex_agent_task_store::ValidationCallStatus;
use codex_agent_task_store::ValidationEvidence;
use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::validation::ValidationTerminalStatus;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use crate::agent::task_capabilities::validate_independent_review_shell;
use crate::exec::ExecExpiration;
use crate::exec::ExecParams;
use crate::exec_policy::ExecApprovalRequest;
use crate::function_tool::FunctionCallError;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellType;
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::command_output_artifact::create_raw_output_artifact;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolOutput;
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
use crate::tools::known_delta_store;
use crate::tools::known_delta_store::KnownDeltaExecutionObservation;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::runtimes::shell::ShellRequest;
use crate::tools::runtimes::shell::ShellRuntime;
use crate::tools::runtimes::shell::ShellRuntimeBackend;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
#[cfg(any(windows, test))]
use crate::tools::sandboxing::same_exec_authorization_envelope;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::plan_tool::ValidationRouteLeaf;
use codex_protocol::protocol::ExecCommandSource;
use codex_shell_command::is_safe_command::is_known_safe_command;
#[cfg(windows)]
use codex_shell_command::powershell::prove_noprofile_powershell_command_as_direct_argv;
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

const MAX_FOCUSED_VALIDATION_TIMEOUT_MS: u64 = 60 * 60 * 1_000;

#[derive(Debug, Serialize)]
struct FocusedValidationViolation {
    code: &'static str,
    message: String,
    constraint: JsonValue,
}

#[derive(Debug, Serialize)]
struct FocusedValidationCapabilityDenied {
    kind: &'static str,
    violations: Vec<FocusedValidationViolation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical_permitted_command: Option<String>,
}

impl FocusedValidationCapabilityDenied {
    fn render(self) -> String {
        match serde_json::to_string(&self) {
            Ok(encoded) => format!("FocusedValidationCapabilityDenied: {encoded}"),
            Err(error) => format!(
                "FocusedValidationCapabilityDenied: failed to encode structured denial: {error}"
            ),
        }
    }
}

fn focused_validation_violation(
    code: &'static str,
    message: impl Into<String>,
    constraint: JsonValue,
) -> FocusedValidationViolation {
    FocusedValidationViolation {
        code,
        message: message.into(),
        constraint,
    }
}

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
    pub(super) force_fresh: bool,
    pub(super) validation_launch: Option<crate::validation_admission::ValidationLaunchPlan>,
    pub(super) validation_leader: Option<crate::validation_admission::ValidationLeaderOwnership>,
    pub(super) validation_waiter: Option<crate::validation_admission::ValidationLeader>,
    pub(super) repository: PathBuf,
}

pub(super) struct RunExecLikeResult {
    pub(super) output: FunctionToolOutput,
    pub(super) exit_code: Option<i32>,
    pub(super) validation_execution_outcome: ValidationExecutionOutcome,
    pub(super) canonical_output: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ValidationExecutionOutcome {
    ExecutedSuccess,
    ExecutedFailure,
    NotExecuted,
}

impl ValidationExecutionOutcome {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::ExecutedSuccess => "executed_success",
            Self::ExecutedFailure => "executed_failure",
            Self::NotExecuted => "not_executed",
        }
    }

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

    pub(super) fn from_value_or_legacy_success(value: &serde_json::Value) -> Self {
        Self::from_value(value).unwrap_or_else(|| {
            if value.get("success").and_then(serde_json::Value::as_bool) == Some(false) {
                Self::ExecutedFailure
            } else {
                Self::ExecutedSuccess
            }
        })
    }

    pub(super) fn tool_outcome(self) -> codex_tools::ToolOutputOutcome {
        match self {
            Self::ExecutedSuccess => codex_tools::ToolOutputOutcome::Success,
            Self::ExecutedFailure => codex_tools::ToolOutputOutcome::Failure,
            Self::NotExecuted => codex_tools::ToolOutputOutcome::Skipped,
        }
    }
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
        metadata.fragments.insert(
            0,
            ToolOutputProjectionFragment::new(
                ToolOutputProjectionFragmentKind::ProcessFinalStatus,
                format!("process final status: exit_code={:?}", self.exit_code),
            ),
        );
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

/// RAII ownership for one admitted bounded operation.
///
/// The durable validation call owns cross-turn state; this guard owns only the local cancellation
/// handle and hard deadline. Dropping it before a terminal durable update invokes the command's
/// existing cancellation path and never extends the underlying timeout.
struct OwnedOperationLease {
    call_id: String,
    cancellation: CancellationToken,
    hard_deadline: chrono::DateTime<Utc>,
    progress_revision: u64,
    terminal: bool,
}

impl OwnedOperationLease {
    fn new(
        call_id: String,
        cancellation: CancellationToken,
        hard_deadline: chrono::DateTime<Utc>,
    ) -> Self {
        Self {
            call_id,
            cancellation,
            hard_deadline,
            progress_revision: 0,
            terminal: false,
        }
    }

    fn record_progress(&mut self) -> bool {
        if Utc::now() > self.hard_deadline {
            return false;
        }
        self.progress_revision = self.progress_revision.saturating_add(1);
        true
    }

    fn complete(&mut self) {
        let _ = (&self.call_id, self.progress_revision);
        self.terminal = true;
    }
}

impl Drop for OwnedOperationLease {
    fn drop(&mut self) {
        if !self.terminal {
            self.cancellation.cancel();
        }
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
        && result.validation_execution_outcome() == ValidationExecutionOutcome::ExecutedFailure;
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

fn operation_lease_deadline(hard_deadline: Option<chrono::DateTime<Utc>>) -> chrono::DateTime<Utc> {
    hard_deadline.unwrap_or_else(|| {
        Utc::now()
            + chrono::Duration::seconds(codex_agent_task_store::DEFAULT_WORKSPACE_LEASE_SECONDS)
    })
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
    let validation_session = args.session.clone();
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
    let focused_validation_admission = (!inspection_command).then(|| {
        focused_validation_command_summary(
            &args.safety_command,
            &args.hook_command,
            args.shell_type.is_none() && !args.is_powershell_script,
            args.exec_params.cwd.as_path(),
            args.repository.as_path(),
            &args.exec_params.expiration,
            args.exec_params
                .sandbox_permissions
                .requests_sandbox_override(),
            args.additional_permissions.is_some(),
            args.prefix_rule.is_some(),
        )
    });
    let focused_validation_command =
        focused_validation_admission
            .and_then(Result::ok)
            .and_then(|command_summary| {
                pin_focused_validation_executable(
                    &mut args.exec_params.command,
                    &args.safety_command,
                    &args.exec_params.env,
                    args.exec_params.cwd.as_path(),
                    args.repository.as_path(),
                )
                .ok()
                .map(|resolved_executable| (command_summary, resolved_executable))
            });
    let call_id = args.call_id.clone();
    let retained_output_ref = format!("tool-call:{}:{call_id}", args.session.thread_id);
    let operation_hard_deadline = match &args.exec_params.expiration {
        crate::exec::ExecExpiration::Timeout(timeout) => chrono::Duration::from_std(*timeout)
            .ok()
            .map(|timeout| Utc::now() + timeout),
        _ => None,
    };
    let mut owned_operation = None;
    let mut focused_validation = if let Some((command_summary, resolved_executable)) =
        focused_validation_command
    {
        let lease_expires_at = operation_lease_deadline(operation_hard_deadline);
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
        owned_operation = operation_hard_deadline.map(|hard_deadline| {
            OwnedOperationLease::new(
                call_id.clone(),
                args.cancellation_token.clone(),
                hard_deadline,
            )
        });
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
                let deadline = leader
                    .evidence
                    .lease_expires_at
                    .or_else(|| token.lease_expires_at())
                    .unwrap_or_else(|| operation_lease_deadline(operation_hard_deadline));
                if deadline <= Utc::now() {
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
                        Ok(()) => coordinator.notify_validation_call(&leader_call_id),
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
                match coordinator
                    .wait_for_validation_call_terminal(
                        &leader_call_id,
                        &args.cancellation_token,
                        deadline,
                    )
                    .await
                    .map_err(|error| {
                        FunctionCallError::RespondToModel(format!(
                            "shared validation leader could not be awaited: {error}"
                        ))
                    })? {
                    Some(settled) if settled.status.is_terminal() => break settled,
                    Some(_) | None => continue,
                }
            };
            let status = leader.status;
            let leader_output_ref = leader.evidence.retained_output_ref.clone();
            let leader_output_summary = leader.evidence.output_summary.clone();
            let leader_validation_result = leader.evidence.validation_result.clone();
            coordinator
                .finish_focused_validation_with_result(
                    token,
                    status,
                    leader_output_ref.clone(),
                    leader_output_summary.clone(),
                    leader_validation_result,
                )
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "shared validation result could not be persisted: {error}"
                    ))
                })?;
            if let Some(operation) = owned_operation.as_mut() {
                operation.record_progress();
                operation.complete();
            }
            args.turn.session_telemetry.counter(
                "codex.multi_agent.validation_proof",
                1,
                &[(
                    "disposition",
                    if status.is_success() {
                        "fresh_reuse"
                    } else {
                        "shared_non_success"
                    },
                )],
            );
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
                    outcome: Some(if status.is_success() {
                        codex_tools::ToolOutputOutcome::Success
                    } else {
                        codex_tools::ToolOutputOutcome::Failure
                    }),
                    post_tool_use_response: None,
                    sampling_request_signal: None,
                    deterministic_continuation_receipts: Vec::new(),
                    deterministic_continuation_owner_key: None,
                    skip_disposition: None,
                },
                exit_code: Some(if status.is_success() { 0 } else { 1 }),
                validation_execution_outcome: if status.is_success() {
                    ValidationExecutionOutcome::ExecutedSuccess
                } else {
                    ValidationExecutionOutcome::ExecutedFailure
                },
                canonical_output: None,
            });
        }
        Some(token)
    } else {
        None
    };
    if args.validation_leader.is_none()
        && let Some(waiter) = args.validation_waiter.take()
    {
        let shared_from_call_id = waiter.shared_from_call_id().to_string();
        let joined = tokio::select! {
            result = waiter.join() => result,
            _ = args.cancellation_token.cancelled() => {
                return Err(FunctionCallError::RespondToModel(
                    "shared validation wait was cancelled".to_string(),
                ));
            }
        };
        let result = joined.ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "shared validation execution ended without a reusable result".to_string(),
            )
        })?;
        let execution_outcome =
            ValidationExecutionOutcome::from_value_or_legacy_success(&result.value);
        let text = result.value.get("text").cloned().unwrap_or_default();
        let validation_result = result
            .value
            .get("validation_result")
            .cloned()
            .and_then(|value| {
                serde_json::from_value::<codex_protocol::validation::ValidationResult>(value).ok()
            })
            .map(|mut result| {
                result.freshness = codex_protocol::validation::ValidationFreshness::Joined;
                result
            });
        if let Some(token) = focused_validation.take() {
            let status = match execution_outcome {
                ValidationExecutionOutcome::ExecutedSuccess => ValidationCallStatus::Succeeded,
                ValidationExecutionOutcome::ExecutedFailure => ValidationCallStatus::Failed,
                ValidationExecutionOutcome::NotExecuted => ValidationCallStatus::NotExecuted,
            };
            coordinator
                .finish_focused_validation_with_result(
                    token,
                    status,
                    Some(retained_output_ref),
                    None,
                    validation_result
                        .as_ref()
                        .and_then(|result| serde_json::to_value(result).ok()),
                )
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "joined validation result could not be persisted: {error}"
                    ))
                })?;
            if let Some(operation) = owned_operation.as_mut() {
                operation.record_progress();
                operation.complete();
            }
            args.turn.session_telemetry.counter(
                "codex.multi_agent.validation_proof",
                1,
                &[("disposition", "fresh_reuse")],
            );
        }
        return Ok(RunExecLikeResult {
            output: shell_command::validation_structured_output(serde_json::json!({
                "call_id": args.call_id.clone(),
                "admission_disposition": "joined",
                "shared_from_call_id": shared_from_call_id,
                "text": text,
                "success": execution_outcome.success(),
                "execution_outcome": execution_outcome.as_str(),
                "command_was_executed": execution_outcome != ValidationExecutionOutcome::NotExecuted,
                "skip_disposition": result.value.get("skip_disposition").cloned(),
                "validation_result": validation_result,
            })),
            exit_code: match execution_outcome {
                ValidationExecutionOutcome::ExecutedSuccess => Some(0),
                ValidationExecutionOutcome::ExecutedFailure => Some(1),
                ValidationExecutionOutcome::NotExecuted => None,
            },
            validation_execution_outcome: execution_outcome,
            canonical_output: None,
        });
    }
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
                        let lease_expires_at = operation_lease_deadline(operation_hard_deadline);
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
    let cancellation_token = args.cancellation_token.clone();
    if focused_validation.is_some() {
        args.turn.session_telemetry.counter(
            "codex.multi_agent.validation_proof",
            1,
            &[("disposition", "stale_or_unknown_rerun")],
        );
    }
    let validation_leader = args.validation_leader.take();
    let mut result = run_exec_like_with_exit_code_inner(args, focused_validation.is_some()).await;
    if let Some(operation) = owned_operation.as_mut() {
        operation.record_progress();
    }
    let terminal_validation_result = validation_session
        .services
        .command_execution
        .validation_result_for_call(&call_id)
        .await;
    if terminal_validation_result
        .as_ref()
        .is_some_and(|result| result.status == ValidationTerminalStatus::Failed)
        && let Ok(result) = &mut result
    {
        result.validation_execution_outcome = ValidationExecutionOutcome::ExecutedFailure;
        result.output.success = Some(false);
        result.output.outcome = Some(codex_tools::ToolOutputOutcome::Failure);
    }
    if let Some(leader) = validation_leader {
        match &result {
            Ok(result) => {
                let execution_outcome = result.validation_execution_outcome();
                leader
                    .complete(crate::validation_admission::ReusableValidationResult {
                        value: serde_json::json!({
                            "text": result.output.body.iter().filter_map(|item| match item {
                                codex_protocol::models::FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
                                _ => None,
                            }).collect::<Vec<_>>().join("\n"),
                            "success": execution_outcome.success(),
                            "execution_outcome": execution_outcome.as_str(),
                            "command_was_executed": execution_outcome != ValidationExecutionOutcome::NotExecuted,
                            "skip_disposition": result.output.skip_disposition,
                            "validation_result": terminal_validation_result.clone(),
                        }),
                    })
                    .await;
            }
            Err(_) => leader.abandon().await,
        }
    }
    heartbeat_stop.cancel();
    if let Some(heartbeat_task) = heartbeat_task
        && let Err(error) = heartbeat_task.await
    {
        tracing::warn!(%error, "validation heartbeat task failed");
    }
    let Some(token) = focused_validation else {
        return result;
    };
    let status = match (&result, cancellation_token.is_cancelled()) {
        (_, true) => ValidationCallStatus::Cancelled,
        (Ok(result), false) => match result.validation_execution_outcome() {
            ValidationExecutionOutcome::ExecutedSuccess => ValidationCallStatus::Succeeded,
            ValidationExecutionOutcome::ExecutedFailure => ValidationCallStatus::Failed,
            ValidationExecutionOutcome::NotExecuted => ValidationCallStatus::NotExecuted,
        },
        (Err(FunctionCallError::RespondToModel(message)), false)
            if message.contains("rejected by user") =>
        {
            ValidationCallStatus::Cancelled
        }
        (Err(_), false) => ValidationCallStatus::Failed,
    };
    let output_summary = validation_output_summary(&result);
    let structured_validation_result =
        terminal_validation_result.and_then(|result| serde_json::to_value(result).ok());
    let record_result = coordinator
        .finish_focused_validation_with_result(
            token,
            status,
            Some(retained_output_ref),
            output_summary,
            structured_validation_result,
        )
        .await;
    if record_result.is_ok()
        && let Some(operation) = owned_operation.as_mut()
    {
        operation.complete();
    }
    if status == ValidationCallStatus::Cancelled {
        validation_session.services.session_telemetry.counter(
            "codex.multi_agent.bounded_operation",
            1,
            &[("outcome", "cancelled")],
        );
    }
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
pub(in crate::tools::handlers) fn child_env_value<'a>(
    env: &'a HashMap<String, String>,
    name: &str,
) -> Option<&'a std::ffi::OsStr> {
    env.iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| std::ffi::OsStr::new(value))
}

#[cfg(not(windows))]
pub(in crate::tools::handlers) fn child_env_value<'a>(
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
    let mut violations = Vec::new();
    if !direct_argv {
        violations.push(focused_validation_violation(
            "direct_argv_required",
            "the command must use direct argv mode",
            json!({"mode": "direct_argv"}),
        ));
    }
    if cwd != repo_root {
        violations.push(focused_validation_violation(
            "repository_root_cwd_required",
            "the command cwd must be the repository root",
            json!({"cwd": "repository_root"}),
        ));
    }
    match expiration {
        ExecExpiration::Timeout(timeout) => match u64::try_from(timeout.as_millis()) {
            Ok(timeout_ms) if timeout_ms == 0 || timeout_ms > MAX_FOCUSED_VALIDATION_TIMEOUT_MS => {
                violations.push(focused_validation_violation(
                    "bounded_timeout_required",
                    format!("timeout_ms must be between 1 and {MAX_FOCUSED_VALIDATION_TIMEOUT_MS}"),
                    json!({"minimum_ms": 1, "maximum_ms": MAX_FOCUSED_VALIDATION_TIMEOUT_MS}),
                ));
            }
            Ok(_) => {}
            Err(_) => violations.push(focused_validation_violation(
                "bounded_timeout_required",
                "timeout is too large",
                json!({"minimum_ms": 1, "maximum_ms": MAX_FOCUSED_VALIDATION_TIMEOUT_MS}),
            )),
        },
        ExecExpiration::DefaultTimeout => violations.push(focused_validation_violation(
            "explicit_timeout_required",
            "timeout_ms must be supplied explicitly",
            json!({"explicit": true}),
        )),
        ExecExpiration::Cancellation(_) | ExecExpiration::TimeoutOrCancellation { .. } => {
            violations.push(focused_validation_violation(
                "bounded_timeout_required",
                "focused validation requires an explicit bounded timeout",
                json!({"explicit": true, "bounded": true}),
            ));
        }
    }
    if sandbox_override || additional_permissions || prefix_rule {
        violations.push(focused_validation_violation(
            "default_permissions_required",
            "sandbox overrides, additional permissions, and prefix rules are not allowed",
            json!({
                "sandbox_override": false,
                "additional_permissions": false,
                "prefix_rule": false
            }),
        ));
    }
    let Some((program, args)) = command.split_first() else {
        violations.push(focused_validation_violation(
            "nonempty_argv_required",
            "the command argv cannot be empty",
            json!({"minimum_items": 1}),
        ));
        return Err(FocusedValidationCapabilityDenied {
            kind: "focused_validation_capability_denied",
            violations,
            canonical_permitted_command: None,
        }
        .render());
    };
    let argv_violations = collect_focused_validation_argv_violations(program, args, repo_root);
    let argv_is_permitted = argv_violations.is_empty();
    violations.extend(argv_violations);
    let canonical = CommandInvocation::Argv {
        program: program.clone(),
        args: args.to_vec(),
    }
    .display_command();
    if canonical != command_summary {
        violations.push(focused_validation_violation(
            "canonical_summary_required",
            "command summary is not the canonical direct-argv rendering",
            json!({"rendering": "canonical_direct_argv"}),
        ));
    }
    if !violations.is_empty() {
        return Err(FocusedValidationCapabilityDenied {
            kind: "focused_validation_capability_denied",
            violations,
            canonical_permitted_command: (direct_argv && argv_is_permitted).then_some(canonical),
        }
        .render());
    }
    Ok(canonical)
}

/// Re-admits a predeclared plan leaf through the same canonical direct-argv
/// contract used by typed focused validation. No command or coverage inference
/// is performed here.
pub(crate) fn validate_structured_validation_leaf(
    leaf: &ValidationRouteLeaf,
    repo_root: &Path,
) -> Result<String, String> {
    let Some((program, args)) = leaf.argv.split_first() else {
        return Err("the validation route argv cannot be empty".to_string());
    };
    let invocation = CommandInvocation::Argv {
        program: program.clone(),
        args: args.to_vec(),
    };
    let canonical = invocation.display_command();
    focused_validation_command_summary(
        &leaf.argv,
        &canonical,
        true,
        repo_root,
        repo_root,
        &ExecExpiration::Timeout(std::time::Duration::from_millis(leaf.timeout_ms)),
        false,
        false,
        false,
    )?;
    if program == "cargo"
        && args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "--workspace" | "--all" | "--all-targets" | "--all-features"
            )
        })
    {
        return Err("auto-validation cargo routes must remain focused".to_string());
    }
    if program == "just"
        && matches!(
            args.first().map(String::as_str),
            Some("test-fast" | "test-compile" | "test-lane-main")
        )
    {
        return Err("auto-validation just routes must name a focused lane or package".to_string());
    }
    Ok(canonical)
}

fn collect_focused_validation_argv_violations(
    program: &str,
    args: &[String],
    repo_root: &Path,
) -> Vec<FocusedValidationViolation> {
    let mut violations = Vec::new();
    if args.iter().any(|arg| forbidden_control_argument(arg)) {
        violations.push(focused_validation_violation(
            "shell_control_argument_forbidden",
            "shell chaining, wrappers, and redirection are not allowed",
            json!({"shell_control_arguments": false}),
        ));
    }
    let program_result = match program {
        "cargo" => validate_cargo_validation(args).map_err(|message| {
            focused_validation_violation(
                "cargo_validation_shape_required",
                message,
                json!({"program": "cargo", "subcommands": ["check", "test"]}),
            )
        }),
        "just" => validate_just_validation(args).map_err(|message| {
            focused_validation_violation(
                "just_validation_shape_required",
                message,
                json!({"program": "just", "recipe_class": "admitted_nonmutating_validation"}),
            )
        }),
        "python" | "python3" => validate_python_validation(args, repo_root).map_err(|message| {
            focused_validation_violation(
                "python_validation_shape_required",
                message,
                json!({"program": ["python", "python3"], "module_mode": true}),
            )
        }),
        _ => Err(focused_validation_violation(
            "validation_program_required",
            "only direct cargo, just, or python validation is allowed",
            json!({"program": ["cargo", "just", "python", "python3"]}),
        )),
    };
    if let Err(violation) = program_result {
        violations.push(violation);
    }
    violations
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
        force_fresh,
        validation_launch,
        validation_leader: _,
        validation_waiter: _,
        repository: _,
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

    let known_delta = if turn.config.features.enabled(Feature::KnownDeltaStore)
        && !focused_validation
        && !exec_params.command.is_empty()
        && known_delta_store::is_immutable_git_show_candidate(
            &exec_params.command[0],
            &exec_params.command[1..],
        ) {
        let project_namespace = if let Some(source) = turn.turn_metadata_state.git_metadata_source()
            && exec_params.cwd.starts_with(source.repo_root().as_path())
        {
            source.project_namespace().await
        } else {
            None
        };
        known_delta_store::prepare_immutable_git_show(
            turn.config.codex_home.as_path(),
            &session.thread_id.to_string(),
            &exec_params.cwd,
            &exec_params.command[0],
            &exec_params.command[1..],
            project_namespace.as_deref(),
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
            .begin_attempt_with_freshness(attempt_key, repair_notice.is_some(), force_fresh)
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
    );
    let event_tracker = track_validation_freshness.then_some(&tracker);
    let event_ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), &call_id, event_tracker);
    emitter.begin(event_ctx).await;

    // This is a preliminary resolution used only for the policy compatibility check. The runtime
    // re-proves the exact command against its final cwd and child environment immediately before
    // constructing the sandbox request. A safety/execution mismatch fails closed.
    #[cfg(windows)]
    let proven_direct_argv = (is_powershell_script
        && !turn_environment.environment.is_remote()
        && exec_params.command == safety_command)
        .then(|| {
            prove_noprofile_powershell_command_as_direct_argv(
                &exec_params.command,
                exec_params.cwd.as_path(),
                &exec_params.env,
            )
        })
        .flatten();

    #[cfg(windows)]
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

    #[cfg(windows)]
    let approved_powershell_direct_argv = if let (Some(proof), Some(canonical_requirement)) =
        (proven_direct_argv, canonical_exec_approval_requirement)
        && same_exec_authorization_envelope(&exec_approval_requirement, &canonical_requirement)
        && let Some(command) = proof.into_command_for_state(
            &exec_params.command,
            exec_params.cwd.as_path(),
            &exec_params.env,
        ) {
        Some(command)
    } else {
        None
    };

    let req = ShellRequest {
        command: exec_params.command.clone(),
        command_for_approval: safety_command,
        #[cfg(windows)]
        approved_powershell_direct_argv,
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
        known_delta: known_delta.clone(),
        validation_launch,
    };
    let mut orchestrator = ToolOrchestrator::new();
    let mut runtime = ShellRuntime::for_shell_command(shell_runtime_backend);
    let validation_started_at = tokio::time::Instant::now();
    let tool_ctx = ToolCtx {
        session: session.clone(),
        turn: turn.clone(),
        call_id: call_id.clone(),
        tool_name,
    };
    let out = match orchestrator
        .run(
            &mut runtime,
            &req,
            &tool_ctx,
            &turn,
            turn.approval_policy.value(),
        )
        .await
    {
        Ok(result) => Ok(result.output),
        Err(ToolError::ValidationSkipped(skipped)) => {
            let skip_disposition = skipped.skip_disposition;
            let value = serde_json::to_value(skipped).unwrap_or_default();
            let mut output = FunctionToolOutput::from_text(value.to_string(), None)
                .with_skip_disposition(skip_disposition);
            output.post_tool_use_response = Some(value);
            return Ok(RunExecLikeResult {
                output,
                exit_code: None,
                validation_execution_outcome: ValidationExecutionOutcome::NotExecuted,
                canonical_output: None,
            });
        }
        Err(error) => Err(error),
    };
    if !known_delta_hit && let Some(known_delta) = known_delta.as_ref() {
        let observation = match &out {
            Ok(output) if is_complete_success(output) => {
                KnownDeltaExecutionObservation::CompleteSuccess {
                    output: output.aggregated_output.text.as_bytes(),
                    executor_cost: output.duration,
                }
            }
            Ok(output) if output.aggregated_output.truncated_after_lines.is_some() => {
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
    let raw_output_artifact =
        if !known_delta_hit && let (Some(_attempt_key), Ok(output)) = (&attempt_key, &out) {
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
    if let (Some(launch), Some(artifact), Some(exit_code)) = (
        req.validation_launch.as_ref(),
        raw_output_artifact.as_ref(),
        exit_code,
    ) {
        session
            .services
            .command_execution
            .publish_inline_validation(launch, artifact.clone(), validation_started_at, exit_code)
            .await;
    }
    let canonical_output = canonical_exec_output_bytes(&out);
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
            outcome: Some(match exit_code {
                Some(0) => codex_tools::ToolOutputOutcome::Success,
                Some(_) => codex_tools::ToolOutputOutcome::Failure,
                None => codex_tools::ToolOutputOutcome::TimedOut,
            }),
            post_tool_use_response,
            sampling_request_signal: None,
            deterministic_continuation_receipts: Vec::new(),
            deterministic_continuation_owner_key: None,
            skip_disposition: None,
        },
        exit_code,
        validation_execution_outcome: match exit_code {
            Some(0) => ValidationExecutionOutcome::ExecutedSuccess,
            Some(_) => ValidationExecutionOutcome::ExecutedFailure,
            None => ValidationExecutionOutcome::ExecutedFailure,
        },
        canonical_output,
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
        Err(ToolError::ValidationSkipped(_)) => None,
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
