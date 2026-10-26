use crate::config::MultiAgentV2Config;
use crate::context::EffectiveMultiAgentMode;
use crate::session::turn_context::TurnContext;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

/// Host-owned `read_file` locator for the configured delegation guidance.
pub(crate) const USAGE_HINT_LOCATOR: &str = "context:multi-agent";

/// Sent instead of the root guidance while delegation needs an explicit
/// request, so a session that never delegates does not resend it every step.
const DEFERRED_ROOT_USAGE_HINT_TEXT: &str = "You are `/root`, the primary agent in a team of agents collaborating to fulfill the user's goals. Agent tools (`spawn_agent`, `followup_task`, `send_message` and related tools) can delegate work when delegation is authorized; use their advertised namespace and entrypoints. Before spawning, messaging or waiting on agents, read the team guidance with `read_file(path=\"context:multi-agent\")`.";

pub(super) fn usage_hint_text<'a>(
    turn_context: &'a TurnContext,
    session_source: &SessionSource,
) -> Option<&'a str> {
    let text = full_usage_hint_text(turn_context, session_source)?;
    let root = !matches!(
        session_source,
        SessionSource::SubAgent(_) | SessionSource::Internal(_)
    );
    if root
        && effective_multi_agent_mode(turn_context)
            == Some(EffectiveMultiAgentMode::ExplicitRequestOnly)
    {
        return Some(DEFERRED_ROOT_USAGE_HINT_TEXT);
    }
    Some(text)
}

/// The configured guidance, deferred or not, that the locator serves.
pub(crate) fn full_usage_hint_text<'a>(
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
