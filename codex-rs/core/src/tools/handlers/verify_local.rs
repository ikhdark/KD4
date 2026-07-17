use crate::exec::ExecCapturePolicy;
use crate::exec::ExecParams;
use crate::exec_env::create_env;
use crate::exec_env::inject_permission_profile_env;
use crate::function_tool::FunctionCallError;
use crate::sandboxing::SandboxPermissions;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::command_output_artifact::create_content_addressed_output_artifact;
use crate::tools::command_output_artifact::output_sha256;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::shell::RunExecLikeArgs;
use crate::tools::handlers::shell::run_exec_like_with_exit_code;
use crate::tools::handlers::verify_local_spec::VERIFY_LOCAL_TOOL_NAME;
use crate::tools::handlers::verify_local_spec::VerifyLocalToolOptions;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::runtimes::shell::ShellRuntimeBackend;
use crate::tools::shell_output_summary::ShellOutputSummaryOptions;
use crate::tools::shell_output_summary::reduce_shell_output_for_model;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use codex_verify_local::CommandResultV2;
use codex_verify_local::ExactOutputArtifactV2;
use codex_verify_local::LaunchErrorKind;
use codex_verify_local::LogState;
use codex_verify_local::OutputOmissionV2;
use codex_verify_local::PlanMode;
use codex_verify_local::PlanRequest;
use codex_verify_local::RawPath;
use codex_verify_local::RepositorySnapshot;
use codex_verify_local::finalize_plan;
use codex_verify_local::plan_verification;
use codex_verify_local::random_hex_128;
use codex_verify_local::render_human;
use codex_verify_local::serialize_legacy_v1;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const VERIFY_LOCAL_JSON_SCHEMA_VERSION: u64 = 1;
const VERIFY_LOCAL_JSON_PRODUCER: &str = "kd4.verify_local";

#[derive(Debug)]
struct VerifyLocalArgs {
    mode_flag: &'static str,
    changed: Vec<String>,
    staged: bool,
    scope_current: bool,
    no_cache: bool,
    json: bool,
    environment_id: Option<String>,
}

impl VerifyLocalArgs {
    fn mode(&self) -> &'static str {
        match self.mode_flag {
            "--plan" => "plan",
            "--fast" => "fast",
            "--final" => "final",
            _ => "unknown",
        }
    }

    fn timeout_ms(&self) -> u64 {
        match self.mode_flag {
            "--plan" => 2 * 60 * 1_000,
            "--fast" => 20 * 60 * 1_000,
            "--final" => 60 * 60 * 1_000,
            _ => 20 * 60 * 1_000,
        }
    }
}

#[derive(Debug)]
struct VerifyLocalRun {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    json: Option<Value>,
    verdict_text: Option<String>,
    tool_success: bool,
    guidance: Option<&'static str>,
    scope: Option<VerifyLocalScope>,
}

impl VerifyLocalRun {
    fn has_versioned_contract(&self) -> bool {
        self.json
            .as_ref()
            .is_some_and(self::is_versioned_verify_local_json)
    }

