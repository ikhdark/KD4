use crate::config::MultiAgentV2Config;
use crate::context::TaskCapsuleFragment;
use crate::session::turn_context::TurnContext;
use codex_protocol::config_types::MultiAgentMode;
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

pub(crate) fn effective_multi_agent_mode(turn_context: &TurnContext) -> Option<MultiAgentMode> {
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
        Some(hint_text) => MultiAgentMode::Custom(hint_text.clone()),
        None => MultiAgentMode::ExplicitRequestOnly,
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
        Some(MultiAgentMode::Proactive) => true,
        Some(MultiAgentMode::Custom(policy)) => policy
            .lines()
            .flat_map(|line| line.split(['.', ';', '\n']))
            .filter_map(parse_spawn_authorization_directive)
            .next_back()
            .is_some_and(|directive| directive == SpawnAuthorizationDirective::Grant),
        Some(MultiAgentMode::ExplicitRequestOnly) => turn_context
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
    if clause.is_empty() || clause.contains('?') || clause.contains(['\'', '"', '`']) {
        return None;
    }
    let normalized = clause.strip_prefix("please ").unwrap_or(&clause).trim();
    let (denied, body) = if let Some(body) = normalized
        .strip_prefix("do not ")
        .or_else(|| normalized.strip_prefix("don't "))
        .or_else(|| normalized.strip_prefix("dont "))
        .or_else(|| normalized.strip_prefix("never "))
    {
        (true, body)
    } else {
        (false, normalized)
    };
    let agent_target = body.contains("agent")
        || body.contains("sub-agent")
        || body.contains("subagent")
        || body.contains("child")
        || body.contains("delegat");
    if !agent_target {
        return None;
    }
    let explicit_action = body.starts_with("spawn ")
        || body.starts_with("use ")
        || body.starts_with("delegate ")
        || body.starts_with("parallelize ")
        || body.starts_with("parallelise ")
        || body.starts_with("work with ")
        || body.contains(" spawn ")
        || body.contains(" delegate ")
        || body.contains("use subagent")
        || body.contains("use sub-agent")
        || body.contains("use agent")
        || body.contains("use multi-agent")
        || body.contains("use multi agent");
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
            "Find checks that affect agents",
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
        assert_eq!(
            parse_spawn_authorization_directive("Do not use subagents for this task"),
            Some(SpawnAuthorizationDirective::Deny)
        );
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
