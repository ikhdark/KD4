use crate::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolCallSource;
use crate::tools::sandboxing::ToolError;
use crate::tools::tool_dispatch_trace::active_tool_dispatch_timing;
use codex_apply_patch::AppliedPatchDelta;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::FileChangeItem;
use codex_protocol::items::TurnItem;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::ExecCommandStatus;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyStatus;
use codex_protocol::protocol::TurnDiffEvent;
use codex_shell_command::parse_command::parse_command;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use codex_utils_string::truncate_middle_with_token_budget;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use super::format_exec_output_str;

const REJECTION_OUTPUT_MAX_TOKENS: usize = 900;

#[derive(Clone, Copy)]
pub(crate) struct ToolEventCtx<'a> {
    patch_environment: Option<&'a crate::session::turn_context::TurnEnvironment>,
    pub session: &'a Session,
    pub turn: &'a TurnContext,
    pub call_id: &'a str,
    pub turn_diff_tracker: Option<&'a SharedTurnDiffTracker>,
    pub parent_call_id: Option<&'a str>,
    pub parent_cell_id: Option<&'a str>,
    pub runtime_tool_call_id: Option<&'a str>,
    pub execution_id: Option<&'a str>,
    pub observed_exit_code: Option<Option<i32>>,
    pub output_metadata: Option<&'a codex_protocol::items::CommandExecutionOutputMetadata>,
}

impl<'a> ToolEventCtx<'a> {
    pub fn new(
        session: &'a Session,
        turn: &'a TurnContext,
        call_id: &'a str,
        turn_diff_tracker: Option<&'a SharedTurnDiffTracker>,
    ) -> Self {
        Self {
            session,
            turn,
            call_id,
            turn_diff_tracker,
            patch_environment: None,
            parent_call_id: None,
            parent_cell_id: None,
            runtime_tool_call_id: None,
            execution_id: None,
            observed_exit_code: None,
            output_metadata: None,
        }
    }

    pub fn with_call_source(mut self, source: &'a ToolCallSource) -> Self {
        if let ToolCallSource::CodeMode {
            cell_id,
            parent_call_id,
            runtime_tool_call_id,
            ..
        } = source
        {
            self.parent_call_id = parent_call_id.as_deref();
            self.parent_cell_id = Some(cell_id);
            self.runtime_tool_call_id = Some(runtime_tool_call_id);
        }
        self
    }
}

pub(crate) enum ToolEventStage<'a> {
    Begin,
    Success {
        output: ExecToolCallOutput,
        applied_patch_delta: Option<&'a AppliedPatchDelta>,
        formatted_output: Option<String>,
    },
    Failure(ToolEventFailure<'a>),
}

pub(crate) enum ToolEventFailure<'a> {
    Output {
        output: ExecToolCallOutput,
        formatted_output: Option<String>,
    },
    Message(String),
    Denied {
        message: String,
        applied_patch_delta: Option<&'a AppliedPatchDelta>,
    },
}

enum TurnDiffTrackerUpdate<'a> {
    Track {
        environment_id: Option<String>,
        delta: &'a AppliedPatchDelta,
    },
    Invalidate,
    None,
}

fn tracker_update_for_known_delta<'a>(
    environment_id: Option<&str>,
    delta: &'a AppliedPatchDelta,
) -> TurnDiffTrackerUpdate<'a> {
    if delta.is_exact() && delta.is_empty() {
        TurnDiffTrackerUpdate::None
    } else {
        TurnDiffTrackerUpdate::Track {
            environment_id: environment_id.map(str::to_string),
            delta,
        }
    }
}

