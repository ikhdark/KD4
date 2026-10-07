use crate::config::MultiAgentV2Config;
use crate::context::EffectiveMultiAgentMode;
use crate::context::TaskCapsuleFragment;
use crate::session::turn_context::TurnContext;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use std::sync::atomic::Ordering;

/// The existing per-turn gate distinguishes absence from an explicit denial.
/// Source bits are recomputed from applicable instruction bodies, not accumulated.
#[derive(Debug, Default)]
pub(crate) struct SpawnAuthorization(std::sync::atomic::AtomicU8);

impl Clone for SpawnAuthorization {
    fn clone(&self) -> Self {
        Self(std::sync::atomic::AtomicU8::new(
            self.0.load(Ordering::Acquire),
        ))
    }
}

impl SpawnAuthorization {
    const USER_GRANT: u8 = 1;
    const USER_DENY: u8 = 2;
    const SOURCE_GRANT: u8 = 4;
    const SOURCE_DENY: u8 = 8;

    pub(crate) fn load(&self, ordering: Ordering) -> bool {
        let state = self.0.load(ordering);
        state & (Self::USER_DENY | Self::SOURCE_DENY) == 0
            && state & (Self::USER_GRANT | Self::SOURCE_GRANT) != 0
    }
    #[cfg(test)]
    pub(crate) fn store(&self, authorized: bool, ordering: Ordering) {
        self.0.store(u8::from(authorized), ordering);
    }
    fn user(&self, directive: SpawnAuthorizationDirective) {
        let bits = match directive {
            SpawnAuthorizationDirective::Grant => Self::USER_GRANT,
            SpawnAuthorizationDirective::Deny => Self::USER_DENY,
        };
        let _ = self
            .0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                Some((state & !3) | bits)
            });
    }
    fn denied(&self) -> bool {
        self.0.load(Ordering::Acquire) & (Self::USER_DENY | Self::SOURCE_DENY) != 0
    }
    fn sources(&self, texts: impl IntoIterator<Item = impl AsRef<str>>) {
        let mut bits = 0;
        for text in texts {
            for directive in spawn_authorization_directives(text.as_ref()) {
                match directive {
                    SpawnAuthorizationDirective::Grant => bits |= Self::SOURCE_GRANT,
                    SpawnAuthorizationDirective::Deny => bits |= Self::SOURCE_DENY,
                }
            }
        }
        let _ = self
            .0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                Some((state & 3) | bits)
            });
    }
}

pub(crate) fn refresh_instruction_authority(
    turn: &TurnContext,
    agents: Option<&crate::agents_md::LoadedAgentsMd>,
) {
    let mut texts = Vec::new();
    if let Some(agents) = agents {
        texts.push(agents.text());
    }
    if let Some(skills) = turn
        .extension_data
        .get::<codex_core_skills::injection::InjectedHostSkillPrompts>()
    {
        texts.extend(skills.admitted_instruction_bodies().map(str::to_string));
    }
    turn.multi_agent_spawn_authorized.sources(texts);
}

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
    if turn_context.multi_agent_spawn_authorized.denied() { return false; }
    match effective_multi_agent_mode(turn_context) {
        Some(EffectiveMultiAgentMode::Custom(policy)) => spawn_authorization_directives(&policy)
            .last()
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
    for directive in spawn_authorization_directives(authorization_text) {
        turn_context.multi_agent_spawn_authorized.user(directive);
    }
}

pub(crate) fn revoke_spawn_authorization_from_input(
    turn_context: &TurnContext,
    input: &[codex_protocol::user_input::UserInput],
) {
    for item in input {
        if let codex_protocol::user_input::UserInput::Text { text, .. } = item
            && spawn_authorization_directives(text)
                .any(|directive| directive == SpawnAuthorizationDirective::Deny)
        {
            turn_context
                .multi_agent_spawn_authorized
                .user(SpawnAuthorizationDirective::Deny);
        }
    }
}