    fn is_proof_bearing(&self) -> bool {
        self.tool_success
            && self.verdict_text.as_deref() == Some("VERIFIED")
            && self.has_versioned_contract()
            && self.scope.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifyLocalScope {
    source: String,
    active_files: Vec<PathBuf>,
    ignored_dirty_files: Vec<PathBuf>,
    stale_reasons: Vec<String>,
}

pub struct VerifyLocalHandler {
    options: VerifyLocalToolOptions,
}

impl VerifyLocalHandler {
    pub(crate) fn for_verify_local_environment_id(include_environment_id: bool) -> Self {
        Self {
            options: VerifyLocalToolOptions::with_verify_local_environment_id(
                include_environment_id,
            ),
        }
    }
}

impl ToolExecutor<ToolInvocation> for VerifyLocalHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(VERIFY_LOCAL_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        (crate::tools::handlers::verify_local_spec::VERIFY_LOCAL_TOOL_BUILDER)(self.options)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl CoreToolRuntime for VerifyLocalHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}

impl VerifyLocalHandler {
    pub(crate) fn is_available_for_step(step_context: &StepContext) -> bool {
        step_context
            .environments
            .turn_environments
            .iter()
            .any(|environment| {
                !environment.environment.is_remote()
                    && find_verify_local_repo_root(environment.cwd()).is_some()
            })
    }

    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            cancellation_token,
            payload,
            tracker,
            call_id,
            ..
        } = invocation;
        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "verify_local handler received unsupported payload".to_string(),
                ));
            }
        };

        let args = parse_verify_local_arguments(&arguments)?;
        self.run_call(
            session,
            turn,
            step_context,
            cancellation_token,
            tracker,
            call_id,
            args,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_call(
        &self,
        session: Arc<Session>,
        turn: Arc<crate::session::turn_context::TurnContext>,
        step_context: Arc<StepContext>,
        cancellation_token: CancellationToken,
        tracker: SharedTurnDiffTracker,
        call_id: String,
        args: VerifyLocalArgs,
        automatic_request: Option<&crate::task_evidence::AutomaticVerifyLocalRequest>,
        plan_override: Option<codex_verify_local::PlanEnvelopeV2>,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let environment =
            resolve_tool_environment(&step_context.environments, args.environment_id.as_deref())?
                .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "verify_local requires a selected local turn environment".to_string(),
                )
            })?;
        if environment.environment.is_remote() {
            return Err(FunctionCallError::RespondToModel(
                "verify_local is available only in the selected local environment".to_string(),
            ));
        }
        let repo_root = find_verify_local_repo_root(environment.cwd()).ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "verify_local is unavailable: expected codex-rs/verify-local and justfile in this repo"
                    .to_string(),
            )
        })?;

        let mode = match args.mode_flag {
            "--fast" => PlanMode::Fast,
            "--final" => PlanMode::Final,
            _ => PlanMode::Plan,
        };
        let changed = args
            .changed
            .iter()
            .map(|path| RawPath::from_utf8(path.replace('\\', "/")))
            .collect::<Vec<_>>();
        let snapshot = if !changed.is_empty() || args.scope_current {
            RepositorySnapshot::from_explicit_paths(&repo_root, changed.clone())
        } else {
            RepositorySnapshot::from_worktree(&repo_root)
        }
        .unwrap_or_else(|error| RepositorySnapshot::full_fallback(&repo_root, error.to_string()));
        let plan = plan_override.unwrap_or_else(|| {
            plan_verification(
                PlanRequest {
                    mode: Some(mode),
                    changed,
                    staged: args.staged,
                    scope_current: args.scope_current,
                    no_cache: args.no_cache,
                    ..PlanRequest::default()
                },
                snapshot,
            )
        });
        let validation_command = vec!["verify_local".to_string(), args.mode_flag.to_string()];
        let validation_tracker = tracker.clone();
        let validation_environment_id = environment.environment_id.clone();
        let (isolation, isolated_codex_home, isolated_sqlite_home) =
            create_isolated_validation_state()?;
        let _isolation = isolation;
        let mut base_env = create_env(
            &turn.config.permissions.shell_environment_policy,
            Some(session.thread_id),
        );
        let active_permission_profile = turn.config.permissions.active_permission_profile();
        inject_permission_profile_env(&mut base_env, active_permission_profile.as_ref());
        base_env.insert(
            "CODEX_HOME".to_string(),
            isolated_codex_home.to_string_lossy().into_owned(),
        );
        base_env.insert(
            "CODEX_SQLITE_HOME".to_string(),
            isolated_sqlite_home.to_string_lossy().into_owned(),
        );
        let validation_start = match automatic_request {
            Some(request) => Some(
                session
                    .services
                    .task_evidence
                    .begin_verify_local_validation_for_automatic_request(request)
                    .await
                    .ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "automatic verify_local run was superseded by newer task evidence"
                                .to_string(),
                        )
                    })?,
            ),
            None => {
                session
                    .services
                    .task_evidence
                    .begin_verify_local_validation()
                    .await
            }
        };
        let validation_cancellation_token = validation_start
            .as_ref()
            .map(|start| start.cancellation_token())
            .unwrap_or_else(CancellationToken::new);
        let command_cancellation_token = CancellationToken::new();
        let cancellation_forwarder = {
            let invocation_cancellation_token = cancellation_token.clone();
            let validation_cancellation_token = validation_cancellation_token.clone();
            let command_cancellation_token = command_cancellation_token.clone();
            CancellationForwarder(tokio::spawn(async move {
                tokio::select! {
                    _ = invocation_cancellation_token.cancelled() => {}
                    _ = validation_cancellation_token.cancelled() => {}
                }
                command_cancellation_token.cancel();
            }))
        };
        let mut facts = Vec::with_capacity(plan.commands.len());
        let artifact_thread_id = format!("{}/verify-local", session.thread_id);
        if mode != PlanMode::Plan && plan.verdict.is_none() {
            for (ordinal, command) in plan.commands.iter().enumerate() {
                let nonce = random_hex_128().unwrap_or_else(|_| "0".repeat(32));
                if command_cancellation_token.is_cancelled() {
                    facts.push(cancelled_command_result(&plan, command, ordinal, nonce));
                    continue;
                }
                let Some(argv) = command
                    .args
                    .iter()
                    .map(|argument| argument.legacy_text().map(str::to_string))
                    .collect::<Option<Vec<_>>>()
                else {
                    facts.push(unsupported_path_result(&plan, command, ordinal, nonce));
                    continue;
                };
                let cwd =
                    AbsolutePathBuf::from_absolute_path(repo_root.clone()).map_err(|err| {
                        FunctionCallError::RespondToModel(format!(
                            "invalid verify_local repo root: {err}"
                        ))
                    })?;
                let hook_command = codex_shell_command::parse_command::shlex_join(&argv);
                let evidence_command = hook_command.clone();
                let exec_params = ExecParams {
                    command: argv.clone(),
                    cwd,
                    expiration: command.timeout_ms.into(),
                    capture_policy: ExecCapturePolicy::Verification,
                    env: base_env.clone(),
                    network: turn.network.clone(),
                    network_environment_id: Some(environment.environment_id.clone()),
                    sandbox_permissions: SandboxPermissions::UseDefault,
                    windows_sandbox_level: turn.windows_sandbox_level,
                    windows_sandbox_private_desktop: turn
                        .config
                        .permissions
                        .windows_sandbox_private_desktop,
                    justification: None,
                    arg0: None,
                };
                let command_call_id = format!("{call_id}-{ordinal}");
                let executed = run_exec_like_with_exit_code(RunExecLikeArgs {
                    tool_name: self.tool_name(),
                    exec_params,
                    cancellation_token: command_cancellation_token.clone(),
                    hook_command,
                    safety_command: argv,
                    shell_type: None,
                    additional_permissions: None,
                    prefix_rule: None,
                    session: Arc::clone(&session),
                    turn: Arc::clone(&turn),
                    turn_environment: environment.clone(),
                    tracker: tracker.clone(),
                    call_id: command_call_id,
                    shell_runtime_backend: ShellRuntimeBackend::ShellCommandClassic,
                    track_validation_freshness: false,
                    attempt_key: None,
                    repair_notice: None,
                    capture_exec_output: true,
                })
                .await;
                facts.push(match executed {
                    Ok(executed) => {
                        command_result_from_core_execution(
                            &plan,
                            command,
                            ordinal,
                            nonce,
                            executed.exec_output.as_ref(),
                            executed.exit_code,
                            turn.config.codex_home.as_path(),
                            &artifact_thread_id,
                            &evidence_command,
                        )
                        .await
                    }
                    Err(_error) if command_cancellation_token.is_cancelled() => {
                        cancelled_command_result(&plan, command, ordinal, nonce)
                    }
                    Err(error) => CommandResultV2 {
                        runner_error: Some(format!("{error:?}")),
                        ..base_command_result(&plan, command, ordinal, nonce)
                    },
                });
            }
        }
        let completed_normally = facts
            .iter()
            .all(|result| !result.timed_out && !result.cancelled && result.runner_error.is_none());
        let finalized = finalize_plan(plan, facts);
        let contract_bytes = serialize_legacy_v1(&finalized, cfg!(windows)).map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "verify_local could not render its V1 contract: {error}"
            ))
        })?;
        let contract_text = String::from_utf8(contract_bytes).map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "verify_local rendered invalid UTF-8: {error}"
            ))
        })?;
        let contract_json = serde_json::from_str::<Value>(&contract_text).ok();
        let output_text = if args.json {
            contract_text
        } else {
            render_human(&finalized)
        };
        let active_files = finalized
            .plan
            .scope
            .as_ref()
            .map(|scope| {
                scope
                    .active_files
                    .iter()
                    .filter_map(RawPath::as_utf8)
                    .map(PathBuf::from)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let stale_reasons = finalized
            .plan
            .scope
            .as_ref()
            .map(|scope| scope.stale_reasons.clone())
            .unwrap_or_default();
        let tool_success = finalized.exit_code == 0;
        let proof_accepted = session
            .services
            .task_evidence
            .record_verify_local(
                args.mode(),
                Some(finalized.verdict.as_str()),
                tool_success,
                finalized.verdict.is_proof_bearing(),
                contract_json.is_some() && completed_normally,
                validation_start.as_ref(),
                &active_files,
                &stale_reasons,
                contract_json.as_ref(),
            )
            .await;
        if proof_accepted {
            let mut tracker = validation_tracker.lock().await;
            crate::turn_diff_tracker::TurnDiffTracker::record_verified_validation(
                &mut tracker,
                validation_command,
                &validation_environment_id,
                &active_files,
                /*clear_unknown_mutation*/ false,
            );
        }
        drop(cancellation_forwarder);
        Ok(boxed_tool_output(FunctionToolOutput::from_text(
            output_text,
            Some(tool_success),
        )))
    }
}