fn apply_patch_intent_paths(changes: &HashMap<PathBuf, FileChange>) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(changes.len());
    for (path, change) in changes {
        paths.push(path.clone());
        if let FileChange::Update {
            move_path: Some(move_path),
            ..
        } = change
            && !move_path.as_os_str().is_empty()
        {
            paths.push(move_path.clone());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn apply_patch_local_cwd(
    ctx: ToolEventCtx<'_>,
    environment_id: Option<&str>,
) -> Option<AbsolutePathBuf> {
    let environment = ctx.patch_environment.or_else(|| match environment_id {
        Some(environment_id) => ctx
            .turn
            .environments
            .turn_environments
            .iter()
            .find(|environment| environment.environment_id == environment_id),
        None => ctx.turn.environments.primary(),
    })?;
    if environment.environment.is_remote() {
        return None;
    }
    environment.cwd().to_abs_path().ok()
}

pub(crate) async fn emit_exec_command_begin(
    ctx: ToolEventCtx<'_>,
    command: &[String],
    cwd: &PathUri,
    parsed_cmd: &[ParsedCommand],
    source: ExecCommandSource,
    interaction_input: Option<String>,
    process_id: Option<&str>,
) {
    ctx.session
        .emit_turn_item_started(
            ctx.turn,
            &TurnItem::CommandExecution(CommandExecutionItem {
                environment: ctx
                    .patch_environment
                    .map(super::super::session::turn_context::TurnEnvironment::provenance),
                output_metadata: None,
                id: ctx.call_id.to_string(),
                process_id: process_id.map(str::to_owned),
                parent_call_id: ctx.parent_call_id.map(str::to_owned),
                parent_cell_id: ctx.parent_cell_id.map(str::to_owned),
                runtime_tool_call_id: ctx.runtime_tool_call_id.map(str::to_owned),
                execution_id: ctx.execution_id.map(str::to_owned).or_else(|| {
                    active_tool_dispatch_timing().map(|timing| timing.execution_id().0.clone())
                }),
                command: command.to_vec(),
                cwd: cwd.clone(),
                parsed_cmd: parsed_cmd.to_vec(),
                source,
                interaction_input,
                status: CommandExecutionStatus::InProgress,
                stdout: None,
                stderr: None,
                aggregated_output: None,
                exit_code: None,
                duration: None,
                formatted_output: None,
            }),
        )
        .await;
}
// Concrete, allocation-free emitter: avoid trait objects and boxed futures.
pub(crate) enum ToolEmitter {
    Shell {
        captured_environment: Option<crate::session::turn_context::TurnEnvironment>,
        command: Vec<String>,
        max_output_tokens: Option<usize>,
        model_command_text: Option<String>,
        cwd: PathUri,
        source: ExecCommandSource,
        parsed_cmd: Vec<ParsedCommand>,
        environment_id: String,
    },
    ApplyPatch {
        changes: HashMap<PathBuf, FileChange>,
        auto_approved: bool,
        environment_id: Option<String>,
        captured_environment: Option<crate::session::turn_context::TurnEnvironment>,
    },
    UnifiedExec {
        captured_environment: Option<crate::session::turn_context::TurnEnvironment>,
        command: Vec<String>,
        model_command_text: Option<String>,
        cwd: PathUri,
        source: ExecCommandSource,
        parsed_cmd: Vec<ParsedCommand>,
        process_id: Option<String>,
        environment_id: String,
    },
}

impl ToolEmitter {
    pub fn shell(
        command: Vec<String>,
        cwd: AbsolutePathBuf,
        source: ExecCommandSource,
        environment_id: String,
    ) -> Self {
        let parsed_cmd = parse_command(&command);
        Self::Shell {
            captured_environment: None,
            command,
            max_output_tokens: None,
            model_command_text: None,
            cwd: PathUri::from_abs_path(&cwd),
            source,
            parsed_cmd,
            environment_id,
        }
    }

    pub fn apply_patch_for_environment(
        changes: HashMap<PathBuf, FileChange>,
        auto_approved: bool,
        environment_id: String,
    ) -> Self {
        Self::ApplyPatch {
            changes,
            auto_approved,
            environment_id: Some(environment_id),
            captured_environment: None,
        }
    }

    pub fn with_captured_environment(
        mut self,
        environment: &crate::session::turn_context::TurnEnvironment,
    ) -> Self {
        let (Self::ApplyPatch {
            captured_environment,
            ..
        }
        | Self::Shell {
            captured_environment,
            ..
        }
        | Self::UnifiedExec {
            captured_environment,
            ..
        }) = &mut self;
        *captured_environment = Some(environment.clone());
        self
    }

    pub fn unified_exec(
        command: &[String],
        cwd: PathUri,
        source: ExecCommandSource,
        process_id: Option<String>,
        environment_id: String,
    ) -> Self {
        let parsed_cmd = parse_command(command);
        Self::UnifiedExec {
            captured_environment: None,
            command: command.to_vec(),
            model_command_text: None,
            cwd,
            source,
            parsed_cmd,
            process_id,
            environment_id,
        }
    }

    pub fn with_max_output_tokens(mut self, requested_limit: Option<usize>) -> Self {
        if let Self::Shell {
            max_output_tokens, ..
        } = &mut self
        {
            *max_output_tokens = requested_limit;
        }
        self
    }

    pub fn with_model_command_text(mut self, command_text: String) -> Self {
        match &mut self {
            Self::Shell {
                model_command_text, ..
            }
            | Self::UnifiedExec {
                model_command_text, ..
            } => *model_command_text = Some(command_text),
            Self::ApplyPatch { .. } => {}
        }
        self
    }

    pub async fn emit<'a>(
        &'a self,
        mut ctx: ToolEventCtx<'a>,
        stage: ToolEventStage<'_>,
    ) -> CodexResult<()> {
        let (Self::ApplyPatch {
            captured_environment,
            ..
        }
        | Self::Shell {
            captured_environment,
            ..
        }
        | Self::UnifiedExec {
            captured_environment,
            ..
        }) = self;
        ctx.patch_environment = captured_environment.as_ref();
        match (self, stage) {
            (
                Self::Shell {
                    command,
                    cwd,
                    source,
                    parsed_cmd,
                    environment_id,
                    ..
                },
                stage,
            ) => {
                emit_exec_stage(
                    ctx,
                    ExecCommandInput::new(
                        command,
                        cwd,
                        environment_id,
                        parsed_cmd,
                        *source,
                        /*interaction_input*/ None,
                        /*process_id*/ None,
                    ),
                    stage,
                )
                .await?;
            }

            (
                Self::ApplyPatch {
                    changes,
                    auto_approved,
                    ..
                },
                ToolEventStage::Begin,
            ) => {
                ctx.session
                    .emit_turn_item_started(
                        ctx.turn,
                        &TurnItem::FileChange(FileChangeItem {
                            effects: None,
                            environment: ctx.patch_environment.map(
                                super::super::session::turn_context::TurnEnvironment::provenance,
                            ),
                            id: ctx.call_id.to_string(),
                            changes: changes.clone(),
                            status: None,
                            auto_approved: Some(*auto_approved),
                            stdout: None,
                            stderr: None,
                        }),
                    )
                    .await;
            }
            (
                Self::ApplyPatch {
                    changes,
                    environment_id,
                    ..
                },
                ToolEventStage::Success {
                    output,
                    applied_patch_delta,
                    ..
                },
            ) => {
                let status = if output.exit_code == 0 {
                    PatchApplyStatus::Completed
                } else {
                    PatchApplyStatus::Failed
                };
                let tracker_update = applied_patch_delta
                    .map(|delta| tracker_update_for_known_delta(environment_id.as_deref(), delta))
                    .unwrap_or(TurnDiffTrackerUpdate::Invalidate);
                emit_patch_end(
                    ctx,
                    changes.clone(),
                    output.stdout.text.clone(),
                    output.stderr.text.clone(),
                    status,
                    tracker_update,
                    environment_id.as_deref(),
                )
                .await?;
            }
            (
                Self::ApplyPatch {
                    changes,
                    environment_id,
                    ..
                },
                ToolEventStage::Failure(ToolEventFailure::Output { output, .. }),
            ) => {
                emit_patch_end(
                    ctx,
                    changes.clone(),
                    output.stdout.text.clone(),
                    output.stderr.text.clone(),
                    if output.exit_code == 0 {
                        PatchApplyStatus::Completed
                    } else {
                        PatchApplyStatus::Failed
                    },
                    TurnDiffTrackerUpdate::Invalidate,
                    environment_id.as_deref(),
                )
                .await?;
            }
            (
                Self::ApplyPatch {
                    changes,
                    environment_id,
                    ..
                },
                ToolEventStage::Failure(ToolEventFailure::Message(message)),
            ) => {
                emit_patch_end(
                    ctx,
                    changes.clone(),
                    String::new(),
                    (*message).to_string(),
                    PatchApplyStatus::Failed,
                    TurnDiffTrackerUpdate::None,
                    environment_id.as_deref(),
                )
                .await?;
            }
            (
                Self::ApplyPatch {
                    changes,
                    environment_id,
                    ..
                },
                ToolEventStage::Failure(ToolEventFailure::Denied {
                    message,
                    applied_patch_delta,
                }),
            ) => {
                emit_patch_end(
                    ctx,
                    changes.clone(),
                    String::new(),
                    (*message).to_string(),
                    PatchApplyStatus::Declined,
                    applied_patch_delta
                        .map(|delta| {
                            tracker_update_for_known_delta(environment_id.as_deref(), delta)
                        })
                        .unwrap_or(TurnDiffTrackerUpdate::None),
                    environment_id.as_deref(),
                )
                .await?;
            }
            (
                Self::UnifiedExec {
                    command,
                    cwd,
                    source,
                    parsed_cmd,
                    process_id,
                    environment_id,
                    ..
                },
                stage,
            ) => {
                emit_exec_stage(
                    ctx,
                    ExecCommandInput::new(
                        command,
                        cwd,
                        environment_id,
                        parsed_cmd,
                        *source,
                        /*interaction_input*/ None,
                        process_id.as_deref(),
                    ),
                    stage,
                )
                .await?;
            }
        }
        Ok(())
    }

    pub async fn begin(&self, ctx: ToolEventCtx<'_>) -> CodexResult<()> {
        self.emit(ctx, ToolEventStage::Begin).await
    }

    fn format_exec_output_for_model(
        &self,
        output: &ExecToolCallOutput,
        ctx: ToolEventCtx<'_>,
    ) -> String {
        let truncation_policy = ctx.turn.model_info.truncation_policy.into();
        let requested_limit = match self {
            Self::Shell {
                max_output_tokens, ..
            } => *max_output_tokens,
            _ => None,
        };
        match self {
            Self::Shell {
                command,
                model_command_text,
                ..
            }
            | Self::UnifiedExec {
                command,
                model_command_text,
                ..
            } => {
                let command_text = model_command_text
                    .as_deref()
                    .map(std::borrow::Cow::Borrowed)
                    .unwrap_or_else(|| std::borrow::Cow::Owned(command.join(" ")));
                let projected = super::project_exec_output_for_model_with_budget(
                    output,
                    truncation_policy,
                    requested_limit,
                    Some(&command_text),
                );
                projected.text
            }
            Self::ApplyPatch { .. } => {
                // Preserve the producer's complete edit summary. The registry
                // retains the canonical patch result before projecting it.
                super::build_content_with_timeout(output).into_owned()
            }
        }
    }

    pub async fn finish(
        &self,
        ctx: ToolEventCtx<'_>,
        out: Result<ExecToolCallOutput, ToolError>,
        applied_patch_delta: Option<&AppliedPatchDelta>,
    ) -> Result<String, FunctionCallError> {
        let execution_summary = match &out {
            Ok(output) => format!("Command completed with exit code {}.", output.exit_code),
            Err(_) => "The tool's execution outcome follows.".to_string(),
        };
        let (event, result) = match out {
            Ok(output) => {
                let content = self.format_exec_output_for_model(&output, ctx);
                let exit_code = output.exit_code;
                let event = ToolEventStage::Success {
                    output,
                    applied_patch_delta,
                    formatted_output: Some(content.clone()),
                };
                let result = if exit_code == 0 {
                    Ok(content)
                } else {
                    Err(FunctionCallError::RespondToModel(content))
                };
                (event, result)
            }
            Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Timeout { output }))) => {
                let response = self.format_exec_output_for_model(&output, ctx);
                let event = ToolEventStage::Failure(ToolEventFailure::Output {
                    output: *output,
                    formatted_output: Some(response.clone()),
                });
                let result = Err(FunctionCallError::RespondToModel(response));
                (event, result)
            }
            Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied { output, .. }))) => {
                let response = self.format_exec_output_for_model(&output, ctx);
                // apply_patch can be denied after it has already committed a
                // known prefix. Reuse the output-bearing path so the visible
                // item still fails while the turn diff consumes that prefix.
                let event = match (self, applied_patch_delta) {
                    (Self::ApplyPatch { .. }, Some(delta)) => ToolEventStage::Success {
                        output: *output,
                        applied_patch_delta: Some(delta),
                        formatted_output: Some(response.clone()),
                    },
                    _ => ToolEventStage::Failure(ToolEventFailure::Output {
                        output: *output,
                        formatted_output: Some(response.clone()),
                    }),
                };
                let result = Err(FunctionCallError::RespondToModel(response));
                (event, result)
            }
            Err(ToolError::Codex(err)) => {
                let message = format!("execution error: {err}");
                let message = self.bound_error_message(message);
                let event = ToolEventStage::Failure(ToolEventFailure::Message(message.clone()));
                let result = Err(FunctionCallError::RespondToModel(message));
                (event, result)
            }
            Err(ToolError::Rejected(msg)) => {
                let bounded = self.bound_error_message(msg);
                let event = ToolEventStage::Failure(ToolEventFailure::Message(bounded.clone()));
                let result = Err(FunctionCallError::RespondToModel(bounded));
                (event, result)
            }
            Err(ToolError::Denied(msg)) => {
                // Normalize common denial messages for exec tools so tests and
                // users see a clear, consistent phrase.
                let normalized = if msg == "rejected by user" {
                    match self {
                        Self::Shell { .. } | Self::UnifiedExec { .. } => {
                            "exec command rejected by user".to_string()
                        }
                        Self::ApplyPatch { .. } => "patch rejected by user".to_string(),
                    }
                } else {
                    msg
                };
                let bounded = self.bound_error_message(normalized);
                let event = ToolEventStage::Failure(ToolEventFailure::Denied {
                    message: bounded.clone(),
                    applied_patch_delta,
                });
                let result = Err(FunctionCallError::RespondToModel(bounded));
                (event, result)
            }
        };
        if let Err(error) = self.emit(ctx, event).await {
            let outcome = match &result {
                Ok(content) => content.as_str(),
                Err(FunctionCallError::RespondToModel(content)) => content.as_str(),
                _ => "The tool outcome was recorded in the completion event.",
            };
            return Err(FunctionCallError::Fatal(format!(
                "{execution_summary} Do not repeat completed effects.\n{outcome}\n\nTool-history persistence failed; continuation stopped because durability is unavailable: {error}"
            )));
        }
        result
    }

    fn bound_error_message(&self, message: String) -> String {
        if matches!(self, Self::ApplyPatch { .. }) {
            message
        } else {
            truncate_middle_with_token_budget(&message, REJECTION_OUTPUT_MAX_TOKENS).0
        }
    }
}

struct ExecCommandInput<'a> {
    command: &'a [String],
    cwd: &'a PathUri,
    environment_id: &'a str,
    parsed_cmd: &'a [ParsedCommand],
    source: ExecCommandSource,
    interaction_input: Option<&'a str>,
    process_id: Option<&'a str>,
}

impl<'a> ExecCommandInput<'a> {
    fn new(
        command: &'a [String],
        cwd: &'a PathUri,
        environment_id: &'a str,
        parsed_cmd: &'a [ParsedCommand],
        source: ExecCommandSource,
        interaction_input: Option<&'a str>,
        process_id: Option<&'a str>,
    ) -> Self {
        Self {
            command,
            cwd,
            environment_id,
            parsed_cmd,
            source,
            interaction_input,
            process_id,
        }
    }
}

struct ExecCommandResult {
    stdout: String,
    stderr: String,
    aggregated_output: String,
    exit_code: Option<i32>,
    duration: Duration,
    formatted_output: String,
    status: ExecCommandStatus,
    timed_out: bool,
}



