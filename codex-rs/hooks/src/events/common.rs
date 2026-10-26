use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::formatted_truncate_text;

use crate::engine::ConfiguredHandler;
use crate::engine::dispatcher;

/// Identifies a thread-spawned subagent when a normal hook runs inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentHookContext {
    pub agent_id: String,
    pub agent_type: String,
}

#[derive(Debug, Default)]
pub struct ContextInjectingHookOutcome {
    pub hook_events: Vec<HookCompletedEvent>,
    pub should_stop: bool,
    pub stop_reason: Option<String>,
    pub additional_contexts: Vec<String>,
}

impl ContextInjectingHookOutcome {
    pub(crate) fn from_serialization_failure(hook_events: Vec<HookCompletedEvent>) -> Self {
        Self {
            hook_events,
            ..Default::default()
        }
    }
}

#[derive(Debug)]
pub struct StatelessHookOutcome {
    pub hook_events: Vec<HookCompletedEvent>,
    pub should_stop: bool,
    pub stop_reason: Option<String>,
}

pub(crate) fn join_text_chunks(chunks: Vec<String>) -> Option<String> {
    if chunks.is_empty() {
        None
    } else {
        Some(chunks.join("\n\n"))
    }
}

pub(crate) fn trimmed_non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn exit_error_message(exit_code: Option<i32>, stderr: &str) -> String {
    let message = match exit_code {
        Some(exit_code) => format!("hook exited with code {exit_code}"),
        None => "hook exited without a status code".to_string(),
    };
    match trimmed_non_empty(stderr) {
        Some(stderr) => formatted_truncate_text(
            &format!("{message}: {stderr}"),
            TruncationPolicy::Tokens(1_024),
        ),
        None => message,
    }
}

pub(crate) fn append_additional_context(
    entries: &mut Vec<HookOutputEntry>,
    additional_contexts_for_model: &mut Vec<String>,
    additional_context: String,
) {
    entries.push(HookOutputEntry {
        kind: HookOutputEntryKind::Context,
        text: additional_context.clone(),
    });
    additional_contexts_for_model.push(additional_context);
}

pub(crate) fn serialization_failure_hook_events(
    handlers: Vec<ConfiguredHandler>,
    turn_id: Option<String>,
    error_message: String,
) -> Vec<HookCompletedEvent> {
    handlers
        .into_iter()
        .map(|handler| {
            let mut run = dispatcher::running_summary(&handler);
            run.status = HookRunStatus::Failed;
            run.completed_at = Some(run.started_at);
            run.duration_ms = Some(0);
            run.entries = vec![HookOutputEntry {
                kind: HookOutputEntryKind::Error,
                text: error_message.clone(),
            }];
            HookCompletedEvent {
                turn_id: turn_id.clone(),
                run,
            }
        })
        .collect()
}

pub(crate) fn serialization_failure_hook_events_for_tool_use(
    handlers: Vec<ConfiguredHandler>,
    turn_id: Option<String>,
    error_message: String,
    tool_use_id: &str,
) -> Vec<HookCompletedEvent> {
    serialization_failure_hook_events(handlers, turn_id, error_message)
        .into_iter()
        .map(|event| hook_completed_for_tool_use(event, tool_use_id))
        .collect()
}

pub(crate) fn hook_completed_for_tool_use(
    mut event: HookCompletedEvent,
    tool_use_id: &str,
) -> HookCompletedEvent {
    event.run = hook_run_for_tool_use(event.run, tool_use_id);
    event
}

pub(crate) fn hook_run_for_tool_use(mut run: HookRunSummary, tool_use_id: &str) -> HookRunSummary {
    run.id = format!("{}:{tool_use_id}", run.id);
    run
}

pub(crate) fn matcher_pattern_for_event(
    event_name: HookEventName,
    matcher: Option<&str>,
) -> Option<&str> {
    if crate::hook_event_supports_matcher(event_name) {
        matcher
    } else {
        None
    }
}

/// A validated matcher whose compiled regex shares the handler's lifetime.
#[derive(Debug, Clone)]
pub(crate) struct HookMatcher {
    pattern: String,
    regex: Option<regex::Regex>,
    match_all: bool,
}

impl HookMatcher {
    pub(crate) fn new(pattern: &str) -> Result<Self, regex::Error> {
        let regex = if is_match_all_matcher(pattern) || is_exact_matcher(pattern) {
            None
        } else {
            Some(regex::Regex::new(pattern)?)
        };
        Ok(Self {
            pattern: pattern.to_owned(),
            regex,
            match_all: is_match_all_matcher(pattern),
        })
    }

    pub(crate) fn matches(&self, input: Option<&str>) -> bool {
        if self.match_all {
            return true;
        }
        input.is_some_and(|input| match &self.regex {
            Some(regex) => regex.is_match(input),
            None => self.pattern.split('|').any(|candidate| candidate == input),
        })
    }
}

impl PartialEq for HookMatcher {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern
    }
}

impl Eq for HookMatcher {}

pub(crate) fn matcher_inputs<'a>(
    tool_name: &'a str,
    matcher_aliases: &'a [String],
) -> Vec<&'a str> {
    // Keep the canonical name first so matcher previews and execution preserve
    // the same primary identity that hook stdin will serialize.
    std::iter::once(tool_name)
        .chain(matcher_aliases.iter().map(String::as_str))
        .collect()
}

fn is_match_all_matcher(matcher: &str) -> bool {
    matcher.is_empty() || matcher == "*"
}

fn is_exact_matcher(matcher: &str) -> bool {
    matcher
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '|')
}
