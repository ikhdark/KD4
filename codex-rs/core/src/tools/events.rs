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
                // Normalize common denial messages for exec tools so users see
                // a clear, consistent phrase.
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