struct CancellationForwarder(tokio::task::JoinHandle<()>);

impl Drop for CancellationForwarder {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn base_command_result(
    plan: &codex_verify_local::PlanEnvelopeV2,
    command: &codex_verify_local::CommandSpecV2,
    ordinal: usize,
    nonce: String,
) -> CommandResultV2 {
    CommandResultV2 {
        schema_version: 2,
        invocation_id: plan.invocation_id.clone(),
        command_id: command.id.clone(),
        command_ordinal: ordinal,
        runner_nonce: nonce,
        exit_code: None,
        signal: None,
        duration_ns: 0,
        timed_out: false,
        cancelled: false,
        runner_error: None,
        launch_error: None,
        log_state: LogState::Complete,
        log_path: None,
        diagnostic: String::new(),
        exact_output_artifact: None,
        diagnostic_omission: None,
        cached: false,
        flaky: false,
        baseline: None,
    }
}

fn cancelled_command_result(
    plan: &codex_verify_local::PlanEnvelopeV2,
    command: &codex_verify_local::CommandSpecV2,
    ordinal: usize,
    nonce: String,
) -> CommandResultV2 {
    CommandResultV2 {
        cancelled: true,
        ..base_command_result(plan, command, ordinal, nonce)
    }
}

fn unsupported_path_result(
    plan: &codex_verify_local::PlanEnvelopeV2,
    command: &codex_verify_local::CommandSpecV2,
    ordinal: usize,
    nonce: String,
) -> CommandResultV2 {
    CommandResultV2 {
        runner_error: Some(
            "command contains a path that Core cannot represent losslessly".to_string(),
        ),
        launch_error: Some(LaunchErrorKind::UnsupportedPath),
        ..base_command_result(plan, command, ordinal, nonce)
    }
}

async fn command_result_from_core_execution(
    plan: &codex_verify_local::PlanEnvelopeV2,
    command: &codex_verify_local::CommandSpecV2,
    ordinal: usize,
    nonce: String,
    output: Option<&ExecToolCallOutput>,
    exit_code: Option<i32>,
    codex_home: &std::path::Path,
    artifact_thread_id: &str,
    command_text: &str,
) -> CommandResultV2 {
    let Some(output) = output else {
        return CommandResultV2 {
            exit_code,
            runner_error: Some("Core execution returned no process facts".to_string()),
            ..base_command_result(plan, command, ordinal, nonce)
        };
    };
    let exact_output = output
        .aggregated_output_bytes
        .as_deref()
        .unwrap_or_else(|| output.aggregated_output.text.as_bytes());
    let sha256 = output_sha256(exact_output);
    let artifact =
        create_content_addressed_output_artifact(codex_home, artifact_thread_id, exact_output)
            .await;
    let (stored_output_artifact, log_path, artifact_error) = match artifact {
        RawOutputArtifact::Stored { path, bytes } => (
            Some(ExactOutputArtifactV2 {
                sha256: sha256.clone(),
                path: path.clone(),
                bytes,
            }),
            Some(path),
            None,
        ),
        RawOutputArtifact::Failed {
            message,
            owned_path,
            ..
        } => (None, owned_path, Some(message)),
    };
    let exact_output_artifact = output
        .output_complete
        .then_some(stored_output_artifact)
        .flatten();
    let capture_error = (!output.output_complete)
        .then(|| "Process output capture remained incomplete after termination".to_string());
    let runner_error = match (capture_error, artifact_error) {
        (Some(capture), Some(artifact)) => Some(format!("{capture}; {artifact}")),
        (Some(capture), None) => Some(capture),
        (None, artifact) => artifact,
    };
    let reduction = reduce_shell_output_for_model(
        &output.aggregated_output.text,
        exit_code.unwrap_or(output.exit_code),
        output.timed_out,
        ShellOutputSummaryOptions {
            enabled: true,
            turn_cost_guard: true,
            command_text: Some(command_text),
        },
    );
    let (preview, diagnostic_omission) = reduction.map_or_else(
        || (output.aggregated_output.text.clone(), None),
        |reduction| {
            (
                reduction.summary,
                Some(OutputOmissionV2 {
                    bytes: u64::try_from(reduction.omitted_bytes).unwrap_or(u64::MAX),
                    lines: u64::try_from(reduction.omitted_lines).unwrap_or(u64::MAX),
                }),
            )
        },
    );
    let output_label = if output.output_complete {
        "Exact output"
    } else {
        "Incomplete captured output"
    };
    let diagnostic = format!(
        "{output_label}: sha256:{sha256} ({} bytes)\n{preview}",
        exact_output.len()
    );
    CommandResultV2 {
        exit_code: exit_code.or(Some(output.exit_code)),
        duration_ns: u64::try_from(output.duration.as_nanos()).unwrap_or(u64::MAX),
        timed_out: output.timed_out,
        runner_error,
        log_state: if !output.output_complete {
            LogState::IncompleteAfterTermination
        } else if exact_output_artifact.is_some() {
            LogState::Complete
        } else {
            LogState::IoFailure
        },
        log_path,
        diagnostic,
        exact_output_artifact,
        diagnostic_omission,
        ..base_command_result(plan, command, ordinal, nonce)
    }
}

pub(crate) async fn run_automatic_verify_local(
    session: Arc<Session>,
    step_context: Arc<StepContext>,
    tracker: SharedTurnDiffTracker,
    request: crate::task_evidence::AutomaticVerifyLocalRequest,
    cancellation_token: CancellationToken,
) -> Result<(), FunctionCallError> {
    run_automatic_verify_local_inner(
        session,
        step_context,
        tracker,
        request,
        cancellation_token,
        None,
    )
    .await
}

#[cfg(test)]
async fn run_automatic_verify_local_with_plan(
    session: Arc<Session>,
    step_context: Arc<StepContext>,
    tracker: SharedTurnDiffTracker,
    request: crate::task_evidence::AutomaticVerifyLocalRequest,
    cancellation_token: CancellationToken,
    plan: codex_verify_local::PlanEnvelopeV2,
) -> Result<(), FunctionCallError> {
    run_automatic_verify_local_inner(
        session,
        step_context,
        tracker,
        request,
        cancellation_token,
        Some(plan),
    )
    .await
}

async fn run_automatic_verify_local_inner(
    session: Arc<Session>,
    step_context: Arc<StepContext>,
    tracker: SharedTurnDiffTracker,
    request: crate::task_evidence::AutomaticVerifyLocalRequest,
    cancellation_token: CancellationToken,
    plan_override: Option<codex_verify_local::PlanEnvelopeV2>,
) -> Result<(), FunctionCallError> {
    let generation = request.evidence_generation;
    let claim_id = request.claim_id.clone();
    let args = VerifyLocalArgs {
        mode_flag: "--fast",
        scope_current: false,
        changed: request.changed_paths.clone(),
        staged: false,
        no_cache: false,
        json: false,
        environment_id: None,
    };
    let turn = Arc::clone(&step_context.turn);
    let result = VerifyLocalHandler::for_verify_local_environment_id(false)
        .run_call(
            Arc::clone(&session),
            turn,
            step_context,
            cancellation_token,
            tracker,
            format!("automatic-verify-local-{claim_id}"),
            args,
            Some(&request),
            plan_override,
        )
        .await
        .map(|_| ());
    match &result {
        Ok(()) => {
            session
                .services
                .task_evidence
                .finish_automatic_verify_local_request(generation, &claim_id)
                .await;
        }
        Err(_) => {
            session
                .services
                .task_evidence
                .release_automatic_verify_plan_request(generation, &claim_id)
                .await;
        }
    }
    result
}

fn parse_verify_local_arguments(arguments: &str) -> Result<VerifyLocalArgs, FunctionCallError> {
    let value: Value = serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse verify_local arguments: {err}"))
    })?;
    let Some(object) = value.as_object() else {
        return Err(FunctionCallError::RespondToModel(
            "verify_local arguments must be a JSON object".to_string(),
        ));
    };
    let allowed = [
        "mode",
        "changed",
        "staged",
        "scope_current",
        "no_cache",
        "json",
        "environment_id",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    let broadening = [
        "all_dirty",
        "allow_workspace",
        "related",
        "related_tests",
        "isolated",
        "baseline",
        "retry_flakes",
        "cache_readonly",
        "regen",
        "scope_start",
        "scope_add",
        "scope_reset",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    for key in object.keys() {
        if broadening.contains(key.as_str()) {
            return Err(FunctionCallError::RespondToModel(format!(
                "unknown field `{key}`; broadening or mutating flags are human CLI-only. Narrow validation with `changed`, `staged`, or `scope_current` instead."
            )));
        }
        if !allowed.contains(key.as_str()) {
            return Err(FunctionCallError::RespondToModel(format!(
                "unknown field `{key}`; verify_local only accepts read-only narrowing fields. Narrow validation with `changed`, `staged`, or `scope_current` instead."
            )));
        }
    }

    let mode_flag = match object.get("mode").and_then(Value::as_str) {
        Some("plan") => "--plan",
        Some("fast") => "--fast",
        Some("final") => "--final",
        Some(other) => {
            return Err(FunctionCallError::RespondToModel(format!(
                "failed to parse verify_local arguments: unsupported mode `{other}`"
            )));
        }
        None => {
            return Err(FunctionCallError::RespondToModel(
                "failed to parse verify_local arguments: missing string field `mode`".to_string(),
            ));
        }
    };
    let changed = object
        .get("changed")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "failed to parse verify_local arguments: missing array field `changed`".to_string(),
            )
        })?
        .iter()
        .map(|value| {
            value.as_str().map(str::to_string).ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "failed to parse verify_local arguments: `changed` must contain only strings"
                        .to_string(),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let read_bool = |name: &str| -> Result<bool, FunctionCallError> {
        object.get(name).and_then(Value::as_bool).ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "failed to parse verify_local arguments: missing boolean field `{name}`"
            ))
        })
    };
    let environment_id = match object.get("environment_id") {
        Some(Value::String(environment_id)) => Some(environment_id.clone()),
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(FunctionCallError::RespondToModel(
                "failed to parse verify_local arguments: `environment_id` must be a string or null"
                    .to_string(),
            ));
        }
    };

    Ok(VerifyLocalArgs {
        mode_flag,
        changed,
        staged: read_bool("staged")?,
        scope_current: read_bool("scope_current")?,
        no_cache: read_bool("no_cache")?,
        json: read_bool("json")?,
        environment_id,
    })
}

