use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;
use codex_utils_absolute_path::AbsolutePathBuf;

use super::common;
use super::common::ContextInjectingHookOutcome;
use crate::engine::CommandShell;
use crate::engine::ConfiguredHandler;
use crate::engine::ScopedRunGate;
use crate::engine::command_runner::CommandRunResult;
use crate::engine::dispatcher;
use crate::engine::output_parser;
use crate::schema::NullableString;
use crate::schema::SubagentCommandInputFields;
use crate::schema::UserPromptSubmitCommandInput;

#[derive(Debug, Clone)]
pub struct UserPromptSubmitRequest {
    pub session_id: ThreadId,
    pub turn_id: String,
    pub subagent: Option<common::SubagentHookContext>,
    pub cwd: AbsolutePathBuf,
    pub transcript_path: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub prompt: String,
}

#[derive(Debug, PartialEq, Eq)]
struct UserPromptSubmitHandlerData {
    should_stop: bool,
    stop_reason: Option<String>,
    additional_contexts_for_model: Vec<String>,
}

pub(crate) fn preview(
    handlers: &[ConfiguredHandler],
    gate: &ScopedRunGate<'_>,
) -> Vec<HookRunSummary> {
    dispatcher::select_handlers(
        handlers,
        HookEventName::UserPromptSubmit,
        /*matcher_input*/ None,
    )
    .into_iter()
    .filter(|handler| gate.admits(handler))
    .map(|handler| dispatcher::running_summary(&handler))
    .collect()
}

pub(crate) async fn run(
    handlers: &[ConfiguredHandler],
    shell: &CommandShell,
    request: UserPromptSubmitRequest,
    gate: &ScopedRunGate<'_>,
) -> ContextInjectingHookOutcome {
    let mut matched = dispatcher::select_handlers(
        handlers,
        HookEventName::UserPromptSubmit,
        /*matcher_input*/ None,
    );
    matched.retain(|handler| gate.claim(handler));
    if matched.is_empty() {
        return ContextInjectingHookOutcome::default();
    }

    let subagent = SubagentCommandInputFields::from(request.subagent.as_ref());
    let input_json = match serde_json::to_string(&UserPromptSubmitCommandInput {
        session_id: request.session_id.to_string(),
        turn_id: request.turn_id.clone(),
        agent_id: subagent.agent_id,
        agent_type: subagent.agent_type,
        transcript_path: NullableString::from_path(request.transcript_path.clone()),
        cwd: request.cwd.display().to_string(),
        hook_event_name: "UserPromptSubmit".to_string(),
        model: request.model.clone(),
        permission_mode: request.permission_mode.clone(),
        prompt: request.prompt,
    }) {
        Ok(input_json) => input_json,
        Err(error) => {
            return ContextInjectingHookOutcome::from_serialization_failure(
                common::serialization_failure_hook_events(
                    matched,
                    Some(request.turn_id),
                    format!("failed to serialize user prompt submit hook input: {error}"),
                ),
            );
        }
    };

    let results = dispatcher::execute_handlers(
        shell,
        matched,
        input_json,
        request.cwd.as_path(),
        Some(request.turn_id),
        parse_completed,
    )
    .await;

    let should_stop = results.iter().any(|result| result.data.should_stop);
    let stop_reason = results
        .iter()
        .find_map(|result| result.data.stop_reason.clone());

    let mut additional_contexts = Vec::new();
    ContextInjectingHookOutcome {
        hook_events: results
            .into_iter()
            .map(|result| {
                additional_contexts.extend(result.data.additional_contexts_for_model);
                result.completed
            })
            .collect(),
        should_stop,
        stop_reason,
        additional_contexts,
    }
}

fn parse_completed(
    handler: &ConfiguredHandler,
    run_result: CommandRunResult,
    turn_id: Option<String>,
) -> dispatcher::ParsedHandler<UserPromptSubmitHandlerData> {
    let mut entries = Vec::new();
    let mut status = HookRunStatus::Completed;
    let mut should_stop = false;
    let mut stop_reason = None;
    let mut additional_contexts_for_model = Vec::new();

    match run_result.error.as_deref() {
        Some(error) => {
            status = HookRunStatus::Failed;
            entries.push(HookOutputEntry {
                kind: HookOutputEntryKind::Error,
                text: error.to_string(),
            });
        }
        None => match run_result.exit_code {
            Some(0) => {
                let trimmed_stdout = run_result.stdout.trim();
                if trimmed_stdout.is_empty() {
                } else if let Some(parsed) =
                    output_parser::parse_user_prompt_submit(&run_result.stdout)
                {
                    if let Some(system_message) = parsed.universal.system_message {
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Warning,
                            text: system_message,
                        });
                    }
                    if parsed.invalid_block_reason.is_none()
                        && let Some(additional_context) = parsed.additional_context
                    {
                        common::append_additional_context(
                            &mut entries,
                            &mut additional_contexts_for_model,
                            additional_context,
                        );
                    }
                    let _ = parsed.universal.suppress_output;
                    if !parsed.universal.continue_processing {
                        status = HookRunStatus::Stopped;
                        should_stop = true;
                        stop_reason = parsed.universal.stop_reason.clone();
                        if let Some(stop_reason_text) = parsed.universal.stop_reason {
                            entries.push(HookOutputEntry {
                                kind: HookOutputEntryKind::Stop,
                                text: stop_reason_text,
                            });
                        }
                    } else if let Some(invalid_block_reason) = parsed.invalid_block_reason {
                        status = HookRunStatus::Failed;
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Error,
                            text: invalid_block_reason,
                        });
                    } else if parsed.should_block {
                        status = HookRunStatus::Blocked;
                        should_stop = true;
                        stop_reason = parsed.reason.clone();
                        if let Some(reason) = parsed.reason {
                            entries.push(HookOutputEntry {
                                kind: HookOutputEntryKind::Feedback,
                                text: reason,
                            });
                        }
                    }
                } else if output_parser::looks_like_json(&run_result.stdout) {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: "hook returned invalid user prompt submit JSON output".to_string(),
                    });
                } else {
                    let additional_context = trimmed_stdout.to_string();
                    common::append_additional_context(
                        &mut entries,
                        &mut additional_contexts_for_model,
                        additional_context,
                    );
                }
            }
            Some(2) => {
                if let Some(reason) = common::trimmed_non_empty(&run_result.stderr) {
                    status = HookRunStatus::Blocked;
                    should_stop = true;
                    stop_reason = Some(reason.clone());
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Feedback,
                        text: reason,
                    });
                } else {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: "UserPromptSubmit hook exited with code 2 but did not write a blocking reason to stderr".to_string(),
                    });
                }
            }
            Some(exit_code) => {
                status = HookRunStatus::Failed;
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text: common::exit_error_message(Some(exit_code), &run_result.stderr),
                });
            }
            None => {
                status = HookRunStatus::Failed;
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text: common::exit_error_message(None, &run_result.stderr),
                });
            }
        },
    }

    let completed = HookCompletedEvent {
        turn_id,
        run: dispatcher::completed_summary(handler, &run_result, status, entries),
    };

    dispatcher::ParsedHandler {
        completed,
        data: UserPromptSubmitHandlerData {
            should_stop,
            stop_reason,
            additional_contexts_for_model,
        },
        completion_order: 0,
    }
}
