use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde_json::Value;

use super::common;
use crate::engine::CommandShell;
use crate::engine::ConfiguredHandler;
use crate::engine::ScopedRunGate;
use crate::engine::command_runner::CommandRunResult;
use crate::engine::dispatcher;
use crate::engine::output_parser;
use crate::schema::PostToolUseCommandInput;
use crate::schema::SubagentCommandInputFields;

#[derive(Debug, Clone)]
pub struct PostToolUseRequest {
    pub session_id: ThreadId,
    pub turn_id: String,
    pub subagent: Option<common::SubagentHookContext>,
    pub cwd: AbsolutePathBuf,
    pub transcript_path: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub tool_name: String,
    pub matcher_aliases: Vec<String>,
    pub mcp_server_name: Option<String>,
    pub mcp_tool_name: Option<String>,
    pub tool_use_id: String,
    pub tool_input: Value,
    pub tool_response: Value,
}

#[derive(Debug, Clone)]
pub struct PostToolUsePlan {
    matched: Vec<ConfiguredHandler>,
}

impl PostToolUsePlan {
    pub fn is_empty(&self) -> bool {
        self.matched.is_empty()
    }
}

#[derive(Debug)]
pub struct PostToolUseOutcome {
    pub hook_events: Vec<HookCompletedEvent>,
    pub should_block: bool,
    pub stop_reason: Option<String>,
    pub additional_contexts: Vec<String>,
    pub feedback_message: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PostToolUseHandlerData {
    should_block: bool,
    stop_reason: Option<String>,
    additional_contexts_for_model: Vec<String>,
    feedback_messages_for_model: Vec<String>,
}

pub(crate) fn plan(
    handlers: &[ConfiguredHandler],
    tool_name: &str,
    matcher_aliases: &[String],
) -> PostToolUsePlan {
    let matcher_inputs = common::matcher_inputs(tool_name, matcher_aliases);
    let matched = dispatcher::select_handlers_for_matcher_inputs(
        handlers,
        HookEventName::PostToolUse,
        &matcher_inputs,
    );
    PostToolUsePlan { matched }
}

pub(crate) fn preview(
    plan: &PostToolUsePlan,
    tool_use_id: &str,
    gate: &ScopedRunGate<'_>,
) -> Vec<HookRunSummary> {
    plan.matched
        .iter()
        .filter(|handler| gate.admits(handler))
        .map(|handler| {
            common::hook_run_for_tool_use(dispatcher::running_summary(handler), tool_use_id)
        })
        .collect()
}

pub(crate) async fn run(
    plan: PostToolUsePlan,
    shell: &CommandShell,
    request: PostToolUseRequest,
    gate: &ScopedRunGate<'_>,
) -> PostToolUseOutcome {
    let mut matched = plan.matched;
    matched.retain(|handler| gate.claim(handler));
    if matched.is_empty() {
        return PostToolUseOutcome {
            hook_events: Vec::new(),
            should_block: false,
            stop_reason: None,
            additional_contexts: Vec::new(),
            feedback_message: None,
        };
    }

    let cwd = request.cwd.clone();
    let turn_id = request.turn_id.clone();
    let tool_use_id = request.tool_use_id.clone();
    let input_json = match command_input_json(request) {
        Ok(input_json) => input_json,
        Err(error) => {
            let hook_events = common::serialization_failure_hook_events_for_tool_use(
                matched,
                Some(turn_id.clone()),
                format!("failed to serialize post tool use hook input: {error}"),
                &tool_use_id,
            );
            return serialization_failure_outcome(hook_events);
        }
    };

    let results = dispatcher::execute_handlers(
        shell,
        matched,
        input_json,
        cwd.as_path(),
        Some(turn_id.clone()),
        parse_completed,
    )
    .await;

    let should_block = results.iter().any(|result| result.data.should_block);
    let stop_reason = results
        .iter()
        .find_map(|result| result.data.stop_reason.clone());

    let mut additional_contexts = Vec::new();
    let mut feedback_messages = Vec::new();
    PostToolUseOutcome {
        hook_events: results
            .into_iter()
            .map(|result| {
                additional_contexts.extend(result.data.additional_contexts_for_model);
                feedback_messages.extend(result.data.feedback_messages_for_model);
                common::hook_completed_for_tool_use(result.completed, &tool_use_id)
            })
            .collect(),
        should_block,
        stop_reason,
        additional_contexts,
        feedback_message: common::join_text_chunks(feedback_messages),
    }
}

/// Serializes command stdin for a selected `PostToolUse` hook.
///
/// Handler selection may include internal matcher aliases, but hook stdin keeps
/// the canonical `tool_name` for logs and for consumers that pair pre/post
/// events across processes. Shell-like tools pass `{ "command": ... }` as
/// `tool_input`; MCP tools pass their resolved JSON arguments.
fn command_input_json(request: PostToolUseRequest) -> Result<String, serde_json::Error> {
    let subagent = SubagentCommandInputFields::from(request.subagent.as_ref());
    serde_json::to_string(&PostToolUseCommandInput {
        session_id: request.session_id.to_string(),
        turn_id: request.turn_id,
        agent_id: subagent.agent_id,
        agent_type: subagent.agent_type,
        transcript_path: crate::schema::NullableString::from_path(request.transcript_path),
        cwd: request.cwd.display().to_string(),
        hook_event_name: "PostToolUse".to_string(),
        model: request.model,
        permission_mode: request.permission_mode,
        tool_name: request.tool_name,
        mcp_server_name: request.mcp_server_name,
        mcp_tool_name: request.mcp_tool_name,
        tool_input: request.tool_input,
        tool_response: request.tool_response,
        tool_use_id: request.tool_use_id,
    })
}

fn parse_completed(
    handler: &ConfiguredHandler,
    run_result: CommandRunResult,
    turn_id: Option<String>,
) -> dispatcher::ParsedHandler<PostToolUseHandlerData> {
    let mut entries = Vec::new();
    let mut status = HookRunStatus::Completed;
    let mut should_block = false;
    let mut stop_reason = None;
    let mut additional_contexts_for_model = Vec::new();
    let mut feedback_messages_for_model = Vec::new();

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
                } else if let Some(parsed) = output_parser::parse_post_tool_use(&run_result.stdout)
                {
                    if let Some(system_message) = parsed.universal.system_message {
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Warning,
                            text: system_message,
                        });
                    }
                    if parsed.invalid_reason.is_none()
                        && parsed.invalid_block_reason.is_none()
                        && let Some(additional_context) = parsed.additional_context
                    {
                        common::append_additional_context(
                            &mut entries,
                            &mut additional_contexts_for_model,
                            additional_context,
                        );
                    }
                    if !parsed.universal.continue_processing {
                        status = HookRunStatus::Stopped;
                        let stop_text = parsed
                            .universal
                            .stop_reason
                            .unwrap_or_else(|| "PostToolUse hook stopped execution".to_string());
                        stop_reason = Some(stop_text.clone());
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Stop,
                            text: stop_text.clone(),
                        });
                        let model_feedback = parsed
                            .reason
                            .as_deref()
                            .and_then(common::trimmed_non_empty)
                            .unwrap_or(stop_text);
                        feedback_messages_for_model.push(model_feedback);
                    } else if let Some(invalid_reason) = parsed.invalid_reason {
                        status = HookRunStatus::Failed;
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Error,
                            text: invalid_reason,
                        });
                    } else if let Some(invalid_block_reason) = parsed.invalid_block_reason {
                        status = HookRunStatus::Failed;
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Error,
                            text: invalid_block_reason,
                        });
                    } else if parsed.should_block {
                        status = HookRunStatus::Blocked;
                        should_block = true;
                        if let Some(reason) = parsed.reason {
                            entries.push(HookOutputEntry {
                                kind: HookOutputEntryKind::Feedback,
                                text: reason.clone(),
                            });
                            feedback_messages_for_model.push(reason);
                        }
                    }
                } else if output_parser::looks_like_json(&run_result.stdout) {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: "hook returned invalid post-tool-use JSON output".to_string(),
                    });
                }
            }
            Some(2) => {
                if let Some(reason) = common::trimmed_non_empty(&run_result.stderr) {
                    status = HookRunStatus::Blocked;
                    should_block = true;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Feedback,
                        text: reason.clone(),
                    });
                    feedback_messages_for_model.push(reason);
                } else {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: "PostToolUse hook exited with code 2 but did not write feedback to stderr".to_string(),
                    });
                }
            }
            Some(exit_code) => {
                status = HookRunStatus::Failed;
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text: format!("hook exited with code {exit_code}"),
                });
            }
            None => {
                status = HookRunStatus::Failed;
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text: "hook exited without a status code".to_string(),
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
        data: PostToolUseHandlerData {
            should_block,
            stop_reason,
            additional_contexts_for_model,
            feedback_messages_for_model,
        },
        completion_order: 0,
    }
}

fn serialization_failure_outcome(hook_events: Vec<HookCompletedEvent>) -> PostToolUseOutcome {
    PostToolUseOutcome {
        hook_events,
        should_block: false,
        stop_reason: None,
        additional_contexts: Vec::new(),
        feedback_message: None,
    }
}