fn build_verify_local_argv(args: &VerifyLocalArgs) -> Vec<String> {
    let mut argv = vec![
        "just".to_string(),
        "verify-local".to_string(),
        args.mode_flag.to_string(),
        "--json".to_string(),
    ];
    for path in &args.changed {
        argv.push(format!("--changed={path}"));
    }
    if args.staged {
        argv.push("--staged".to_string());
    }
    if args.scope_current {
        argv.push("--scope".to_string());
        argv.push("current".to_string());
    }
    if args.no_cache {
        argv.push("--no-cache".to_string());
    }
    argv
}

fn find_verify_local_repo_root(cwd: &PathUri) -> Option<PathBuf> {
    let cwd = cwd.to_abs_path().ok()?;
    for candidate in cwd.as_path().ancestors() {
        if candidate
            .join("codex-rs")
            .join("verify-local")
            .join("Cargo.toml")
            .is_file()
            && candidate.join("justfile").is_file()
        {
            return Some(candidate.to_path_buf());
        }
    }
    None
}

fn create_isolated_validation_state()
-> Result<(tempfile::TempDir, PathBuf, PathBuf), FunctionCallError> {
    let isolation = tempfile::Builder::new()
        .prefix("codex-verify-local-")
        .tempdir()
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to isolate verify_local state: {err}"
            ))
        })?;
    let codex_home = isolation.path().join("codex-home");
    let sqlite_home = isolation.path().join("sqlite-home");
    std::fs::create_dir_all(&codex_home)
        .and_then(|_| std::fs::create_dir_all(&sqlite_home))
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to create isolated verify_local state: {err}"
            ))
        })?;
    Ok((isolation, codex_home, sqlite_home))
}