async fn emit_exec_stage(
    ctx: ToolEventCtx<'_>,
    exec_input: ExecCommandInput<'_>,
    stage: ToolEventStage<'_>,
) -> CodexResult<()> {
    let failed = matches!(&stage, ToolEventStage::Failure(_))
        || ctx
            .output_metadata
            .is_some_and(|metadata| metadata.failure_cause.is_some());
    match stage {
        ToolEventStage::Begin => {

            emit_exec_command_begin(
                ctx,
                exec_input.command,
                exec_input.cwd,
                exec_input.parsed_cmd,
                exec_input.source,
                exec_input.interaction_input.map(str::to_owned),
                exec_input.process_id,
            )
            .await;
        }
        ToolEventStage::Success {
            output,
            formatted_output,
            ..
        }
        | ToolEventStage::Failure(ToolEventFailure::Output {
            output,
            formatted_output,
        }) => {
            let exec_result = ExecCommandResult {
                stdout: output.stdout.text.clone(),
                stderr: output.stderr.text.clone(),
                aggregated_output: output.aggregated_output.text.clone(),
                exit_code: ctx.observed_exit_code.unwrap_or(Some(output.exit_code)),
                duration: output.duration,
                formatted_output: formatted_output.unwrap_or_else(|| {
                    format_exec_output_str(&output, ctx.turn.model_info.truncation_policy.into())
                }),
                status: if !failed
                    && !output.timed_out
                    && (ctx.observed_exit_code.unwrap_or(Some(output.exit_code)) == Some(0)
                        || ctx
                            .output_metadata
                            .is_some_and(|metadata| metadata.search_no_match))
                {
                    ExecCommandStatus::Completed
                } else {
                    ExecCommandStatus::Failed
                },
                timed_out: output.timed_out,
            };
            emit_exec_end(ctx, exec_input, exec_result).await?;
        }
        ToolEventStage::Failure(ToolEventFailure::Message(message)) => {
            let text = message.to_string();
            let exec_result = ExecCommandResult {
                stdout: String::new(),
                stderr: text.clone(),
                aggregated_output: text.clone(),
                exit_code: None,
                duration: Duration::ZERO,
                formatted_output: text,
                status: ExecCommandStatus::Failed,
                timed_out: false,
            };
            emit_exec_end(ctx, exec_input, exec_result).await?;
        }
        ToolEventStage::Failure(ToolEventFailure::Denied { message, .. }) => {
            let text = message.to_string();
            let exec_result = ExecCommandResult {
                stdout: String::new(),
                stderr: text.clone(),
                aggregated_output: text.clone(),
                exit_code: None,
                duration: Duration::ZERO,
                formatted_output: text,
                status: ExecCommandStatus::Declined,
                timed_out: false,
            };
            emit_exec_end(ctx, exec_input, exec_result).await?;
        }
    }
    Ok(())
}


async fn emit_exec_end(
    ctx: ToolEventCtx<'_>,
    exec_input: ExecCommandInput<'_>,
    exec_result: ExecCommandResult,
) -> CodexResult<()> {
    let mut persistence_result = Ok(());
    // Command outputs are not a file-change inventory. Keep caches conservative
    // without parsing commands, taking workspace baselines, or changing patch diffs.
    if exec_result.status != ExecCommandStatus::Declined {
        ctx.session.services.git_workspace.note_host_workspace_mutation();
        persistence_result = ctx.session
            .invalidate_tool_history_source_dependencies(None, None).await;
        if let Some(tracker) = ctx.turn_diff_tracker {
            let revision = {
                let mut tracker = tracker.lock().await;
                tracker.record_command_execution();
                tracker.current_mutation_revision()
            };
            ctx.session.services.command_execution
                .invalidate_repository_revision(&ctx.turn.sub_id, revision).await;
        }
    }
    ctx.session
        .emit_turn_item_completed(
            ctx.turn,
            TurnItem::CommandExecution(CommandExecutionItem {
                environment: ctx
                    .patch_environment
                    .map(super::super::session::turn_context::TurnEnvironment::provenance),
                id: ctx.call_id.to_string(),
                process_id: exec_input.process_id.map(str::to_owned),
                output_metadata: ctx.output_metadata.cloned(),
                parent_call_id: ctx.parent_call_id.map(str::to_owned),
                parent_cell_id: ctx.parent_cell_id.map(str::to_owned),
                runtime_tool_call_id: ctx.runtime_tool_call_id.map(str::to_owned),
                execution_id: ctx.execution_id.map(str::to_owned).or_else(|| {
                    active_tool_dispatch_timing().map(|timing| timing.execution_id().0.clone())
                }),
                command: exec_input.command.to_vec(),
                cwd: exec_input.cwd.clone(),
                parsed_cmd: exec_input.parsed_cmd.to_vec(),
                source: exec_input.source,
                interaction_input: exec_input.interaction_input.map(str::to_owned),
                status: exec_result.status.into(),
                stdout: Some(exec_result.stdout),
                stderr: Some(exec_result.stderr),
                aggregated_output: Some(exec_result.aggregated_output),
                exit_code: exec_result.exit_code,
                duration: Some(exec_result.duration),
                formatted_output: Some(exec_result.formatted_output),
            }),
        )
        .await;
    // The external command already ran. Publish its terminal item and account for
    // its mutation before propagating the durability barrier failure.
    persistence_result
}


fn patch_effects(update: &TurnDiffTrackerUpdate<'_>) -> codex_protocol::items::FileChangeEffects {
    use codex_apply_patch::AppliedPatchFileChange;
    use codex_protocol::items::FileChangeEffect;
    let mut changes = Vec::new();
    if let TurnDiffTrackerUpdate::Track { delta, .. } = update {
        for change in delta.changes() {
            let path = change.path_uri().clone();
            match &change.change {
                AppliedPatchFileChange::Add { content, overwritten_content } => changes.push(FileChangeEffect {
                    path, before: overwritten_content.clone(), after: Some(content.clone()),
                }),
                AppliedPatchFileChange::Delete { content } => changes.push(FileChangeEffect {
                    path, before: Some(content.clone()), after: None,
                }),
                AppliedPatchFileChange::Update { old_content, new_content, overwritten_move_content, .. } => {
                    if let Some(destination) = change.move_path_uri() {
                        changes.push(FileChangeEffect { path: destination.clone(),
                            before: overwritten_move_content.clone(), after: Some(new_content.clone()) });
                        changes.push(FileChangeEffect { path, before: Some(old_content.clone()), after: None });
                    } else {
                        changes.push(FileChangeEffect { path, before: Some(old_content.clone()), after: Some(new_content.clone()) });
                    }
                }
            }
        }
    }
    let exact = match update {
        TurnDiffTrackerUpdate::Track { delta, .. } => delta.is_exact(),
        TurnDiffTrackerUpdate::None => true,
        TurnDiffTrackerUpdate::Invalidate => false,
    };
    codex_protocol::items::FileChangeEffects { changes, exact }
}

