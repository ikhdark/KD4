use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;

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

pub(crate) fn flatten_additional_contexts<'a>(
    additional_contexts: impl IntoIterator<Item = &'a [String]>,
) -> Vec<String> {
    additional_contexts
        .into_iter()
        .flat_map(|chunk| chunk.iter().cloned())
        .collect()
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
    match event_name {
        HookEventName::PreToolUse
        | HookEventName::PermissionRequest
        | HookEventName::PostToolUse
        | HookEventName::SessionStart
        | HookEventName::SubagentStart
        | HookEventName::SubagentStop
        | HookEventName::PreCompact
        | HookEventName::PostCompact => matcher,
        HookEventName::UserPromptSubmit | HookEventName::Stop | HookEventName::Interrupt => None,
    }
}

pub(crate) fn validate_matcher_pattern(matcher: &str) -> Result<(), regex::Error> {
    if is_match_all_matcher(matcher) || is_exact_matcher(matcher) {
        return Ok(());
    }
    regex::Regex::new(matcher).map(|_| ())
}

pub(crate) fn matches_matcher(matcher: Option<&str>, input: Option<&str>) -> bool {
    match matcher {
        None => true,
        Some(matcher) if is_match_all_matcher(matcher) => true,
        Some(matcher) if is_exact_matcher(matcher) => input
            .map(|input| matcher.split('|').any(|candidate| candidate == input))
            .unwrap_or(false),
        Some(matcher) => input
            .and_then(|input| {
                regex::Regex::new(matcher)
                    .ok()
                    .map(|regex| regex.is_match(input))
            })
            .unwrap_or(false),
    }
}

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

#[cfg(test)]
mod tests {
    use codex_protocol::protocol::HookEventName;
    use pretty_assertions::assert_eq;

    use super::ContextInjectingHookOutcome;
    use super::StatelessHookOutcome;
    use super::matcher_pattern_for_event;
    use super::matches_matcher;
    use super::validate_matcher_pattern;

    #[test]
    fn shared_hook_outcomes_preserve_their_payload_shapes() {
        let empty_context_injecting =
            ContextInjectingHookOutcome::from_serialization_failure(Vec::new());
        assert!(empty_context_injecting.hook_events.is_empty());
        assert!(!empty_context_injecting.should_stop);
        assert_eq!(empty_context_injecting.stop_reason, None);
        assert!(empty_context_injecting.additional_contexts.is_empty());

        let context_injecting = ContextInjectingHookOutcome {
            hook_events: Vec::new(),
            should_stop: true,
            stop_reason: Some("blocked".to_string()),
            additional_contexts: vec!["context".to_string()],
        };
        assert!(context_injecting.hook_events.is_empty());
        assert!(context_injecting.should_stop);
        assert_eq!(context_injecting.stop_reason.as_deref(), Some("blocked"));
        assert_eq!(context_injecting.additional_contexts, vec!["context"]);

        let stateless = StatelessHookOutcome {
            hook_events: Vec::new(),
            should_stop: false,
            stop_reason: None,
        };
        assert!(stateless.hook_events.is_empty());
        assert!(!stateless.should_stop);
        assert_eq!(stateless.stop_reason, None);
    }

    #[test]
    fn matcher_omitted_matches_all_occurrences() {
        assert!(matches_matcher(/*matcher*/ None, Some("Bash")));
        assert!(matches_matcher(/*matcher*/ None, Some("Write")));
    }

    #[test]
    fn matcher_star_matches_all_occurrences() {
        assert!(matches_matcher(Some("*"), Some("Bash")));
        assert!(matches_matcher(Some("*"), Some("Edit")));
        assert_eq!(validate_matcher_pattern("*"), Ok(()));
    }

    #[test]
    fn matcher_empty_string_matches_all_occurrences() {
        assert!(matches_matcher(Some(""), Some("Bash")));
        assert!(matches_matcher(Some(""), Some("SessionStart")));
        assert_eq!(validate_matcher_pattern(""), Ok(()));
    }

    #[test]
    fn exact_matcher_supports_pipe_alternatives() {
        assert!(matches_matcher(Some("Edit|Write"), Some("Edit")));
        assert!(matches_matcher(Some("Edit|Write"), Some("Write")));
        assert!(!matches_matcher(Some("Edit|Write"), Some("Bash")));
        assert_eq!(validate_matcher_pattern("Edit|Write"), Ok(()));
    }

    #[test]
    fn literal_matcher_uses_exact_matching() {
        assert!(matches_matcher(Some("Bash"), Some("Bash")));
        assert!(!matches_matcher(Some("Bash"), Some("BashOutput")));
        assert!(matches_matcher(
            Some("mcp__memory__create_entities"),
            Some("mcp__memory__create_entities")
        ));
        assert!(!matches_matcher(
            Some("mcp__memory"),
            Some("mcp__memory__create_entities")
        ));
        assert_eq!(validate_matcher_pattern("mcp__memory"), Ok(()));
    }

    #[test]
    fn matcher_uses_regex_when_it_contains_regex_characters() {
        assert!(matches_matcher(Some("^Bash"), Some("BashOutput")));
        assert_eq!(validate_matcher_pattern("^Bash"), Ok(()));
    }

    #[test]
    fn mcp_matchers_support_regex_wildcards() {
        assert!(matches_matcher(
            Some("mcp__memory__.*"),
            Some("mcp__memory__create_entities")
        ));
        assert!(matches_matcher(
            Some("mcp__.*__write.*"),
            Some("mcp__filesystem__write_file")
        ));
        assert!(!matches_matcher(
            Some("mcp__.*__write.*"),
            Some("mcp__filesystem__read_file")
        ));
        assert_eq!(validate_matcher_pattern("mcp__memory__.*"), Ok(()));
    }

    #[test]
    fn matcher_supports_anchored_regexes() {
        assert!(matches_matcher(Some("^Bash$"), Some("Bash")));
        assert!(!matches_matcher(Some("^Bash$"), Some("BashOutput")));
        assert_eq!(validate_matcher_pattern("^Bash$"), Ok(()));
    }

    #[test]
    fn invalid_regex_is_rejected() {
        assert!(validate_matcher_pattern("[").is_err());
        assert!(!matches_matcher(Some("["), Some("Bash")));
    }

    #[test]
    fn unsupported_events_ignore_matchers() {
        assert_eq!(
            matcher_pattern_for_event(HookEventName::UserPromptSubmit, Some("^hello")),
            None
        );
        assert_eq!(
            matcher_pattern_for_event(HookEventName::Stop, Some("^done$")),
            None
        );
    }

    #[test]
    fn supported_events_keep_matchers() {
        assert_eq!(
            matcher_pattern_for_event(HookEventName::PreToolUse, Some("Bash")),
            Some("Bash")
        );
        assert_eq!(
            matcher_pattern_for_event(HookEventName::PostToolUse, Some("Edit|Write")),
            Some("Edit|Write")
        );
        assert_eq!(
            matcher_pattern_for_event(HookEventName::SessionStart, Some("startup|resume")),
            Some("startup|resume")
        );
        assert_eq!(
            matcher_pattern_for_event(HookEventName::PreCompact, Some("^auto$")),
            Some("^auto$")
        );
        assert_eq!(
            matcher_pattern_for_event(HookEventName::PostCompact, Some("manual|auto")),
            Some("manual|auto")
        );
    }
}