fn parse_verify_local_run(
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
) -> VerifyLocalRun {
    let json = parse_verify_local_json(&stdout);
    let has_versioned_json = json
        .as_ref()
        .is_some_and(self::is_versioned_verify_local_json);
    let verdict_text = if has_versioned_json {
        json.as_ref()
            .and_then(|value| value.get("verdict"))
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        parse_text_verdict(&stdout, &stderr)
    };
    let (semantic_success, guidance) = if json.is_some() && !has_versioned_json {
        (
            false,
            Some(
                "The verifier returned an unsupported JSON contract; expected schema_version 1 from kd4.verify_local.",
            ),
        )
    } else {
        match verdict_text.as_deref() {
            Some("VERIFIED") => (true, None),
            Some("VERIFIED (no proof needed)") => (
                true,
                Some(
                    "VERIFIED (no proof needed) completed cleanly but does not count as proof-bearing validation evidence.",
                ),
            ),
            Some("PLANNED") => (
                true,
                Some(
                    "PLANNED returned the verifier plan only; run fast or final mode for proof-bearing validation.",
                ),
            ),
            Some("NEEDS_SCOPE") => (
                false,
                Some(
                    "NEEDS_SCOPE: narrow validation with changed, staged, or scope_current, then rerun verify_local.",
                ),
            ),
            Some("NEEDS_REGEN") => (
                false,
                Some(
                    "NEEDS_REGEN: regeneration is mutating and CLI-only, so this is an autonomous blocker.",
                ),
            ),
            Some(_) | None => (
                false,
                Some(
                    "The verifier did not produce proof-bearing validation; fix the issue or report the blocker.",
                ),
            ),
        }
    };
    let tool_success = semantic_success && exit_code == Some(0);
    let scope = json
        .as_ref()
        .filter(|_| has_versioned_json)
        .and_then(self::parse_json_scope_value);
    VerifyLocalRun {
        exit_code,
        stdout,
        stderr,
        json,
        verdict_text,
        tool_success,
        guidance,
        scope,
    }
}

