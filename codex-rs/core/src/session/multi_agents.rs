use crate::config::MultiAgentV2Config;
use crate::context::EffectiveMultiAgentMode;
use crate::context::TaskCapsuleFragment;
use crate::session::turn_context::TurnContext;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use std::sync::atomic::Ordering;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpawnAuthorizationDirective {
    Grant,
    Deny,
}

pub(super) fn usage_hint_text<'a>(
    turn_context: &'a TurnContext,
    session_source: &SessionSource,
) -> Option<&'a str> {
    if turn_context.multi_agent_version != MultiAgentVersion::V2 {
        return None;
    }

    let multi_agent_v2 = &turn_context.config.multi_agent_v2;
    configured_usage_hint_text_for_source(multi_agent_v2, session_source)
}

fn configured_usage_hint_text_for_source<'a>(
    multi_agent_v2: &'a MultiAgentV2Config,
    session_source: &SessionSource,
) -> Option<&'a str> {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }) => {
            multi_agent_v2.subagent_usage_hint_text.as_deref()
        }
        SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => multi_agent_v2.root_agent_usage_hint_text.as_deref(),
        SessionSource::Internal(_) | SessionSource::SubAgent(_) => None,
    }
}

pub(crate) fn effective_multi_agent_mode(
    turn_context: &TurnContext,
) -> Option<EffectiveMultiAgentMode> {
    if turn_context.multi_agent_version != MultiAgentVersion::V2 {
        return None;
    }

    // A configured hint, including an empty string, defines a custom policy. Reasoning effort
    // never changes whether additional model processes may be started.
    let multi_agent_mode = match &turn_context
        .config
        .multi_agent_v2
        .multi_agent_mode_hint_text
    {
        Some(hint_text) => EffectiveMultiAgentMode::Custom(hint_text.clone()),
        None => EffectiveMultiAgentMode::ExplicitRequestOnly,
    };

    match &turn_context.session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        | SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => Some(multi_agent_mode),
        SessionSource::Internal(_) | SessionSource::SubAgent(_) => None,
    }
}

pub(crate) fn spawn_is_authorized(turn_context: &TurnContext) -> bool {
    match effective_multi_agent_mode(turn_context) {
        Some(EffectiveMultiAgentMode::Custom(policy)) => policy
            .lines()
            .flat_map(|line| line.split(['.', ';', '\n']))
            .filter_map(parse_spawn_authorization_directive)
            .next_back()
            .is_some_and(|directive| directive == SpawnAuthorizationDirective::Grant),
        Some(EffectiveMultiAgentMode::ExplicitRequestOnly) => turn_context
            .multi_agent_spawn_authorized
            .load(Ordering::Acquire),
        None => false,
    }
}

pub(crate) fn update_spawn_authorization_from_text(turn_context: &TurnContext, text: &str) {
    let task_capsule_objective = TaskCapsuleFragment::objective_from_rendered(text);
    let authorization_text = task_capsule_objective.as_deref().unwrap_or(text);
    for directive in authorization_text
        .lines()
        .flat_map(|line| line.split(['.', ';', '\n']))
        .filter_map(parse_spawn_authorization_directive)
    {
        turn_context.multi_agent_spawn_authorized.store(
            directive == SpawnAuthorizationDirective::Grant,
            Ordering::Release,
        );
    }
}