fn spawn_authorization_directives(
    text: &str,
) -> impl Iterator<Item = SpawnAuthorizationDirective> + '_ {
    let mut fence: Option<(u8, usize)> = None;
    text.lines()
        .filter_map(move |line| {
            if fence.is_none() && (line.starts_with("    ") || line.starts_with('\t')) {
                return None;
            }
            let line = line.trim_start();
            if line.starts_with('>') {
                return None;
            }
            let marker = line.as_bytes().first().copied();
            let marker_len = if matches!(marker, Some(b'`' | b'~')) {
                line.bytes()
                    .take_while(|byte| Some(*byte) == marker)
                    .count()
            } else {
                0
            };
            if let Some((open_marker, open_len)) = fence {
                if marker == Some(open_marker)
                    && marker_len >= open_len
                    && line[marker_len..].trim().is_empty()
                {
                    fence = None;
                }
                return None;
            }
            // Examples are data, not permission directives. Track fences before
            // splitting clauses so their inner lines cannot grant or revoke authority.
            if marker_len >= 3 {
                fence = Some((marker.unwrap_or_default(), marker_len));
                return None;
            }
            let line = line.strip_prefix("* ").or_else(|| line.strip_prefix("- ")).unwrap_or(line);
            Some(strip_spawn_directive_emphasis(line))
        })
        .flat_map(|line| line.split(['.', ';']))
        .filter_map(parse_spawn_authorization_directive)
}

fn strip_spawn_directive_emphasis(text: &str) -> &str {
    let mut text = text.trim();
    // Formatting a direct request does not quote it. Only unwrap matching
    // emphasis around the entire text; never remove inline-code or quote marks.
    for marker in ["**", "__", "*", "_"] {
        if let Some(inner) = text
            .strip_prefix(marker)
            .and_then(|inner| inner.strip_suffix(marker))
        {
            text = inner.trim();
        }
    }
    text
}

