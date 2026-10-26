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
use crate::schema::SessionStartCommandInput;
use crate::schema::SubagentStartCommandInput;

#[derive(Debug, Clone, Copy)]
pub enum SessionStartSource {
    Startup,
    Resume,
    Clear,
    Compact,
}

impl SessionStartSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Resume => "resume",
            Self::Clear => "clear",
            Self::Compact => "compact",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionStartRequest {
    pub session_id: ThreadId,
    pub cwd: AbsolutePathBuf,
    pub transcript_path: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub target: StartHookTarget,
}

impl SessionStartRequest {
    /// Returns the turn that scopes `once_per` limits: a subagent start carries
    /// its own turn, while a session start uses the dispatching turn.
    pub(crate) fn scope_turn_id<'a>(&'a self, turn_id: Option<&'a str>) -> &'a str {
        match &self.target {
            StartHookTarget::SessionStart { .. } => turn_id.unwrap_or_default(),
            StartHookTarget::SubagentStart { turn_id, .. } => turn_id,
        }
    }
}

#[derive(Debug, Clone)]
pub enum StartHookTarget {
    SessionStart {
        source: SessionStartSource,
    },
    SubagentStart {
        turn_id: String,
        agent_id: String,
        agent_type: String,
    },
}

impl StartHookTarget {
    fn event_name(&self) -> HookEventName {
        match self {
            Self::SessionStart { .. } => HookEventName::SessionStart,
            Self::SubagentStart { .. } => HookEventName::SubagentStart,
        }
    }

    fn matcher_input(&self) -> &str {
        match self {
            Self::SessionStart { source } => source.as_str(),
            Self::SubagentStart { agent_type, .. } => agent_type.as_str(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SessionStartHandlerData {
    should_stop: bool,
    stop_reason: Option<String>,
    additional_contexts_for_model: Vec<String>,
}

pub(crate) fn preview(
    handlers: &[ConfiguredHandler],
    request: &SessionStartRequest,
    gate: &ScopedRunGate<'_>,
) -> Vec<HookRunSummary> {
    dispatcher::select_handlers(
        handlers,
        request.target.event_name(),
        Some(request.target.matcher_input()),
    )
    .into_iter()
    .filter(|handler| gate.admits(handler))
    .map(|handler| dispatcher::running_summary(&handler))
    .collect()
}

pub(crate) async fn run(
    handlers: &[ConfiguredHandler],
    shell: &CommandShell,
    request: SessionStartRequest,
    turn_id: Option<String>,
    gate: &ScopedRunGate<'_>,
) -> ContextInjectingHookOutcome {
    let mut matched = dispatcher::select_handlers(
        handlers,
        request.target.event_name(),
        Some(request.target.matcher_input()),
    );
    matched.retain(|handler| gate.claim(handler));
    if matched.is_empty() {
        return ContextInjectingHookOutcome::default();
    }

    let (input_json, turn_id) = match request.target {
        StartHookTarget::SessionStart { source } => {
            let input_json = match serde_json::to_string(&SessionStartCommandInput::new(
                request.session_id.to_string(),
                request.transcript_path.clone(),
                request.cwd.display().to_string(),
                request.model.clone(),
                request.permission_mode.clone(),
                source.as_str().to_string(),
            )) {
                Ok(input_json) => input_json,
                Err(error) => {
                    return ContextInjectingHookOutcome::from_serialization_failure(
                        common::serialization_failure_hook_events(
                            matched,
                            turn_id,
                            format!("failed to serialize session start hook input: {error}"),
                        ),
                    );
                }
            };
            (input_json, turn_id)
        }
        StartHookTarget::SubagentStart {
            turn_id: subagent_turn_id,
            agent_id,
            agent_type,
        } => {
            let input = SubagentStartCommandInput {
                session_id: request.session_id.to_string(),
                turn_id: subagent_turn_id.clone(),
                transcript_path: NullableString::from_path(request.transcript_path.clone()),
                cwd: request.cwd.display().to_string(),
                hook_event_name: "SubagentStart".to_string(),
                model: request.model.clone(),
                permission_mode: request.permission_mode.clone(),
                agent_id,
                agent_type,
            };
            let input_json = match serde_json::to_string(&input) {
                Ok(input_json) => input_json,
                Err(error) => {
                    return ContextInjectingHookOutcome::from_serialization_failure(
                        common::serialization_failure_hook_events(
                            matched,
                            Some(subagent_turn_id),
                            format!("failed to serialize subagent start hook input: {error}"),
                        ),
                    );
                }
            };
            (input_json, Some(subagent_turn_id))
        }
    };

    let results = dispatcher::execute_handlers(
        shell,
        matched,
        input_json,
        request.cwd.as_path(),
        turn_id,
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

/// Interprets completed `SessionStart` and `SubagentStart` hook runs.
///
/// The two events have different input payloads but share most output
/// handling: hook JSON can emit warnings/context, invalid JSON-looking stdout
/// fails, and plain stdout becomes model context. Only `SessionStart` honors
/// `continue:false`; `SubagentStart` stays context-injection-only.
fn parse_completed(
    handler: &ConfiguredHandler,
    run_result: CommandRunResult,
    turn_id: Option<String>,
) -> dispatcher::ParsedHandler<SessionStartHandlerData> {
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
                } else if let Some(parsed) = match handler.event_name {
                    HookEventName::SessionStart => {
                        output_parser::parse_session_start(&run_result.stdout)
                    }
                    HookEventName::SubagentStart => {
                        output_parser::parse_subagent_start(&run_result.stdout)
                    }
                    event_name => {
                        panic!("expected start hook event, got {event_name:?}")
                    }
                } {
                    if let Some(system_message) = parsed.universal.system_message {
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Warning,
                            text: system_message,
                        });
                    }
                    if let Some(additional_context) = parsed.additional_context {
                        common::append_additional_context(
                            &mut entries,
                            &mut additional_contexts_for_model,
                            additional_context,
                        );
                    }
                    let _ = parsed.universal.suppress_output;
                    if handler.event_name == HookEventName::SessionStart
                        && !parsed.universal.continue_processing
                    {
                        status = HookRunStatus::Stopped;
                        should_stop = true;
                        stop_reason = parsed.universal.stop_reason.clone();
                        if let Some(stop_reason_text) = parsed.universal.stop_reason {
                            entries.push(HookOutputEntry {
                                kind: HookOutputEntryKind::Stop,
                                text: stop_reason_text,
                            });
                        }
                    }
                } else if output_parser::looks_like_json(&run_result.stdout) {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: match handler.event_name {
                            HookEventName::SessionStart => {
                                "hook returned invalid session start JSON output"
                            }
                            HookEventName::SubagentStart => {
                                "hook returned invalid subagent start JSON output"
                            }
                            event_name => {
                                panic!("expected start hook event, got {event_name:?}")
                            }
                        }
                        .to_string(),
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
        data: SessionStartHandlerData {
            should_stop,
            stop_reason,
            additional_contexts_for_model,
        },
        completion_order: 0,
    }
}