fn is_versioned_verify_local_json(value: &Value) -> bool {
    value.is_object()
        && value.get("schema_version").and_then(Value::as_u64)
            == Some(self::VERIFY_LOCAL_JSON_SCHEMA_VERSION)
        && value.get("producer").and_then(Value::as_str) == Some(self::VERIFY_LOCAL_JSON_PRODUCER)
        && value.get("verdict").and_then(Value::as_str).is_some()
}

fn parse_verify_local_json(stdout: &str) -> Option<Value> {
    serde_json::from_str::<Value>(stdout)
        .ok()
        .filter(Value::is_object)
}

fn parse_json_scope_value(value: &Value) -> Option<VerifyLocalScope> {
    let scope = value.get("scope")?.as_object()?;
    let source = scope
        .get("source")
        .and_then(Value::as_str)
        .filter(|source| !source.trim().is_empty())?
        .to_string();
    Some(VerifyLocalScope {
        source,
        active_files: parse_path_array(scope.get("active_files")?)?,
        ignored_dirty_files: parse_path_array(scope.get("ignored_dirty_files")?)?,
        stale_reasons: parse_string_array(scope.get("stale_reasons")?)?,
    })
}

fn parse_path_array(value: &Value) -> Option<Vec<PathBuf>> {
    parse_string_array(value)?
        .into_iter()
        .map(|path| (!path.trim().is_empty()).then(|| PathBuf::from(path)))
        .collect()
}