fn parse_spawn_authorization_directive(clause: &str) -> Option<SpawnAuthorizationDirective> {
    let clause = clause.trim().to_ascii_lowercase();
    // Apostrophes inside words are not quotation delimiters: contractions can revoke
    // authorization, and possessives can appear in otherwise direct requests.
    let contains_quote = clause
        .char_indices()
        .any(|(index, character)| match character {
            '\'' => {
                !clause[..index].ends_with(char::is_alphanumeric)
                    || !clause[index + 1..].starts_with(char::is_alphanumeric)
            }
            '"' | '`' => true,
            _ => false,
        });
    if clause.is_empty() || contains_quote {
        return None;
    }
    let normalized = clause.strip_prefix("please ").unwrap_or(&clause).trim();
    // Only direct requests are permission directives. Incidental discussion of
    // delegation (including negated explanations) must never grant authority.
    let normalized = normalized
        .strip_prefix("can you ")
        .or_else(|| normalized.strip_prefix("could you "))
        .or_else(|| normalized.strip_prefix("would you "))
        .unwrap_or(normalized);
    let normalized = normalized.trim_end_matches('?');
    let (denied, body) = if let Some(body) = normalized
        .strip_prefix("do not ")
        .or_else(|| normalized.strip_prefix("don't "))
        .or_else(|| normalized.strip_prefix("dont "))
        .or_else(|| normalized.strip_prefix("never "))
        .or_else(|| normalized.strip_prefix("i do not want you to "))
        .or_else(|| normalized.strip_prefix("i don't want you to "))
    {
        (true, body)
    } else {
        (false, normalized)
    };
    let target = [
        "spawn ",
        "use ",
        "work with ",
        "parallelize with ",
        "parallelise with ",
    ]
    .into_iter()
    .find_map(|prefix| body.strip_prefix(prefix));
    let agent_target = target.is_some_and(|target| {
        let mut words = target.split_whitespace();
        let noun = words.find(|word| {
            !matches!(
                *word,
                "a" | "an"
                    | "the"
                    | "one"
                    | "two"
                    | "three"
                    | "another"
                    | "first"
                    | "second"
                    | "multiple"
                    | "several"
                    | "some"
                    | "parallel"
            ) && !word.chars().all(|c| c.is_ascii_digit())
        });
        if matches!(noun, Some("child" | "children"))
            && matches!(words.next(), Some("process" | "processes"))
        {
            return false;
        }
        matches!(
            noun,
            Some(
                "agent"
                    | "agents"
                    | "subagent"
                    | "subagents"
                    | "sub-agent"
                    | "sub-agents"
                    | "child"
                    | "children"
            )
        )
    });
    let explicit_action = agent_target
        || [
            "delegate this work to agents",
            "delegate to agents",
            "delegate this task to agents",
            "delegate work to agents",
            "delegate to subagents",
        ]
        .into_iter()
        .any(|directive| {
            body == directive
                || body
                    .strip_prefix(directive)
                    .is_some_and(|tail| tail.starts_with(' '))
        });
    explicit_action.then_some(if denied {
        SpawnAuthorizationDirective::Deny
    } else {
        SpawnAuthorizationDirective::Grant
    })
}

#[cfg(test)]
mod tests {
    use super::SpawnAuthorizationDirective;
    use super::parse_spawn_authorization_directive;
    use crate::context::ContextualUserFragment;
    use crate::context::TaskCapsuleFragment;

    #[test]
    fn direct_spawn_requests_are_authorization_directives() {
        for request in [
            "Use subagents to inspect both paths",
            "Please spawn an agent for the independent audit",
            "Spawn a child and continue",
            "Delegate this work to agents",
            "Parallelize with multiple agents",
            "Use subagents to inspect the user's code",
            "Can you use subagents?",
        ] {
            assert_eq!(
                parse_spawn_authorization_directive(request),
                Some(SpawnAuthorizationDirective::Grant),
                "{request:?}"
            );
        }
    }

    #[test]
    fn discussion_of_multi_agent_code_is_not_authorization() {
        for request in [
            "Audit multi-agent spawning behavior",
            "Explain how spawn authorization works",
            "Explain why we should not use agents",
            "Use regex to explain why not use agents",
            "Use agents.md to document the policy",
            "Spawn a process that checks agent configuration",
            "I want an explanation of when to use subagents",
            "Find checks that affect agents",
            "'Use subagents'",
            "\"Use subagents\"",
            "`Use subagents`",
            "Explain 'use subagents'",
            "Don't use 'subagents'",
        ] {
            assert_eq!(
                parse_spawn_authorization_directive(request),
                None,
                "{request:?}"
            );
        }
    }

    #[test]
    fn child_process_requests_do_not_authorize_agents() {
        for request in [
            "Spawn a child process",
            "Use child processes to run the build",
        ] {
            assert_eq!(
                parse_spawn_authorization_directive(request),
                None,
                "{request:?}"
            );
        }
    }

    #[test]
    fn explicit_denial_revokes_spawn_authority() {
        for request in [
            "Do not use subagents for this task",
            "Don't use subagents for this task",
            "Please don't use subagents for the user's task",
            "I do not want you to use subagents",
        ] {
            assert_eq!(
                parse_spawn_authorization_directive(request),
                Some(SpawnAuthorizationDirective::Deny),
                "{request:?}"
            );
        }
    }

    #[test]
    fn delegated_task_capsule_objective_is_an_authorization_directive() {
        let capsule = TaskCapsuleFragment::new(
            r#"{"schema_version":1,"objective":"spawn the second agent"}"#.to_string(),
        )
        .render();
        let objective = TaskCapsuleFragment::objective_from_rendered(&capsule)
            .expect("rendered capsule objective");

        assert_eq!(
            parse_spawn_authorization_directive(&objective),
            Some(SpawnAuthorizationDirective::Grant)
        );
    }
}