async fn emit_patch_end(
    ctx: ToolEventCtx<'_>,
    changes: HashMap<PathBuf, FileChange>,
    stdout: String,
    stderr: String,
    status: PatchApplyStatus,
    tracker_update: TurnDiffTrackerUpdate<'_>,
    environment_id: Option<&str>,
) -> CodexResult<()> {
    let mut persistence_result = Ok(());
    let evidence_cwd = match &tracker_update {
        TurnDiffTrackerUpdate::Track { environment_id, .. } => {
            apply_patch_local_cwd(ctx, environment_id.as_deref())
        }
        TurnDiffTrackerUpdate::Invalidate | TurnDiffTrackerUpdate::None => {
            apply_patch_local_cwd(ctx, environment_id)
        }
    };
    // Remote source observations have no host-watcher freshness proof. Do not
    // invent a host mutation (or broad legacy invalidation) for their writes.
    if status != PatchApplyStatus::Declined && evidence_cwd.is_some()
        && !matches!(&tracker_update, TurnDiffTrackerUpdate::None)
    {
        let affected_paths = match &tracker_update {
            TurnDiffTrackerUpdate::Track { delta, .. } if delta.is_exact() => evidence_cwd.as_ref().map(|cwd| {
            delta.changes().iter().flat_map(|change| {
                let destination = match &change.change {
                    codex_apply_patch::AppliedPatchFileChange::Update { move_path, .. } => move_path.as_ref(),
                    _ => None,
                };
                std::iter::once(change.path.clone()).chain(destination.cloned())
            })
                .into_iter()
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        cwd.as_path().join(path)
                    }
                })
                .collect::<BTreeSet<_>>()
            }),
            _ => None,
        };
        let generation_batch = match ctx.turn_diff_tracker {
            Some(tracker) => tracker
                .lock()
                .await
                .workspace_evidence_generation_batch_for_call(ctx.call_id),
            None => None,
        };
        let mutation_deferred = generation_batch.is_some_and(|batch| {
            batch.record_mutation(
                ctx.call_id,
                evidence_cwd.as_ref().map_or_else(
                    || ctx.turn.config.cwd.clone().to_path_buf(),
                    codex_utils_absolute_path::AbsolutePathBuf::to_path_buf,
                ),
                affected_paths.clone(),
                /*observe_command_ledger*/ false,
            )
        });
        let current_workspace_identity = if affected_paths.is_some() && !mutation_deferred {
            match evidence_cwd.as_ref() {
                Some(cwd) => {
                    ctx.session
                        .services
                        .git_workspace
                        .workspace_evidence_for_turn(ctx.turn, cwd.as_path())
                        .await
                        .identity
                }
                None => None,
            }
        } else {
            None
        };
        if !mutation_deferred {
            persistence_result = ctx
                .session
                .invalidate_tool_history_source_dependencies(
                    affected_paths.as_ref(),
                    current_workspace_identity.as_ref(),
                )
                .await;
        }
    }
    ctx.session
        .emit_turn_item_completed(
            ctx.turn,
            TurnItem::FileChange(FileChangeItem {
                effects: Some(patch_effects(&tracker_update)),
                environment: ctx
                    .patch_environment
                    .map(super::super::session::turn_context::TurnEnvironment::provenance),
                id: ctx.call_id.to_string(),
                changes,
                status: Some(status),
                auto_approved: None,
                stdout: Some(stdout),
                stderr: Some(stderr),
            }),
        )
        .await;

    if let Some(tracker) = ctx.turn_diff_tracker {
        let patch_modes = match &tracker_update {
            TurnDiffTrackerUpdate::Track { delta, .. } => delta.preimage_modes().clone(),
            _ => HashMap::new(),
        };
        let (unified_diff, invalidation_warning, after_capture) = {
            let mut guard = tracker.lock().await;
            let before_revision = guard.current_mutation_revision();
            let unified_diff = match tracker_update {
                TurnDiffTrackerUpdate::Track {
                    environment_id,
                    delta,
                } => {
                    guard.set_patch_modes(
                        environment_id.as_deref().unwrap_or_default(),
                        patch_modes,
                    );
                    guard.track_delta_for_call(
                        environment_id.as_deref().unwrap_or_default(),
                        delta,
                        Some(ctx.call_id),
                    );
                    guard.take_unified_diff_if_changed()
                }
                TurnDiffTrackerUpdate::Invalidate => {
                    guard.record_invalidation_cause(serde_json::json!({
                        "call_id": ctx.call_id, "tool": "apply_patch",
                        "mutation_class": "unknown_patch_mutation",
                    }));
                    guard.record_unknown_mutation();
                    guard.take_unified_diff_if_changed()
                }
                TurnDiffTrackerUpdate::None => None,
            };
            let revision = guard.current_mutation_revision();
            let after_capture = (revision != before_revision)
                .then(|| guard.workspace_evidence_generation_batch_for_call(ctx.call_id))
                .flatten()
                .map(|batch| (batch, revision));
            (
                unified_diff,
                guard.take_invalidation_warning(),
                after_capture,
            )
        };
        if let (Some((batch, revision)), Some(cwd)) = (after_capture, evidence_cwd.as_ref()) {
            batch
                .prefetch_identity_after_mutation(ctx.session, ctx.turn, cwd.as_path(), revision)
                .await;
        }
        if let Some(message) = invalidation_warning {
            ctx.session
                .send_event(
                    ctx.turn,
                    EventMsg::Warning(codex_protocol::protocol::WarningEvent {
                        message: message.to_string(),
                    }),
                )
                .await;
        }
        if let Some(unified_diff) = unified_diff {
            ctx.session
                .send_event(ctx.turn, EventMsg::TurnDiff(TurnDiffEvent { unified_diff }))
                .await;
        }
    }
    // Preserve the committed delta and completion event even when its evidence
    // invalidation could not be made durable.
    persistence_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::make_session_and_context_with_dynamic_tools_and_rx;
    use crate::session::turn_context::TurnEnvironment;
    use crate::tools::tool_dispatch_trace::ToolDispatchTiming;
    use crate::tools::tool_dispatch_trace::scope_tool_dispatch_timing;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_exec_server::LOCAL_FS;
    use codex_protocol::AgentPath;
    use codex_protocol::ThreadId;
    use codex_protocol::error::CodexErr;
    use codex_protocol::error::SandboxErr;
    use codex_protocol::exec_output::ExecToolCallOutput;
    use codex_protocol::exec_output::StreamOutput;
    use codex_protocol::items::TurnItem;
    use codex_protocol::protocol::PatchApplyStatus;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    use codex_utils_path_uri::PathUri;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::Mutex;
    use tokio::time::Instant;

    #[tokio::test]
    async fn command_completion_preserves_patch_diff_without_attribution() {
        let (session, turn, events) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        let workspace = tempdir().unwrap();
        let cwd = AbsolutePathBuf::from_absolute_path(workspace.path()).unwrap();
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let delta = codex_apply_patch::apply_patch(
            "*** Begin Patch\n*** Add File: patch.txt\n+patch-owned\n*** End Patch",
            &PathUri::from_abs_path(&cwd), &mut Vec::new(), &mut Vec::new(),
            LOCAL_FS.as_ref(), None,
        ).await.unwrap();
        tracker.lock().await.track_delta(codex_exec_server::LOCAL_ENVIRONMENT_ID, &delta);
        let patch_diff = tracker.lock().await.get_unified_diff();
        let emitter = ToolEmitter::shell(
            vec!["arbitrary-command".into()],
            cwd,
            ExecCommandSource::Agent,
            codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
        );
        let before = session.services.git_workspace.workspace_evidence_capture_sequence();
        let ctx = ToolEventCtx::new(&session, &turn, "command", Some(&tracker));
        emitter.begin(ctx).await.unwrap();
        emitter.finish(ctx, Ok(ExecToolCallOutput::default()), None).await.unwrap();
        let tracker = tracker.lock().await;
        assert_eq!(tracker.current_mutation_revision(), 2);
        assert_eq!(tracker.changed_paths().len(), 1);
        assert!(!tracker.has_untracked_changes());
        assert_eq!(tracker.get_unified_diff(), patch_diff);
        assert_eq!(session.services.git_workspace.workspace_evidence_capture_sequence(), before);
        let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert!(emitted.iter().any(|event| matches!(event.msg, EventMsg::ItemCompleted(_))));
        assert!(!emitted.iter().any(|event| matches!(event.msg, EventMsg::Warning(_) | EventMsg::TurnDiff(_))));
    }

    #[tokio::test]
    async fn uncertain_commands_invalidate_without_workspace_scans() {
        for git in [false, true] {
            let temp = tempdir().unwrap();
            if git { initialize_git_repository(temp.path()); }
            let (session, mut turn, _events) =
                make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
            set_turn_environments(&mut turn, &[(codex_exec_server::LOCAL_ENVIRONMENT_ID, temp.path())]);
            let cache = &session.services.git_workspace;
            let before = cache.workspace_evidence_capture_sequence();
            let emitter = ToolEmitter::shell(
                vec!["custom-mutator".into()],
                AbsolutePathBuf::from_absolute_path(temp.path()).unwrap(),
                ExecCommandSource::Agent,
                codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
            );
            let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
            let ctx = ToolEventCtx::new(&session, &turn, "uncertain", Some(&tracker));
            emitter.begin(ctx).await.unwrap();
            std::fs::write(temp.path().join("external.txt"), "command or concurrent writer").unwrap();
            emitter.finish(ctx, Ok(ExecToolCallOutput::default()), None).await.unwrap();
            assert_eq!(cache.workspace_evidence_capture_sequence(), before,
                "neither command boundary may scan Git or a non-Git tree");
            let tracker = tracker.lock().await;
            assert_eq!(tracker.current_mutation_revision(), 1);
            assert!(!tracker.has_untracked_changes());
        }
    }

    #[tokio::test]
    async fn nested_command_begin_event_carries_canonical_parent_and_execution_ids() {
        let (session, turn, rx_event) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        let dir = tempdir().expect("tempdir");
        let cwd = PathUri::from_host_native_path(dir.path()).expect("absolute cwd");
        let source = ToolCallSource::CodeMode {
            cell_id: "cell-7".to_string(),
            parent_call_id: Some("outer-exec-3".to_string()),
            runtime_tool_call_id: "runtime-call-11".to_string(),
            nested_deadline: None,
            cancellation_cause: None,
        };
        let timing = Arc::new(ToolDispatchTiming::new(Instant::now(), /*eager*/ true));
        let execution_id = timing.execution_id().0.clone();

        scope_tool_dispatch_timing(Arc::clone(&timing), async {
            emit_exec_command_begin(
                ToolEventCtx::new(session.as_ref(), turn.as_ref(), "nested-command-5", None)
                    .with_call_source(&source),
                &["rg".to_string(), "needle".to_string()],
                &cwd,
                &[],
                ExecCommandSource::Agent,
                None,
                None,
            )
            .await;
        })
        .await;

        let event = rx_event.recv().await.expect("item started event");
        let EventMsg::ItemStarted(event) = event.msg else {
            panic!("expected item started event");
        };
        let TurnItem::CommandExecution(item) = event.item else {
            panic!("expected command execution item");
        };
        assert_eq!(item.id, "nested-command-5");
        assert_eq!(item.parent_call_id.as_deref(), Some("outer-exec-3"));
        assert_eq!(item.parent_cell_id.as_deref(), Some("cell-7"));
        assert_eq!(
            item.runtime_tool_call_id.as_deref(),
            Some("runtime-call-11")
        );
        assert_eq!(item.execution_id.as_deref(), Some(execution_id.as_str()));
    }

    async fn enable_typed_task_for_repo(
        session: &mut Arc<Session>,
        turn: &mut Arc<TurnContext>,
        repo: &Path,
        write_paths: &[&str],
    ) -> (
        codex_agent_task_store::AttemptId,
        Arc<codex_agent_task_store::LocalAgentTaskStore>,
    ) {
        let root_session_id = "events-root".to_string();
        let state_runtime = codex_state::StateRuntime::init(
            repo.join(".typed-task-home"),
            "test-provider".to_string(),
        )
        .await
        .expect("typed task state initializes");
        let agent_control = &Arc::get_mut(session)
            .expect("single session reference")
            .services
            .agent_control;
        let coordinator = agent_control.task_coordinator();
        coordinator
            .initialize(state_runtime, root_session_id.clone())
            .await
            .expect("typed task coordinator initializes");
        let (assignment, attempt) = coordinator
            .create_assignment(
                repo,
                codex_agent_task_store::AssignmentDraft {
                    root_session_id: root_session_id.clone(),
                    admission_origin: codex_agent_task_store::AssignmentAdmissionOrigin::Typed,
                    role: codex_agent_task_store::AgentRole::Worker,
                    capability_profile:
                        codex_agent_task_store::CapabilityProfile::ScopedSourceWrite,
                    objective: "exercise command mutation evidence".to_string(),
                    acceptance_criteria: vec![codex_agent_task_store::AcceptanceCriterion {
                        evidence_policy: None,
                        id: "mutation-evidence".to_string(),
                        text: "command mutation evidence is finalized".to_string(),
                    }],
                    read_scope: Vec::new(),
                    write_scope: write_paths
                        .iter()
                        .map(|path| codex_agent_task_store::RepoScope {
                            path: (*path).to_string(),
                            recursive: false,
                        })
                        .collect(),
                    stop_condition: "evidence finalized".to_string(),
                    dependencies: Vec::new(),
                    risk_hints: Vec::new(),
                    required_evidence: vec!["focused mutation test".to_string()],
                    prohibited_changes: Vec::new(),
                    contract_claims: Vec::new(),
                    workspace_strategy: codex_agent_task_store::WorkspaceStrategy::Auto,
                    relation: None,
                    architecture_contract_ref: None,
                },
            )
            .await
            .expect("typed assignment is created");
        let agent_path = AgentPath::try_from("/root/events_worker").expect("valid agent path");
        coordinator
            .bind_agent_task(codex_agent_task_store::AgentTaskBindingDraft {
                assignment_id: assignment.assignment_id,
                attempt_id: attempt.attempt_id,
                agent_path: agent_path.to_string(),
                task_name: "events_worker".to_string(),
                thread_id: None,
            })
            .await
            .expect("typed assignment is bound");
        Arc::get_mut(turn)
            .expect("single turn reference")
            .session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_path: Some(agent_path),
            agent_nickname: None,
            agent_role: Some("worker".to_string()),
        });
        (
            attempt.attempt_id,
            coordinator.store().expect("typed task store is available"),
        )
    }

    fn initialize_git_repository(path: &Path) {
        std::fs::create_dir_all(path).expect("create repository");
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(path)
            .status()
            .expect("launch git init");
        assert!(status.success(), "git init failed");
    }

    fn set_turn_environments(turn: &mut Arc<TurnContext>, environments: &[(&str, &Path)]) {
        let turn = Arc::get_mut(turn).expect("single turn reference");
        let template = turn
            .environments
            .primary()
            .expect("primary environment")
            .clone();
        turn.environments.turn_environments = environments
            .iter()
            .map(|(environment_id, cwd)| {
                let cwd = AbsolutePathBuf::from_absolute_path(cwd).expect("absolute cwd");
                TurnEnvironment::new(
                    (*environment_id).to_string(),
                    Arc::clone(&template.environment),
                    PathUri::from_abs_path(&cwd),
                    template.shell.clone(),
                )
            })
            .collect();
    }

    async fn assert_failed_apply_patch_tracks_committed_delta(
        out: Result<ExecToolCallOutput, ToolError>,
        expected_status: PatchApplyStatus,
    ) {
        let (session, turn, rx_event) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let dir = tempdir().expect("tempdir");
        let cwd = PathUri::from_host_native_path(dir.path()).expect("absolute cwd");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let delta = codex_apply_patch::apply_patch(
            "*** Begin Patch\n*** Add File: out/dest.txt\n+after\n*** End Patch",
            &cwd,
            &mut stdout,
            &mut stderr,
            LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await
        .expect("apply patch");

        ToolEmitter::ApplyPatch {
            changes: HashMap::new(),
            auto_approved: false,
            environment_id: None,
            captured_environment: None,
        }
        .finish(
            ToolEventCtx::new(session.as_ref(), turn.as_ref(), "call-id", Some(&tracker)),
            out,
            Some(&delta),
        )
        .await
        .expect_err("failed patch");

        let completed = rx_event.recv().await.expect("item completed event");
        assert!(matches!(
            completed.msg,
            EventMsg::ItemCompleted(event)
                if matches!(
                    &event.item,
                    TurnItem::FileChange(FileChangeItem {
                        status: Some(status),
                        ..
                    }) if status == &expected_status
                )
        ));

        let unified_diff = loop {
            let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
                .await
                .expect("turn diff event")
                .expect("channel open");
            if let EventMsg::TurnDiff(TurnDiffEvent { unified_diff }) = event.msg {
                break unified_diff;
            }
        };
        assert!(unified_diff.contains("out/dest.txt"));
        assert!(unified_diff.contains("+after"));
    }

    #[tokio::test]
    async fn declined_and_sandbox_denied_patches_track_committed_delta() {
        let output = ExecToolCallOutput {
            exit_code: 1,
            ..Default::default()
        };
        assert_failed_apply_patch_tracks_committed_delta(
            Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                output: Box::new(output),
                network_policy_decision: None,
            }))),
            PatchApplyStatus::Failed,
        )
        .await;
        assert_failed_apply_patch_tracks_committed_delta(
            Err(ToolError::Denied("rejected by user".to_string())),
            PatchApplyStatus::Declined,
        )
        .await;
    }

    #[tokio::test]
    async fn apply_patch_execution_error_preserves_full_diagnostics_for_model_and_event() {
        let (session, turn, rx_event) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        let message = format!(
            "failure-head\n{}\nfailure-tail",
            "diagnostic detail\n".repeat(2_000)
        );
        let error = ToolEmitter::ApplyPatch {
            changes: HashMap::new(),
            auto_approved: false,
            environment_id: None,
            captured_environment: None,
        }
        .finish(
            ToolEventCtx::new(session.as_ref(), turn.as_ref(), "call-id", None),
            Err(ToolError::Codex(CodexErr::InvalidRequest(message.clone()))),
            None,
        )
        .await
        .expect_err("execution failure should be returned to the model");
        let FunctionCallError::RespondToModel(text) = error else {
            panic!("expected a model-visible execution error");
        };
        assert!(text.starts_with("execution error: failure-head\n"));
        assert!(text.ends_with("failure-tail"));
        assert_eq!(text, format!("execution error: {message}"));
        assert!(!text.contains("InvalidRequest("));
        assert!(!text.contains("\\n"));
        let completed = rx_event.recv().await.expect("item completed event");
        assert!(matches!(
            completed.msg,
            EventMsg::ItemCompleted(event)
                if matches!(
                    &event.item,
                    TurnItem::FileChange(FileChangeItem {
                        status: Some(PatchApplyStatus::Failed),
                        stderr: Some(stderr),
                        ..
                    }) if stderr == &text
                )
        ));
    }

    #[tokio::test]
    async fn operational_apply_patch_rejection_is_reported_as_failed() {
        let (session, turn, rx_event) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        let message = "apply_patch: the typed task store is unavailable";

        let error = ToolEmitter::ApplyPatch {
            changes: HashMap::new(),
            auto_approved: false,
            environment_id: None,
            captured_environment: None,
        }
        .finish(
            ToolEventCtx::new(session.as_ref(), turn.as_ref(), "call-id", None),
            Err(ToolError::Rejected(message.to_string())),
            None,
        )
        .await
        .expect_err("operational failure should be returned to the model");
        assert!(matches!(
            error,
            FunctionCallError::RespondToModel(text) if text == message
        ));

        let completed = rx_event.recv().await.expect("item completed event");
        assert!(matches!(
            completed.msg,
            EventMsg::ItemCompleted(event)
                if matches!(
                    &event.item,
                    TurnItem::FileChange(FileChangeItem {
                        status: Some(PatchApplyStatus::Failed),
                        stderr: Some(stderr),
                        ..
                    }) if stderr == message
                )
        ));
    }

    #[tokio::test]
    async fn mutation_boundary_declined_command_does_not_advance_tracker() {
        let (session, turn, rx_event) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        let dir = tempdir().expect("tempdir");
        let cwd = AbsolutePathBuf::from_absolute_path(dir.path()).expect("absolute cwd");
        let emitter = ToolEmitter::shell(
            vec!["rm".to_string(), "protected.txt".to_string()],
            cwd,
            ExecCommandSource::Agent,
            String::new(),
        );
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let rejection = format!(
            "rejection-head\n{}\nrejection-tail",
            "denied-🙂-".repeat(2_000)
        );

        let error = emitter
            .finish(
                ToolEventCtx::new(session.as_ref(), turn.as_ref(), "call-id", Some(&tracker)),
                Err(ToolError::Denied(rejection.clone())),
                None,
            )
            .await
            .expect_err("rejection should be returned to the model");
        let FunctionCallError::RespondToModel(model_text) = error else {
            panic!("expected structured model-visible denial");
        };

        assert!(model_text.len() < rejection.len());
        assert!(model_text.starts_with("rejection-head"));
        assert!(model_text.ends_with("rejection-tail"));
        assert_eq!(model_text.matches("tokens truncated").count(), 1);
        assert!(model_text.len() <= REJECTION_OUTPUT_MAX_TOKENS * 4 + 128);

        let completed = rx_event.recv().await.expect("item completed event");
        let EventMsg::ItemCompleted(event) = completed.msg else {
            panic!("expected item completed event");
        };
        let TurnItem::CommandExecution(item) = event.item else {
            panic!("expected command execution item");
        };
        assert_eq!(item.status, CommandExecutionStatus::Declined);
        assert_eq!(item.stderr.as_deref(), Some(model_text.as_str()));
        assert_eq!(item.aggregated_output.as_deref(), Some(model_text.as_str()));
        assert_eq!(item.formatted_output.as_deref(), Some(model_text.as_str()));
        assert_eq!(tracker.lock().await.current_mutation_revision(), 0);
    }

    #[tokio::test]
    async fn completed_item_reuses_the_model_output_projection() {
        let (session, turn, rx_event) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        let dir = tempdir().expect("tempdir");
        let cwd = AbsolutePathBuf::from_absolute_path(dir.path()).expect("absolute cwd");
        let raw_output = (0..900)
            .map(|index| {
                if index == 420 {
                    "error[E0599]: no method named `repair_bug` found".to_string()
                } else {
                    format!("build log {index}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let output = ExecToolCallOutput {
            exit_code: 101,
            stdout: StreamOutput::new(String::new()),
            stderr: StreamOutput::new(raw_output.clone()),
            aggregated_output: StreamOutput::new(raw_output.clone()),
            duration: Duration::from_millis(25),
            timed_out: false,
        };
        let emitter = ToolEmitter::shell(
            vec![
                "powershell.exe".to_string(),
                "-EncodedCommand".to_string(),
                "opaque-payload".to_string(),
            ],
            cwd,
            ExecCommandSource::Agent,
            codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
        )
        .with_model_command_text("cargo test -p codex-core focused_contract".to_string());

        let error = emitter
            .finish(
                ToolEventCtx::new(session.as_ref(), turn.as_ref(), "call-id", None),
                Ok(output),
                None,
            )
            .await
            .expect_err("nonzero output is returned to the model");
        let FunctionCallError::RespondToModel(model_text) = error else {
            panic!("expected model-visible command failure");
        };
        assert!(model_text.contains("error[E0599]: no method named `repair_bug` found"));
        assert!(model_text.contains("build log 899"));

        let completed = rx_event.recv().await.expect("item completed event");
        let EventMsg::ItemCompleted(event) = completed.msg else {
            panic!("expected item completed event");
        };
        let TurnItem::CommandExecution(item) = event.item else {
            panic!("expected command execution item");
        };
        assert_eq!(item.formatted_output.as_deref(), Some(model_text.as_str()));
        assert_eq!(item.aggregated_output.as_deref(), Some(raw_output.as_str()));
    }





    #[tokio::test]
    async fn unknown_path_writer_with_git_equality_retains_uncertainty() {
        let temp = tempdir().unwrap();
        let repo = temp.path().join("repo");
        initialize_git_repository(&repo);
        let (session, mut turn, _events) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        set_turn_environments(
            &mut turn,
            &[(codex_exec_server::LOCAL_ENVIRONMENT_ID, repo.as_path())],
        );
        let command = vec![
            "bash".into(),
            "-c".into(),
            "echo report > /tmp/report.txt".into(),
        ];
        assert!(matches!(
            crate::turn_diff_tracker::command_mutation(&command, Some(&repo)),
            crate::turn_diff_tracker::CommandMutation::KnownMutation { paths: None }
        ));
        let emitter = ToolEmitter::shell(
            command,
            AbsolutePathBuf::from_absolute_path(&repo).unwrap(),
            ExecCommandSource::Agent,
            codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
        );
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        emitter
            .begin(ToolEventCtx::new(
                session.as_ref(),
                turn.as_ref(),
                "unchanged-writer",
                Some(&tracker),
            ))
            .await
            .unwrap();
        // The command's only effect is outside the repository.
        std::fs::write(temp.path().join("report.txt"), "report").unwrap();
        emitter
            .finish(
                ToolEventCtx::new(
                    session.as_ref(),
                    turn.as_ref(),
                    "unchanged-writer",
                    Some(&tracker),
                ),
                Ok(ExecToolCallOutput::default()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(tracker.lock().await.current_mutation_revision(), 1);
    }

    #[tokio::test]
    async fn git_unchanged_unknown_command_conservatively_invalidates_evidence() {
        let temp = tempdir().unwrap();
        let repo = temp.path().join("repo");
        initialize_git_repository(&repo);
        std::fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
        let (session, mut turn, _events) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        set_turn_environments(
            &mut turn,
            &[(codex_exec_server::LOCAL_ENVIRONMENT_ID, repo.as_path())],
        );
        let observation = crate::tool_history::WorkspaceEvidenceObservation::from_response_item(
            None,
            &codex_protocol::models::ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "earlier-read".to_string(),
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                    "before the command".to_string(),
                ),
                internal_chat_message_metadata_passthrough: None,
            },
            BTreeSet::new(),
        )
        .expect("workspace observation");
        session.register_workspace_evidence(observation, ()).await;
        let epoch_before = session
            .services
            .command_execution
            .observe_repository_revision(&turn.sub_id, 0)
            .await;
        let command = vec!["unknown-writer".to_string()];
        assert_eq!(
            crate::turn_diff_tracker::command_mutation(&command, Some(&repo)),
            crate::turn_diff_tracker::CommandMutation::Uncertain
        );
        let emitter = ToolEmitter::shell(
            command,
            AbsolutePathBuf::from_absolute_path(&repo).unwrap(),
            ExecCommandSource::Agent,
            codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
        );
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let ctx = ToolEventCtx::new(
            session.as_ref(),
            turn.as_ref(),
            "ignored-writer",
            Some(&tracker),
        );
        emitter.begin(ctx).await.unwrap();
        // The command's only effect is an ignored file, invisible to Git.
        std::fs::write(repo.join("ignored.txt"), "generated").unwrap();
        emitter
            .finish(ctx, Ok(ExecToolCallOutput::default()), None)
            .await
            .unwrap();
        let mutation_revision = tracker.lock().await.current_mutation_revision();
        // Evidence stays conservative: the turn revision, the exact-retry
        // epoch and earlier workspace evidence all see a possible change.
        assert_eq!(mutation_revision, 1);
        assert_eq!(
            session
                .services
                .command_execution
                .observe_repository_revision(&turn.sub_id, mutation_revision)
                .await,
            epoch_before + 1
        );
        let live =
            serde_json::to_value(session.clone_history().await.tool_history_state()).unwrap();
        assert_eq!(
            live["workspace_evidence"]["earlier-read"]["source_dependencies_current"],
            false
        );
        // Without command scans, unchanged Git bytes do not certify validation inputs.
    }

    #[tokio::test]
    async fn git_visible_unattributed_command_change_advances_validation() {
        let temp = tempdir().unwrap();
        let repo = temp.path().join("repo");
        initialize_git_repository(&repo);
        let (session, mut turn, _events) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        set_turn_environments(
            &mut turn,
            &[(codex_exec_server::LOCAL_ENVIRONMENT_ID, repo.as_path())],
        );
        let emitter = ToolEmitter::shell(
            vec!["unknown-writer".to_string()],
            AbsolutePathBuf::from_absolute_path(&repo).unwrap(),
            ExecCommandSource::Agent,
            codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
        );
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let ctx = ToolEventCtx::new(
            session.as_ref(),
            turn.as_ref(),
            "visible-writer",
            Some(&tracker),
        );
        emitter.begin(ctx).await.unwrap();
        // Neither declared nor patched in this turn, but visible to Git.
        std::fs::write(repo.join("generated.rs"), "pub fn generated() {}\n").unwrap();
        emitter
            .finish(ctx, Ok(ExecToolCallOutput::default()), None)
            .await
            .unwrap();
        let tracker = tracker.lock().await;
        assert!(tracker.unattributed_display_paths().is_empty());
        assert_eq!(tracker.current_mutation_revision(), 1);
    }

    #[tokio::test]
    async fn non_git_unknown_command_conservatively_invalidates_evidence() {
        let workspace = tempdir().unwrap();
        std::fs::write(workspace.path().join("lib.py"), "VALUE = 1\n").unwrap();
        let (session, mut turn, _events) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        set_turn_environments(
            &mut turn,
            &[(codex_exec_server::LOCAL_ENVIRONMENT_ID, workspace.path())],
        );
        assert!(
            session
                .services
                .git_workspace
                .workspace_evidence_identity(workspace.path())
                .await
                .is_none(),
            "the fixture must be a completed capture outside any Git repository"
        );
        let observation = crate::tool_history::WorkspaceEvidenceObservation::from_response_item(
            None,
            &codex_protocol::models::ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "earlier-read".to_string(),
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                    "before the command".to_string(),
                ),
                internal_chat_message_metadata_passthrough: None,
            },
            BTreeSet::new(),
        )
        .expect("workspace observation");
        session.register_workspace_evidence(observation, ()).await;
        let epoch_before = session
            .services
            .command_execution
            .observe_repository_revision(&turn.sub_id, 0)
            .await;
        let command = vec!["unknown-writer".to_string()];
        assert_eq!(
            crate::turn_diff_tracker::command_mutation(&command, Some(workspace.path())),
            crate::turn_diff_tracker::CommandMutation::Uncertain
        );
        let emitter = ToolEmitter::shell(
            command,
            AbsolutePathBuf::from_absolute_path(workspace.path()).unwrap(),
            ExecCommandSource::Agent,
            codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
        );
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let ctx = ToolEventCtx::new(
            session.as_ref(),
            turn.as_ref(),
            "non-git-reader",
            Some(&tracker),
        );
        emitter.begin(ctx).await.unwrap();
        // The command leaves every byte of the workspace tree as it was.
        emitter
            .finish(ctx, Ok(ExecToolCallOutput::default()), None)
            .await
            .unwrap();
        let mutation_revision = tracker.lock().await.current_mutation_revision();
        // Evidence stays conservative, as it does in a repository.
        assert_eq!(mutation_revision, 1);
        assert_eq!(
            session
                .services
                .command_execution
                .observe_repository_revision(&turn.sub_id, mutation_revision)
                .await,
            epoch_before + 1
        );
        let live =
            serde_json::to_value(session.clone_history().await.tool_history_state()).unwrap();
        assert_eq!(
            live["workspace_evidence"]["earlier-read"]["source_dependencies_current"],
            false
        );
        // No tree scan attempts to prove unchanged validation inputs.
    }

    #[tokio::test]
    async fn non_git_same_size_rewrite_during_uncertain_command_advances_validation() {
        let workspace = tempdir().unwrap();
        let source = workspace.path().join("lib.py");
        std::fs::write(&source, "VALUE = 1\n").unwrap();
        let (session, mut turn, _events) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        set_turn_environments(
            &mut turn,
            &[(codex_exec_server::LOCAL_ENVIRONMENT_ID, workspace.path())],
        );
        assert!(
            session
                .services
                .git_workspace
                .workspace_evidence_identity(workspace.path())
                .await
                .is_none(),
            "the fixture must be a completed capture outside any Git repository"
        );
        let emitter = ToolEmitter::shell(
            vec!["unknown-writer".to_string()],
            AbsolutePathBuf::from_absolute_path(workspace.path()).unwrap(),
            ExecCommandSource::Agent,
            codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
        );
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let ctx = ToolEventCtx::new(
            session.as_ref(),
            turn.as_ref(),
            "non-git-writer",
            Some(&tracker),
        );
        emitter.begin(ctx).await.unwrap();
        // Neither declared nor patched. Size and modification time survive;
        // only the bytes change.
        let modified = std::fs::metadata(&source).unwrap().modified().unwrap();
        std::fs::write(&source, "VALUE = 2\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        emitter
            .finish(ctx, Ok(ExecToolCallOutput::default()), None)
            .await
            .unwrap();
        let tracker = tracker.lock().await;
        assert_eq!(tracker.current_mutation_revision(), 1);
    }









    #[tokio::test]
    async fn commands_do_not_rewrite_the_committed_patch_diff() {
        for remove in [true, false] {
            for declared in [true, false] {
                let temp = tempdir().unwrap();
                let repo = temp.path().join("repo");
                initialize_git_repository(&repo);
                std::fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
                let (session, mut turn, _events) =
                    make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
                set_turn_environments(
                    &mut turn,
                    &[(codex_exec_server::LOCAL_ENVIRONMENT_ID, repo.as_path())],
                );
                let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
                let cwd = PathUri::from_host_native_path(&repo).unwrap();
                let delta = codex_apply_patch::apply_patch(
                    "*** Begin Patch\n*** Add File: ignored.txt\n+after\n*** End Patch",
                    &cwd,
                    &mut Vec::new(),
                    &mut Vec::new(),
                    LOCAL_FS.as_ref(),
                    None,
                )
                .await
                .unwrap();
                ToolEmitter::ApplyPatch {
                    changes: HashMap::new(),
                    auto_approved: false,
                    environment_id: Some(codex_exec_server::LOCAL_ENVIRONMENT_ID.into()),
                    captured_environment: None,
                }
                .finish(
                    ToolEventCtx::new(&session, &turn, "ignored-patch", Some(&tracker)),
                    Ok(ExecToolCallOutput::default()),
                    Some(&delta),
                )
                .await
                .unwrap();
                assert!(
                    tracker
                        .lock()
                        .await
                        .exact_unified_diff()
                        .unwrap()
                        .unwrap()
                        .contains("+after")
                );
                let path = repo.join("ignored.txt");
                let command = if declared {
                    vec!["rm".into(), path.to_string_lossy().into_owned()]
                } else {
                    vec!["unknown-writer".into()]
                };
                let emitter = ToolEmitter::shell(
                    command,
                    AbsolutePathBuf::from_absolute_path(&repo).unwrap(),
                    ExecCommandSource::Agent,
                    codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
                );
                emitter
                    .begin(ToolEventCtx::new(
                        &session,
                        &turn,
                        "ignored-write",
                        Some(&tracker),
                    ))
                    .await
                    .unwrap();
                if remove {
                    std::fs::remove_file(&path).unwrap();
                } else {
                    std::fs::write(&path, "overwritten").unwrap();
                }
                emitter
                    .finish(
                        ToolEventCtx::new(&session, &turn, "ignored-write", Some(&tracker)),
                        Ok(ExecToolCallOutput::default()),
                        None,
                    )
                    .await
                    .unwrap();
                assert!(
                    tracker.lock().await.exact_unified_diff().unwrap().unwrap().contains("+after"),
                    "command effects are outside the apply-patch diff: remove={remove}, declared={declared}"
                );
            }
        }
    }

    #[tokio::test]
    async fn command_completion_invalidates_cached_identity_without_recapturing() {
        let temp = tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        initialize_git_repository(&repo);
        let (mut session, mut turn, _rx_event) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        set_turn_environments(
            &mut turn,
            &[(codex_exec_server::LOCAL_ENVIRONMENT_ID, repo.as_path())],
        );
        Arc::get_mut(&mut session)
            .expect("single session reference")
            .services
            .command_execution =
            crate::tools::command_execution::CommandExecutionLedger::load_or_new(
                temp.path().join("codex-home"),
                "workspace-reuse".to_string(),
                &repo,
            )
            .await;
        session
            .services
            .command_execution
            .observe_repository_revision("workspace-reuse-turn", 0)
            .await;
        let before_hash = session
            .services
            .command_execution
            .current_workspace_identity_hash(codex_exec_server::LOCAL_ENVIRONMENT_ID, &repo)
            .await
            .expect("initial workspace identity");
        let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("absolute cwd");
        let emitter = ToolEmitter::shell(
            vec!["custom-mutator".to_string()],
            cwd,
            ExecCommandSource::Agent,
            codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
        );
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let batch = Arc::new(crate::tools::parallel::WorkspaceEvidenceGenerationBatch::new());
        assert!(batch.register_call("uncertain-call"));
        tracker
            .lock()
            .await
            .activate_workspace_evidence_generation_batch(&batch);
        // The normal dispatcher captures this baseline under the workspace lease.
        let admitted = session
            .services
            .git_workspace
            .workspace_evidence_identity(&repo)
            .await;
        batch.record_captured_identity(&repo, 0, admitted).await;
        let capture_count_before = session
            .services
            .git_workspace
            .workspace_evidence_capture_count();

        emitter
            .begin(ToolEventCtx::new(
                session.as_ref(),
                turn.as_ref(),
                "uncertain-call",
                Some(&tracker),
            ))
            .await
            .expect("begin event should publish");
        assert_eq!(
            session
                .services
                .git_workspace
                .workspace_evidence_capture_count(),
            capture_count_before,
            "command begin must share the dispatch baseline"
        );
        tokio::fs::write(repo.join("changed.txt"), b"changed")
            .await
            .expect("mutate repository");
        emitter
            .finish(
                ToolEventCtx::new(
                    session.as_ref(),
                    turn.as_ref(),
                    "uncertain-call",
                    Some(&tracker),
                ),
                Ok(ExecToolCallOutput::default()),
                None,
            )
            .await
            .expect("successful uncertain command");

        assert_eq!(
            session
                .services
                .git_workspace
                .workspace_evidence_capture_count()
                - capture_count_before,
            0,
            "command completion must not recapture the workspace"
        );
        let after_hash = session
            .services
            .command_execution
            .current_workspace_identity_hash(codex_exec_server::LOCAL_ENVIRONMENT_ID, &repo)
            .await
            ;
        assert!(after_hash.is_none(), "the previous identity must be invalidated, not refreshed");
        assert!(!before_hash.is_empty());
        let revision = tracker.lock().await.current_mutation_revision();
        assert!(
            batch
                .captured_identity(&repo, revision)
                .await
                .flatten()
                .is_none(),
            "a new revision must not inherit the pre-command capture"
        );
    }

    #[tokio::test]
    async fn shell_and_unified_exec_do_not_capture_task_snapshots() {
        let temp = tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        initialize_git_repository(&repo);
        tokio::fs::write(repo.join("shell.txt"), "before shell")
            .await
            .expect("shell fixture");
        tokio::fs::write(repo.join("unified.txt"), "before unified")
            .await
            .expect("unified fixture");
        let (mut session, mut turn, _rx_event) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        set_turn_environments(
            &mut turn,
            &[(codex_exec_server::LOCAL_ENVIRONMENT_ID, repo.as_path())],
        );
        let (_attempt_id, store) = enable_typed_task_for_repo(
            &mut session,
            &mut turn,
            &repo,
            &["shell.txt", "unified.txt"],
        )
        .await;
        let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("absolute cwd");

        let shell = ToolEmitter::shell(
            vec!["touch".to_string(), "shell.txt".to_string()],
            cwd.clone(),
            ExecCommandSource::Agent,
            codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
        );
        shell
            .begin(ToolEventCtx::new(
                session.as_ref(),
                turn.as_ref(),
                "shell-mutation",
                None,
            ))
            .await
            .expect("begin event should publish");
        tokio::fs::write(repo.join("shell.txt"), "after shell")
            .await
            .expect("shell mutation");
        shell
            .finish(
                ToolEventCtx::new(session.as_ref(), turn.as_ref(), "shell-mutation", None),
                Ok(ExecToolCallOutput::default()),
                None,
            )
            .await
            .expect("shell mutation completes");

        let unified_command = vec!["touch".to_string(), "unified.txt".to_string()];
        ToolEmitter::unified_exec(
            &unified_command,
            PathUri::from_abs_path(&cwd),
            ExecCommandSource::Agent,
            Some("process".to_string()),
            codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
        )
        .begin(ToolEventCtx::new(
            session.as_ref(),
            turn.as_ref(),
            "unified-mutation",
            None,
        ))
        .await
        .expect("begin event should publish");
        tokio::fs::write(repo.join("unified.txt"), "after unified")
            .await
            .expect("unified mutation");
        ToolEmitter::unified_exec(
            &unified_command,
            PathUri::from_abs_path(&cwd),
            ExecCommandSource::Agent,
            Some("process".to_string()),
            codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
        )
        .finish(
            ToolEventCtx::new(session.as_ref(), turn.as_ref(), "unified-mutation", None),
            Ok(ExecToolCallOutput::default()),
            None,
        )
        .await
        .expect("unified mutation completes");

        assert_eq!(
            tokio::fs::read_to_string(repo.join("shell.txt"))
                .await
                .unwrap(),
            "after shell"
        );
        assert_eq!(
            tokio::fs::read_to_string(repo.join("unified.txt"))
                .await
                .unwrap(),
            "after unified"
        );
        assert!(
            !repo
                .join(".typed-task-home/agent-task-coordination/snapshots")
                .exists()
        );
        store.close().await;
    }

    #[tokio::test]
    async fn net_zero_patch_emits_empty_turn_diff() {
        let (session, mut turn, rx_event) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let dir = tempdir().expect("tempdir");
        set_turn_environments(
            &mut turn,
            &[(codex_exec_server::LOCAL_ENVIRONMENT_ID, dir.path())],
        );
        let cwd = PathUri::from_host_native_path(dir.path()).expect("absolute cwd");

        for (call_id, patch, change) in [
            (
                "add-file",
                "*** Begin Patch\n*** Add File: a.txt\n+one\n*** End Patch",
                FileChange::Add {
                    content: "one\n".to_string(),
                },
            ),
            (
                "delete-file",
                "*** Begin Patch\n*** Delete File: a.txt\n*** End Patch",
                FileChange::Delete {
                    content: "one\n".to_string(),
                },
            ),
        ] {
            let emitter = ToolEmitter::apply_patch_for_environment(
                HashMap::from([(dir.path().join("a.txt"), change)]),
                true,
                codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
            );
            let ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), call_id, Some(&tracker));
            emitter
                .begin(ctx)
                .await
                .expect("begin event should publish");
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let delta = codex_apply_patch::apply_patch(
                patch,
                &cwd,
                &mut stdout,
                &mut stderr,
                LOCAL_FS.as_ref(),
                None,
            )
            .await
            .expect("actual patch commits");
            emitter
                .finish(ctx, Ok(ExecToolCallOutput::default()), Some(&delta))
                .await
                .expect("patch result is model-visible");
            let events = std::iter::from_fn(|| rx_event.try_recv().ok()).collect::<Vec<_>>();
            assert_eq!(events.iter().filter(|event| matches!(&event.msg,
                EventMsg::ItemStarted(event) if matches!(&event.item, TurnItem::FileChange(item) if item.id == call_id)
            )).count(), 1);
            assert_eq!(events.iter().filter(|event| matches!(&event.msg,
                EventMsg::ItemCompleted(event) if matches!(&event.item, TurnItem::FileChange(item) if item.id == call_id && item.status == Some(PatchApplyStatus::Completed))
            )).count(), 1);
            let diffs = events
                .iter()
                .filter_map(|event| match &event.msg {
                    EventMsg::TurnDiff(event) => Some(event.unified_diff.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                diffs.len(),
                1,
                "one visible diff update per committed patch"
            );
            if call_id == "delete-file" {
                assert_eq!(diffs[0], "");
                assert!(!dir.path().join("a.txt").exists());
            } else {
                assert!(diffs[0].contains("+one"));
                assert_eq!(
                    std::fs::read(dir.path().join("a.txt")).expect("actual added file"),
                    b"one\n"
                );
            }
        }
    }

    #[tokio::test]
    async fn invalidation_emits_empty_turn_diff() {
        let (session, mut turn, rx_event) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let dir = tempdir().expect("tempdir");
        set_turn_environments(
            &mut turn,
            &[(codex_exec_server::LOCAL_ENVIRONMENT_ID, dir.path())],
        );
        let cwd = PathUri::from_host_native_path(dir.path()).expect("absolute cwd");
        let emitter = ToolEmitter::apply_patch_for_environment(
            HashMap::from([(
                dir.path().join("a.txt"),
                FileChange::Add {
                    content: "one\n".to_string(),
                },
            )]),
            true,
            codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
        );
        let ctx = ToolEventCtx::new(
            session.as_ref(),
            turn.as_ref(),
            "known-patch",
            Some(&tracker),
        );
        emitter
            .begin(ctx)
            .await
            .expect("begin event should publish");
        let delta = codex_apply_patch::apply_patch(
            "*** Begin Patch\n*** Add File: a.txt\n+one\n*** End Patch",
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            LOCAL_FS.as_ref(),
            None,
        )
        .await
        .expect("actual initial patch");
        emitter
            .finish(ctx, Ok(ExecToolCallOutput::default()), Some(&delta))
            .await
            .expect("known patch completes");
        let initial_events = std::iter::from_fn(|| rx_event.try_recv().ok()).collect::<Vec<_>>();
        assert!(initial_events.iter().any(|event| matches!(&event.msg,
            EventMsg::TurnDiff(event) if event.unified_diff.contains("+one"))));

        let unknown = ToolEmitter::apply_patch_for_environment(
            HashMap::new(),
            true,
            codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
        );
        let ctx = ToolEventCtx::new(
            session.as_ref(),
            turn.as_ref(),
            "unknown-patch",
            Some(&tracker),
        );
        unknown
            .begin(ctx)
            .await
            .expect("begin event should publish");
        unknown
            .finish(ctx, Ok(ExecToolCallOutput::default()), None)
            .await
            .expect("missing external delta still completes and invalidates");
        let events = std::iter::from_fn(|| rx_event.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(events.iter().filter(|event| matches!(&event.msg,
            EventMsg::ItemCompleted(event) if matches!(&event.item, TurnItem::FileChange(item) if item.id == "unknown-patch" && item.status == Some(PatchApplyStatus::Completed))
        )).count(), 1);
        let diffs = events
            .iter()
            .filter_map(|event| match &event.msg {
                EventMsg::TurnDiff(event) => Some(event.unified_diff.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            diffs,
            vec![""],
            "the consumer must stop displaying the now-unknown prior diff"
        );
        let warnings = events
            .iter()
            .filter_map(|event| match &event.msg {
                EventMsg::Warning(event) => Some(event.message.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("The turn diff is unavailable"));
        assert!(warnings[0].contains("unknown-patch"));
        assert!(warnings[0].contains("apply_patch"));
        assert!(warnings[0].contains("unknown_patch_mutation"));
        assert_eq!(
            std::fs::read(dir.path().join("a.txt"))
                .expect("invalidation preserves the actual file"),
            b"one\n"
        );
    }
    async fn assert_tool_history_failure_preserves_mutation_completion(apply_patch: bool) {
        let (session, mut turn, rx_event) =
            make_session_and_context_with_dynamic_tools_and_rx(Vec::new()).await;
        let workspace = tempdir().expect("workspace");
        set_turn_environments(
            &mut turn,
            &[(codex_exec_server::LOCAL_ENVIRONMENT_ID, workspace.path())],
        );
        let codex_home = &turn.config.codex_home;
        let observation = crate::tool_history::WorkspaceEvidenceObservation::from_response_item(
            None,
            &codex_protocol::models::ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "earlier-read".to_string(),
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                    "before mutation".to_string(),
                ),
                internal_chat_message_metadata_passthrough: None,
            },
            BTreeSet::new(),
        )
        .expect("workspace observation");
        session.register_workspace_evidence(observation, ()).await;
        session
            .flush_tool_history_persistence()
            .await
            .expect("initial evidence is durable");
        let history_directory = codex_home.join("tool-history");
        let saved_history = codex_home.join("saved-tool-history");
        tokio::fs::rename(&history_directory, &saved_history)
            .await
            .expect("retain durable baseline");
        tokio::fs::write(
            &history_directory,
            "blocks both journal and snapshot writes",
        )
        .await
        .expect("install real filesystem failure");

        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let cwd = AbsolutePathBuf::from_absolute_path(workspace.path()).expect("absolute cwd");
        let ctx = ToolEventCtx::new(session.as_ref(), turn.as_ref(), "mutation", Some(&tracker));
        let (emitter, delta) = if apply_patch {
            let delta = codex_apply_patch::apply_patch(
                "*** Begin Patch\n*** Add File: changed.txt\n+after\n*** End Patch",
                &PathUri::from_abs_path(&cwd),
                &mut Vec::new(),
                &mut Vec::new(),
                LOCAL_FS.as_ref(),
                None,
            )
            .await
            .expect("actual patch commits before the durability failure");
            (
                ToolEmitter::apply_patch_for_environment(
                    HashMap::from([(
                        workspace.path().join("changed.txt"),
                        FileChange::Add {
                            content: "after\n".to_string(),
                        },
                    )]),
                    true,
                    codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
                ),
                Some(delta),
            )
        } else {
            tokio::fs::write(workspace.path().join("changed.txt"), "after\n")
                .await
                .expect("command mutation is already committed");
            (
                ToolEmitter::shell(
                    vec!["touch".to_string(), "changed.txt".to_string()],
                    cwd,
                    ExecCommandSource::Agent,
                    codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
                ),
                None,
            )
        };
        emitter
            .begin(ctx)
            .await
            .expect("begin event should publish");
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            emitter.finish(
                ctx,
                Ok(ExecToolCallOutput {
                    aggregated_output: StreamOutput::new("committed outcome marker".to_string()),
                    ..Default::default()
                }),
                delta.as_ref(),
            ),
        )
        .await
        .expect("permanent persistence failure must finish promptly")
        .expect_err("a successful external mutation must not hide failed durability");
        let FunctionCallError::Fatal(message) = error else {
            panic!("durability failure must stop safely");
        };
        assert!(message.contains("Command completed with exit code 0"));
        assert!(message.contains("committed outcome marker"));
        assert!(message.contains("Do not repeat completed effects"));
        assert_eq!(
            tokio::fs::read_to_string(workspace.path().join("changed.txt"))
                .await
                .unwrap(),
            "after\n",
            "persistence failure must not pretend to roll back the external mutation",
        );
        // Normal dispatch emits each typed item followed by its legacy alias.
        // Completion has already returned, so a missing terminal event must fail
        // immediately rather than wait for an event that will never arrive.
        let mut events = std::iter::from_fn(|| rx_event.try_recv().ok());
        let started = events.next().expect("started item");
        match started.msg {
            EventMsg::ItemStarted(event) => match event.item {
                TurnItem::FileChange(item) if apply_patch => assert_eq!(item.id, "mutation"),
                TurnItem::CommandExecution(item) if !apply_patch => {
                    assert_eq!(item.id, "mutation");
                    assert_eq!(item.status, CommandExecutionStatus::InProgress);
                }
                item => panic!("unexpected started item: {item:?}"),
            },
            event => panic!("expected started item, got {event:?}"),
        }
        match events.next().expect("legacy begin event").msg {
            EventMsg::PatchApplyBegin(event) if apply_patch => {
                assert_eq!(event.call_id, "mutation");
            }
            EventMsg::ExecCommandBegin(event) if !apply_patch => {
                assert_eq!(event.call_id, "mutation");
            }
            event => panic!("unexpected legacy begin event: {event:?}"),
        }
        match events.next().expect("completed item").msg {
            EventMsg::ItemCompleted(event) => match event.item {
                TurnItem::FileChange(item) if apply_patch => {
                    assert_eq!(item.id, "mutation");
                    assert_eq!(item.status, Some(PatchApplyStatus::Completed));
                }
                TurnItem::CommandExecution(item) if !apply_patch => {
                    assert_eq!(item.id, "mutation");
                    assert_eq!(item.status, CommandExecutionStatus::Completed);
                    assert_eq!(item.exit_code, Some(0));
                }
                item => panic!("unexpected terminal item: {item:?}"),
            },
            event => panic!("expected terminal item, got {event:?}"),
        }
        match events.next().expect("legacy completion event").msg {
            EventMsg::PatchApplyEnd(event) if apply_patch => {
                assert_eq!(event.call_id, "mutation");
                assert!(event.success);
                assert_eq!(event.status, PatchApplyStatus::Completed);
            }
            EventMsg::ExecCommandEnd(event) if !apply_patch => {
                assert_eq!(event.call_id, "mutation");
                assert_eq!(event.status, ExecCommandStatus::Completed);
                assert_eq!(event.exit_code, Some(0));
            }
            event => panic!("unexpected legacy completion event: {event:?}"),
        }
        assert_eq!(tracker.lock().await.current_mutation_revision(), 1);
        if apply_patch {
            let diff = events.next().expect("committed patch diff");
            assert!(
                matches!(diff.msg, EventMsg::TurnDiff(TurnDiffEvent { unified_diff })
                if unified_diff.contains("changed.txt") && unified_diff.contains("+after"))
            );
        }
        assert!(
            events.next().is_none(),
            "completion must be emitted exactly once"
        );
        let history = session.clone_history().await;
        if !apply_patch {
            assert!(!history.raw_items().iter().any(|item| matches!(item,
                codex_protocol::models::ResponseItem::Message { role, content, .. }
                    if role == "developer" && content.iter().any(|part| matches!(part,
                        codex_protocol::models::ContentItem::InputText { text }
                            if text.contains("without fresh workspace verification"))))));
        }
        let live = serde_json::to_value(history.tool_history_state()).unwrap();
        assert_eq!(
            live["workspace_evidence"]["earlier-read"]["source_dependencies_current"],
            false
        );

        tokio::fs::remove_file(&history_directory)
            .await
            .expect("remove failure fixture");
        tokio::fs::rename(&saved_history, &history_directory)
            .await
            .expect("restore baseline");
        session
            .flush_tool_history_persistence()
            .await
            .expect("explicit checkpoint recovers");
        let (reloaded, warning) = crate::tool_history::load_tool_history_state(
            codex_home,
            &session.thread_id.to_string(),
        )
        .await
        .into_state_and_warning();
        assert_eq!(warning, None);
        let reloaded = serde_json::to_value(reloaded).unwrap();
        assert_eq!(
            reloaded["workspace_evidence"]["earlier-read"]["source_dependencies_current"],
            false
        );
    }

    #[tokio::test]
    async fn tool_history_failure_preserves_shell_and_patch_completion() {
        assert_tool_history_failure_preserves_mutation_completion(false).await;
        assert_tool_history_failure_preserves_mutation_completion(true).await;
    }
}
#[cfg(test)]
mod captured_patch_scope_tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn patch_scope_uses_the_captured_cwd_and_never_a_remote_host_alias() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let root = tempfile::tempdir().unwrap();
        let cwd = AbsolutePathBuf::from_absolute_path(root.path()).unwrap();
        let local = crate::session::turn_context::TurnEnvironment::new(
            "local".into(),
            Arc::clone(&turn.environments.primary().unwrap().environment),
            PathUri::from_abs_path(&cwd),
            None,
        );
        let emitter =
            ToolEmitter::apply_patch_for_environment(HashMap::new(), true, "local".into())
                .with_captured_environment(&local);
        let ToolEmitter::ApplyPatch {
            captured_environment: Some(captured),
            ..
        } = emitter
        else {
            panic!("patch emitter must capture its operation scope")
        };
        let mut ctx = ToolEventCtx::new(&session, &turn, "scope", None);
        ctx.patch_environment = Some(&captured);
        assert_eq!(apply_patch_local_cwd(ctx, Some("local")), Some(cwd.clone()));
        let remote = crate::session::turn_context::TurnEnvironment::new(
            "remote".into(),
            Arc::new(
                codex_exec_server::Environment::create_for_tests(Some("ws://127.0.0.1:1".into()))
                    .unwrap(),
            ),
            PathUri::from_abs_path(&cwd),
            None,
        );
        ctx.patch_environment = Some(&remote);
        assert!(apply_patch_local_cwd(ctx, Some("remote")).is_none());
    }
}

#[cfg(test)]
#[path = "events_workspace_invalidation_tests.rs"]
mod workspace_invalidation_tests;