fn parse_spawn_authorization_directive(clause: &str) -> Option<SpawnAuthorizationDirective> {
    let clause = strip_spawn_directive_emphasis(clause).to_ascii_lowercase();
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
    // "Implement subagents again" is a request to resume delegation, not to
    // implement agent support. Only accept this verb with an otherwise bare
    // agent target followed by "again"; code/feature requests remain data.
    let reimplement_target = body.strip_prefix("implement ");
    let target = [
        "spawn ",
        "use ",
        "work with ",
        "parallelize with ",
        "parallelise with ",
    ]
    .into_iter()
    .find_map(|prefix| body.strip_prefix(prefix))
    .or(reimplement_target);
    let agent_target = target.is_some_and(|target| {
        let mut words = target
            .split_whitespace()
            .map(|word| word.trim_end_matches([',', ':', '!']));
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
        let is_agent = matches!(
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
        );
        is_agent
            && (reimplement_target.is_none()
                || matches!((words.next(), words.next()), (Some("again"), None)))
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

    #[tokio::test]
    async fn applicable_instruction_authority_matches_user_authority_and_preserves_denials() {
        let (_, mut turn) = crate::session::tests::make_session_and_context().await;
        turn.multi_agent_version = codex_protocol::protocol::MultiAgentVersion::V2;
        std::sync::Arc::make_mut(&mut turn.config)
            .multi_agent_v2
            .multi_agent_mode_hint_text = None;
        let source = crate::agents_md::LoadedAgentsMd::from_text_for_testing(
            "* Use subagents for independent work.",
        );
        super::refresh_instruction_authority(&turn, Some(&source));
        assert!(super::spawn_is_authorized(&turn));
        super::update_spawn_authorization_from_text(&turn, "Do not use subagents");
        super::refresh_instruction_authority(&turn, Some(&source));
        assert!(!super::spawn_is_authorized(&turn));
        super::update_spawn_authorization_from_text(&turn, "Use subagents");
        let denied = crate::agents_md::LoadedAgentsMd::from_text_for_testing("Never use subagents");
        super::refresh_instruction_authority(&turn, Some(&denied));
        assert!(!super::spawn_is_authorized(&turn));
        let conflicting = crate::agents_md::LoadedAgentsMd::from_text_for_testing(
            "Never use subagents. Use subagents.",
        );
        super::refresh_instruction_authority(&turn, Some(&conflicting));
        assert!(!super::spawn_is_authorized(&turn));
        turn.multi_agent_spawn_authorized
            .store(false, super::Ordering::Release);
        for example in [
            "```\nUse subagents\n```",
            "`Use subagents`",
            "> Use subagents",
            "> Example. Use subagents",
            "    Use subagents",
        ] {
            let source = crate::agents_md::LoadedAgentsMd::from_text_for_testing(example);
            super::refresh_instruction_authority(&turn, Some(&source));
            assert!(!super::spawn_is_authorized(&turn));
        }
        let mut skills = codex_core_skills::injection::InjectedHostSkillPrompts::default();
        skills.record_instruction_fragment(
            "user",
            "<skill>Use subagents</skill>".to_string(),
            "Use subagents".to_string(),
        );
        turn.extension_data.insert(skills);
        super::refresh_instruction_authority(&turn, None);
        assert!(super::spawn_is_authorized(&turn));
    }

    #[tokio::test]
    async fn fenced_examples_do_not_change_spawn_authorization() {
        let (_, mut turn) = crate::session::tests::make_session_and_context().await;
        turn.multi_agent_version = codex_protocol::protocol::MultiAgentVersion::V2;
        for (text, expected) in [
            ("Explain this example:\n```text\nUse subagents\n```", false),
            ("Use subagents.\n```text\nDo not use subagents\n```", true),
            ("~~~\nUse subagents\n~~~\nDo not use subagents", false),
            ("````text\n```\nUse subagents\n````", false),
            ("```\nUse subagents", false),
            ("implement subagents again", true),
            ("**Use subagents**", true),
            ("**Use subagents.**", true),
            ("Use subagents. **Don't use subagents**", false),
            ("**Use subagents**; __Do not use subagents__", false),
            ("**\"Use subagents\"**", false),
            ("**`Use subagents`**", false),
            ("```text\n**Use subagents**\n```", false),
            ("```text\nimplement subagents again\n```", false),
            ("Implement subagents again. Don't implement subagents again", false),
            ("Don't implement subagents again. Implement subagents again", true),
            (
                "```\nDo not use subagents\n```\nPlease spawn an agent",
                true,
            ),
        ] {
            std::sync::Arc::make_mut(&mut turn.config)
                .multi_agent_v2
                .multi_agent_mode_hint_text = None;
            turn.multi_agent_spawn_authorized
                .store(false, super::Ordering::Release);
            turn.update_multi_agent_spawn_authorization(&[
                codex_protocol::user_input::UserInput::Text {
                    text: text.to_string(),
                    text_elements: Vec::new(),
                },
            ]);
            assert_eq!(
                super::spawn_is_authorized(&turn),
                expected,
                "user text: {text}"
            );

            std::sync::Arc::make_mut(&mut turn.config)
                .multi_agent_v2
                .multi_agent_mode_hint_text = Some(text.to_string());
            assert_eq!(
                super::spawn_is_authorized(&turn),
                expected,
                "custom policy: {text}"
            );
        }
    }

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
            "implement subagents again",
            "Please implement multiple sub-agents again!",
            "Could you implement subagents again?",
            "**Use subagents**",
            "__Use subagents__",
            "*Use subagents*",
            "_Use subagents_",
            "**implement subagents again**",
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
            "Implement subagents",
            "Implement subagent support again",
            "Implement agents in Rust again",
            "Implement subagents again in the spawn code",
            "Explain why we should implement subagents again",
            "\"implement subagents again\"",
            "`implement subagents again`",
            "**Explain how to use subagents**",
            "**\"Use subagents\"**",
            "**`Use subagents`**",
            "**Use subagents",
            "Use subagents**",
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
            "Implement child processes again",
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
            "Do not implement subagents again",
            "Don't implement subagents again",
            "**Don't use subagents**",
            "__Do not implement subagents again__",
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
