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
use crate::schema::PreToolUseCommandInput;
use crate::schema::SubagentCommandInputFields;

#[derive(Debug, Clone)]
pub struct PreToolUseRequest {
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
    /// Flat names of tools already dispatched earlier in this turn, in order.
    pub turn_tool_calls: Vec<String>,
}

#[derive(Debug)]
pub struct PreToolUseOutcome {
    pub hook_events: Vec<HookCompletedEvent>,
    pub should_block: bool,
    pub block_reason: Option<String>,
    pub additional_contexts: Vec<String>,
    pub updated_input: Option<Value>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PreToolUseHandlerData {
    should_block: bool,
    block_reason: Option<String>,
    additional_contexts_for_model: Vec<String>,
    updated_input: Option<Value>,
}

pub(crate) fn preview(
    handlers: &[ConfiguredHandler],
    request: &PreToolUseRequest,
    gate: &ScopedRunGate<'_>,
) -> Vec<HookRunSummary> {
    let matcher_inputs = common::matcher_inputs(&request.tool_name, &request.matcher_aliases);
    dispatcher::select_handlers_for_matcher_inputs(
        handlers,
        HookEventName::PreToolUse,
        &matcher_inputs,
    )
    .into_iter()
    .filter(|handler| gate.admits(handler))
    .map(|handler| {
        common::hook_run_for_tool_use(dispatcher::running_summary(&handler), &request.tool_use_id)
    })
    .collect()
}

/// Whether any configured handler selects this tool. Per-turn run gates are
/// ignored, so this may over-report but never misses a handler `run` executes.
pub(crate) fn has_matching_handler(
    handlers: &[ConfiguredHandler],
    tool_name: &str,
    matcher_aliases: &[String],
) -> bool {
    let matcher_inputs = common::matcher_inputs(tool_name, matcher_aliases);
    dispatcher::matching_handlers_for_matcher_inputs(
        handlers,
        HookEventName::PreToolUse,
        &matcher_inputs,
    )
    .next()
    .is_some()
}

pub(crate) async fn run(
    handlers: &[ConfiguredHandler],
    shell: &CommandShell,
    request: PreToolUseRequest,
    gate: &ScopedRunGate<'_>,
) -> PreToolUseOutcome {
    let matcher_inputs = common::matcher_inputs(&request.tool_name, &request.matcher_aliases);
    let mut matched = dispatcher::select_handlers_for_matcher_inputs(
        handlers,
        HookEventName::PreToolUse,
        &matcher_inputs,
    );
    matched.retain(|handler| gate.claim(handler));
    if matched.is_empty() {
        return PreToolUseOutcome {
            hook_events: Vec::new(),
            should_block: false,
            block_reason: None,
            additional_contexts: Vec::new(),
            updated_input: None,
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
                format!("failed to serialize pre tool use hook input: {error}"),
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
    let block_reason = results
        .iter()
        .find_map(|result| result.data.block_reason.clone());

    let updated_input = if should_block {
        None
    } else {
        configured_updated_input(&results)
    };

    let mut additional_contexts = Vec::new();
    PreToolUseOutcome {
        hook_events: results
            .into_iter()
            .map(|result| {
                additional_contexts.extend(result.data.additional_contexts_for_model);
                common::hook_completed_for_tool_use(result.completed, &tool_use_id)
            })
            .collect(),
        should_block,
        block_reason,
        additional_contexts,
        updated_input,
    }
}

/// Chooses the last configured hook that produced a rewrite.
///
/// The dispatcher keeps results in configured order while executing hooks
/// concurrently. Scheduler timing must not decide which command is executed.
fn configured_updated_input(
    results: &[dispatcher::ParsedHandler<PreToolUseHandlerData>],
) -> Option<Value> {
    results
        .iter()
        .rev()
        .find_map(|result| result.data.updated_input.clone())
}

/// Serializes command stdin for a selected `PreToolUse` hook.
///
/// Handler selection may include internal matcher aliases, but hook stdin keeps
/// the canonical `tool_name` so audit logs and downstream policy decisions stay
/// stable. Shell-like tools pass `{ "command": ... }` as `tool_input`; MCP
/// tools pass their resolved JSON arguments.
fn command_input_json(request: PreToolUseRequest) -> Result<String, serde_json::Error> {
    let subagent = SubagentCommandInputFields::from(request.subagent.as_ref());
    serde_json::to_string(&PreToolUseCommandInput {
        session_id: request.session_id.to_string(),
        turn_id: request.turn_id,
        agent_id: subagent.agent_id,
        agent_type: subagent.agent_type,
        transcript_path: crate::schema::NullableString::from_path(request.transcript_path),
        cwd: request.cwd.display().to_string(),
        hook_event_name: "PreToolUse".to_string(),
        model: request.model,
        permission_mode: request.permission_mode,
        tool_name: request.tool_name,
        mcp_server_name: request.mcp_server_name,
        mcp_tool_name: request.mcp_tool_name,
        tool_input: request.tool_input,
        tool_use_id: request.tool_use_id,
        turn_tool_calls: request.turn_tool_calls,
    })
}

fn parse_completed(
    handler: &ConfiguredHandler,
    run_result: CommandRunResult,
    turn_id: Option<String>,
) -> dispatcher::ParsedHandler<PreToolUseHandlerData> {
    let mut entries = Vec::new();
    let mut status = HookRunStatus::Completed;
    let mut should_block = false;
    let mut block_reason = None;
    let mut additional_contexts_for_model = Vec::new();
    let mut updated_input = None;

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
                } else if let Some(parsed) = output_parser::parse_pre_tool_use(&run_result.stdout) {
                    if let Some(system_message) = parsed.universal.system_message {
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Warning,
                            text: system_message,
                        });
                    }
                    if let Some(invalid_reason) = parsed.invalid_reason {
                        if parsed.should_block {
                            status = HookRunStatus::Blocked;
                            should_block = true;
                            block_reason = Some(invalid_reason.clone());
                        } else {
                            status = HookRunStatus::Failed;
                        }
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Error,
                            text: invalid_reason,
                        });
                    } else {
                        if let Some(additional_context) = parsed.additional_context {
                            common::append_additional_context(
                                &mut entries,
                                &mut additional_contexts_for_model,
                                additional_context,
                            );
                        }
                        if let Some(reason) = parsed.block_reason {
                            status = HookRunStatus::Blocked;
                            should_block = true;
                            block_reason = Some(reason.clone());
                            entries.push(HookOutputEntry {
                                kind: HookOutputEntryKind::Feedback,
                                text: reason,
                            });
                        }
                        if !should_block {
                            updated_input = parsed.updated_input;
                        }
                    }
                } else if output_parser::looks_like_json(&run_result.stdout) {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: "hook returned invalid pre-tool-use JSON output".to_string(),
                    });
                }
            }
            Some(2) => {
                if let Some(reason) = common::trimmed_non_empty(&run_result.stderr) {
                    status = HookRunStatus::Blocked;
                    should_block = true;
                    block_reason = Some(reason.clone());
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Feedback,
                        text: reason,
                    });
                } else {
                    status = HookRunStatus::Blocked;
                    should_block = true;
                    block_reason = Some(
                        "PreToolUse hook blocked execution without providing a reason".to_string(),
                    );
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: "PreToolUse hook exited with code 2 but did not write a blocking reason to stderr".to_string(),
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
        data: PreToolUseHandlerData {
            should_block,
            block_reason,
            additional_contexts_for_model,
            updated_input,
        },
        completion_order: 0,
    }
}

fn serialization_failure_outcome(hook_events: Vec<HookCompletedEvent>) -> PreToolUseOutcome {
    PreToolUseOutcome {
        hook_events,
        should_block: false,
        block_reason: None,
        additional_contexts: Vec::new(),
        updated_input: None,
    }
}