fn parse_string_array(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect()
}

fn parse_text_verdict(stdout: &str, stderr: &str) -> Option<String> {
    stdout
        .lines()
        .chain(stderr.lines())
        .find_map(|line| line.trim().strip_prefix("Verdict:").map(str::trim))
        .map(str::to_string)
}

fn finalize_verify_local_output(
    output: FunctionToolOutput,
    exec_output: Option<&ExecToolCallOutput>,
    exit_code: Option<i32>,
    raw_json: bool,
) -> (FunctionToolOutput, VerifyLocalRun) {
    let original_post_tool_use_response = output.post_tool_use_response;
    let run = exec_output.map_or_else(
        || parse_verify_local_run(String::new(), String::new(), None),
        |exec_output| {
            parse_verify_local_run(
                exec_output.stdout.text.clone(),
                exec_output.stderr.text.clone(),
                exit_code,
            )
        },
    );
    let mut transformed = FunctionToolOutput::from_text(
        render_verify_local_output(&run, raw_json),
        Some(run.tool_success),
    );
    transformed.post_tool_use_response = run.json.clone().or(original_post_tool_use_response);
    (transformed, run)
}

fn render_verify_local_output(run: &VerifyLocalRun, raw_json: bool) -> String {
    if raw_json {
        if let Some(value) = &run.json {
            return serde_json::to_string_pretty(value).unwrap_or_else(|_| render_raw_output(run));
        }
        return render_raw_output(run);
    }

    let exit_code = run
        .exit_code
        .map_or_else(|| "unknown".to_string(), |code| code.to_string());
    let verdict = run.verdict_text.as_deref().unwrap_or("UNKNOWN");
    let mut text = format!("Verdict: {verdict}\nExit code: {exit_code}");
    if let Some(scope) = &run.scope {
        text.push_str("\nScope: ");
        text.push_str(&scope.source);
        if !scope.active_files.is_empty() {
            text.push_str(&format!(" ({} active file(s))", scope.active_files.len()));
        }
    }
    if let Some(guidance) = run.guidance {
        text.push_str("\nGuidance: ");
        text.push_str(guidance);
    }
    if let Some(value) = &run.json {
        append_structured_summary(&mut text, value);
    }
    if !run.stderr.trim().is_empty() {
        text.push_str("\n\nStderr:\n");
        text.push_str(run.stderr.trim());
    }
    if run.json.is_none()
        && run.verdict_text.as_deref() != Some("VERIFIED")
        && !run.stdout.trim().is_empty()
    {
        text.push_str("\n\nStdout:\n");
        text.push_str(run.stdout.trim());
    }
    text
}

fn append_structured_summary(text: &mut String, value: &Value) {
    if let Some(mode) = value.get("mode").and_then(Value::as_str) {
        text.push_str("\nMode: ");
        text.push_str(mode);
    }
    append_entry_summary(text, value, "planned", "Planned checks");
    append_entry_summary(text, value, "results", "Results");
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        text.push_str("\nError: ");
        text.push_str(error);
    }
    if let Some(rerun) = value.get("rerun").and_then(Value::as_str) {
        text.push_str("\nRerun: ");
        text.push_str(rerun);
    }
}

fn append_entry_summary(text: &mut String, value: &Value, field: &str, label: &str) {
    let Some(entries) = value.get(field).and_then(Value::as_array) else {
        return;
    };
    text.push_str(&format!("\n{label}: {}", entries.len()));
    let ids = entries
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .take(4)
        .collect::<Vec<_>>();
    if !ids.is_empty() {
        text.push_str(" (");
        text.push_str(&ids.join(", "));
        if entries.len() > ids.len() {
            text.push_str(&format!(", +{} more", entries.len() - ids.len()));
        }
        text.push(')');
    }
}

fn render_raw_output(run: &VerifyLocalRun) -> String {
    let exit_code = run
        .exit_code
        .map_or_else(|| "unknown".to_string(), |code| code.to_string());
    let mut text = format!("Exit code: {exit_code}\nStdout:\n{}", run.stdout.trim());
    if !run.stderr.trim().is_empty() {
        text.push_str("\n\nStderr:\n");
        text.push_str(run.stderr.trim());
    }
    if let Some(guidance) = run.guidance {
        text.push_str("\n\nGuidance: ");
        text.push_str(guidance);
    }
    text
}

#[cfg(test)]
#[path = "verify_local_tests.rs"]
mod tests;
