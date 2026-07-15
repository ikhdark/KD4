use std::path::Path;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::FuturesUnordered;

use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookExecutionMode;
use codex_protocol::protocol::HookHandlerType;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;
use codex_protocol::protocol::HookScope;

use super::CommandShell;
use super::ConfiguredHandler;
use super::command_runner::CommandRunResult;
use super::command_runner::run_command;
use crate::events::common::CompiledMatcher;
use crate::events::common::compile_matcher_pattern;
use crate::events::common::matches_matcher;

#[derive(Clone)]
struct IndexedHandler {
    handler: Arc<ConfiguredHandler>,
    matcher: CompiledMatcher,
}

#[derive(Clone)]
pub(crate) struct HandlerIndex {
    by_event: [Arc<[IndexedHandler]>; 10],
}

impl HandlerIndex {
    pub(crate) fn empty() -> Self {
        Self {
            by_event: std::array::from_fn(|_| Arc::from([])),
        }
    }

    pub(crate) fn new(handlers: &[Arc<ConfiguredHandler>]) -> Self {
        let mut by_event: [Vec<IndexedHandler>; 10] = std::array::from_fn(|_| Vec::new());
        for handler in handlers {
            let matcher = compile_matcher_pattern(handler.matcher.as_deref())
                .expect("discovery validated supported hook matcher");
            by_event[event_slot(handler.event_name)].push(IndexedHandler {
                handler: Arc::clone(handler),
                matcher,
            });
        }
        Self {
            by_event: by_event.map(Arc::from),
        }
    }

    pub(crate) fn prepare(&self, context: HookMatchContext<'_>) -> PreparedHookPlan {
        let event_name = context.event_name();
        let handlers = self.by_event[event_slot(event_name)]
            .iter()
            .filter(|indexed| context.matches(&indexed.matcher))
            .map(|indexed| Arc::clone(&indexed.handler))
            .collect();
        PreparedHookPlan {
            event_name,
            handlers,
        }
    }
}

fn event_slot(event_name: HookEventName) -> usize {
    match event_name {
        HookEventName::PreToolUse => 0,
        HookEventName::PermissionRequest => 1,
        HookEventName::PostToolUse => 2,
        HookEventName::PreCompact => 3,
        HookEventName::PostCompact => 4,
        HookEventName::SessionStart => 5,
        HookEventName::UserPromptSubmit => 6,
        HookEventName::SubagentStart => 7,
        HookEventName::SubagentStop => 8,
        HookEventName::Stop => 9,
    }
}

#[derive(Debug, Clone, Copy)]
pub enum HookMatchContext<'a> {
    PreToolUse {
        canonical_tool_name: &'a str,
        matcher_aliases: &'a [String],
    },
    PermissionRequest {
        canonical_tool_name: &'a str,
        matcher_aliases: &'a [String],
    },
    PostToolUse {
        canonical_tool_name: &'a str,
        matcher_aliases: &'a [String],
    },
    SessionStart {
        source: &'a str,
    },
    SubagentStart {
        agent_type: &'a str,
    },
    SubagentStop {
        agent_type: &'a str,
    },
    PreCompact {
        trigger: &'a str,
    },
    PostCompact {
        trigger: &'a str,
    },
    UserPromptSubmit,
    Stop,
}

impl HookMatchContext<'_> {
    fn event_name(self) -> HookEventName {
        match self {
            Self::PreToolUse { .. } => HookEventName::PreToolUse,
            Self::PermissionRequest { .. } => HookEventName::PermissionRequest,
            Self::PostToolUse { .. } => HookEventName::PostToolUse,
            Self::SessionStart { .. } => HookEventName::SessionStart,
            Self::SubagentStart { .. } => HookEventName::SubagentStart,
            Self::SubagentStop { .. } => HookEventName::SubagentStop,
            Self::PreCompact { .. } => HookEventName::PreCompact,
            Self::PostCompact { .. } => HookEventName::PostCompact,
            Self::UserPromptSubmit => HookEventName::UserPromptSubmit,
            Self::Stop => HookEventName::Stop,
        }
    }

    fn matches(self, matcher: &CompiledMatcher) -> bool {
        match self {
            Self::PreToolUse {
                canonical_tool_name,
                matcher_aliases,
            }
            | Self::PermissionRequest {
                canonical_tool_name,
                matcher_aliases,
            }
            | Self::PostToolUse {
                canonical_tool_name,
                matcher_aliases,
            } => {
                matcher.matches_optional(Some(canonical_tool_name))
                    || matcher_aliases
                        .iter()
                        .any(|alias| matcher.matches_optional(Some(alias)))
            }
            Self::SessionStart { source } => matcher.matches_optional(Some(source)),
            Self::SubagentStart { agent_type } | Self::SubagentStop { agent_type } => {
                matcher.matches_optional(Some(agent_type))
            }
            Self::PreCompact { trigger } | Self::PostCompact { trigger } => {
                matcher.matches_optional(Some(trigger))
            }
            Self::UserPromptSubmit | Self::Stop => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PreparedHookPlan {
    event_name: HookEventName,
    handlers: Vec<Arc<ConfiguredHandler>>,
}

impl PreparedHookPlan {
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    pub fn preview(&self) -> Vec<HookRunSummary> {
        self.handlers
            .iter()
            .map(|handler| running_summary(handler))
            .collect()
    }

    pub fn preview_for_tool_use(&self, suffix: &str) -> Vec<HookRunSummary> {
        self.preview()
            .into_iter()
            .map(|run| crate::events::common::hook_run_for_tool_use(run, suffix))
            .collect()
    }

    pub(crate) fn into_handlers(self) -> Vec<Arc<ConfiguredHandler>> {
        self.handlers
    }

    pub(crate) fn event_name(&self) -> HookEventName {
        self.event_name
    }
}

#[derive(Debug)]
pub(crate) struct ParsedHandler<T> {
    pub completed: HookCompletedEvent,
    pub data: T,
    pub completion_order: usize,
}

pub(crate) fn select_handlers(
    handlers: &[ConfiguredHandler],
    event_name: HookEventName,
    matcher_input: Option<&str>,
) -> Vec<Arc<ConfiguredHandler>> {
    let matcher_inputs = matcher_input.into_iter().collect::<Vec<_>>();
    select_handlers_for_matcher_inputs(handlers, event_name, &matcher_inputs)
}

pub(crate) fn select_handlers_for_matcher_inputs(
    handlers: &[ConfiguredHandler],
    event_name: HookEventName,
    matcher_inputs: &[&str],
) -> Vec<Arc<ConfiguredHandler>> {
    // Check each configured handler once, even when several compatibility names
    // match the same regex. A hook like `apply_patch|Write|Edit` should run a
    // single time for one tool call, not once per matching alias.
    handlers
        .iter()
        .filter(|handler| handler.event_name == event_name)
        .filter(|handler| match event_name {
            HookEventName::PreToolUse
            | HookEventName::PermissionRequest
            | HookEventName::PostToolUse
            | HookEventName::SessionStart
            | HookEventName::SubagentStart
            | HookEventName::SubagentStop
            | HookEventName::PreCompact
            | HookEventName::PostCompact => {
                if matcher_inputs.is_empty() {
                    matches_matcher(handler.matcher.as_deref(), /*input*/ None)
                } else {
                    matcher_inputs
                        .iter()
                        .any(|input| matches_matcher(handler.matcher.as_deref(), Some(input)))
                }
            }
            HookEventName::UserPromptSubmit | HookEventName::Stop => true,
        })
        .cloned()
        .map(Arc::new)
        .collect()
}

pub(crate) fn running_summary(handler: &ConfiguredHandler) -> HookRunSummary {
    HookRunSummary {
        id: handler.run_id(),
        event_name: handler.event_name,
        handler_type: HookHandlerType::Command,
        execution_mode: HookExecutionMode::Sync,
        scope: scope_for_event(handler.event_name),
        source_path: handler.source_path.clone(),
        source: handler.source,
        display_order: handler.display_order,
        status: HookRunStatus::Running,
        status_message: handler.status_message.clone(),
        started_at: chrono::Utc::now().timestamp(),
        completed_at: None,
        duration_ms: None,
        entries: Vec::new(),
    }
}

pub(crate) async fn execute_handlers<T>(
    shell: &CommandShell,
    handlers: Vec<Arc<ConfiguredHandler>>,
    input_json: String,
    cwd: &Path,
    turn_id: Option<String>,
    parse: fn(&ConfiguredHandler, CommandRunResult, Option<String>) -> ParsedHandler<T>,
) -> Vec<ParsedHandler<T>> {
    let input_json: Arc<[u8]> = input_json.into_bytes().into();
    let mut pending = FuturesUnordered::new();
    for (configured_order, handler) in handlers.into_iter().enumerate() {
        let input_json = Arc::clone(&input_json);
        let turn_id = turn_id.clone();
        pending.push(async move {
            let result = run_command(shell, &handler, configured_order, &input_json, cwd).await;
            (configured_order, parse(&handler, result, turn_id))
        });
    }

    let mut completed = Vec::new();
    let mut completion_order = 0;
    while let Some((configured_order, mut parsed)) = pending.next().await {
        parsed.completion_order = completion_order;
        completion_order += 1;
        completed.push((configured_order, parsed));
    }
    completed.sort_by_key(|(configured_order, _)| *configured_order);
    completed.into_iter().map(|(_, parsed)| parsed).collect()
}

pub(crate) fn completed_summary(
    handler: &ConfiguredHandler,
    run_result: &CommandRunResult,
    status: HookRunStatus,
    entries: Vec<codex_protocol::protocol::HookOutputEntry>,
) -> HookRunSummary {
    HookRunSummary {
        id: handler.run_id(),
        event_name: handler.event_name,
        handler_type: HookHandlerType::Command,
        execution_mode: HookExecutionMode::Sync,
        scope: scope_for_event(handler.event_name),
        source_path: handler.source_path.clone(),
        source: handler.source,
        display_order: handler.display_order,
        status,
        status_message: handler.status_message.clone(),
        started_at: run_result.started_at,
        completed_at: Some(run_result.completed_at),
        duration_ms: Some(run_result.duration_ms),
        entries,
    }
}

pub(crate) fn scope_for_event(event_name: HookEventName) -> HookScope {
    match event_name {
        HookEventName::SessionStart | HookEventName::SubagentStart => HookScope::Thread,
        HookEventName::PreToolUse
        | HookEventName::PermissionRequest
        | HookEventName::PostToolUse
        | HookEventName::PreCompact
        | HookEventName::PostCompact
        | HookEventName::UserPromptSubmit
        | HookEventName::SubagentStop
        | HookEventName::Stop => HookScope::Turn,
    }
}

pub(crate) fn hook_event_name_label(event_name: HookEventName) -> &'static str {
    match event_name {
        HookEventName::PreToolUse => "PreToolUse",
        HookEventName::PermissionRequest => "PermissionRequest",
        HookEventName::PostToolUse => "PostToolUse",
        HookEventName::PreCompact => "PreCompact",
        HookEventName::PostCompact => "PostCompact",
        HookEventName::SessionStart => "SessionStart",
        HookEventName::UserPromptSubmit => "UserPromptSubmit",
        HookEventName::SubagentStart => "SubagentStart",
        HookEventName::SubagentStop => "SubagentStop",
        HookEventName::Stop => "Stop",
    }
}

pub(crate) fn hook_execution_mode_label(mode: HookExecutionMode) -> &'static str {
    match mode {
        HookExecutionMode::Sync => "sync",
        HookExecutionMode::Async => "async",
    }
}

pub(crate) fn hook_handler_type_label(handler_type: HookHandlerType) -> &'static str {
    match handler_type {
        HookHandlerType::Command => "command",
        HookHandlerType::Prompt => "prompt",
        HookHandlerType::Agent => "agent",
    }
}

pub(crate) fn hook_scope_label(scope: HookScope) -> &'static str {
    match scope {
        HookScope::Thread => "thread",
        HookScope::Turn => "turn",
    }
}

pub(crate) fn hook_source_label(source: codex_protocol::protocol::HookSource) -> &'static str {
    match source {
        codex_protocol::protocol::HookSource::System => "system",
        codex_protocol::protocol::HookSource::User => "user",
        codex_protocol::protocol::HookSource::Project => "project",
        codex_protocol::protocol::HookSource::Mdm => "mdm",
        codex_protocol::protocol::HookSource::SessionFlags => "session_flags",
        codex_protocol::protocol::HookSource::Plugin => "plugin",
        codex_protocol::protocol::HookSource::CloudRequirements => "cloud_requirements",
        codex_protocol::protocol::HookSource::CloudManagedConfig => "cloud_managed_config",
        codex_protocol::protocol::HookSource::LegacyManagedConfigFile => {
            "legacy_managed_config_file"
        }
        codex_protocol::protocol::HookSource::LegacyManagedConfigMdm => "legacy_managed_config_mdm",
        codex_protocol::protocol::HookSource::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codex_protocol::protocol::HookEventName;
    use codex_protocol::protocol::HookSource;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use pretty_assertions::assert_eq;

    use super::ConfiguredHandler;
    use super::HandlerIndex;
    use super::HookMatchContext;
    use super::select_handlers;
    use super::select_handlers_for_matcher_inputs;

    fn make_handler(
        event_name: HookEventName,
        matcher: Option<&str>,
        command: &str,
        display_order: i64,
    ) -> ConfiguredHandler {
        ConfiguredHandler {
            event_name,
            matcher: matcher.map(str::to_owned),
            command: command.to_string(),
            timeout_sec: 5,
            status_message: None,
            source_path: test_path_buf("/tmp/hooks.json").abs(),
            source: HookSource::User,
            display_order,
            env: std::collections::HashMap::new(),
        }
    }

    fn selected_orders(index: &HandlerIndex, context: HookMatchContext<'_>) -> Vec<i64> {
        index
            .prepare(context)
            .into_handlers()
            .into_iter()
            .map(|handler| handler.display_order)
            .collect()
    }

    #[test]
    fn prepared_hook_plan_matches_all_contexts_aliases_once_and_preserves_order() {
        let handlers = vec![
            Arc::new(make_handler(
                HookEventName::PreToolUse,
                Some("apply_patch|Write"),
                "pre combined",
                30,
            )),
            Arc::new(make_handler(
                HookEventName::PermissionRequest,
                Some("^Edit$"),
                "permission alias",
                40,
            )),
            Arc::new(make_handler(
                HookEventName::PostToolUse,
                Some("^Write$"),
                "post alias",
                50,
            )),
            Arc::new(make_handler(
                HookEventName::SessionStart,
                Some("^resume$"),
                "session",
                60,
            )),
            Arc::new(make_handler(
                HookEventName::SubagentStart,
                Some("^explorer$"),
                "subagent start",
                70,
            )),
            Arc::new(make_handler(
                HookEventName::SubagentStop,
                Some("^explorer$"),
                "subagent stop",
                80,
            )),
            Arc::new(make_handler(
                HookEventName::PreCompact,
                Some("^auto$"),
                "pre compact",
                90,
            )),
            Arc::new(make_handler(
                HookEventName::PostCompact,
                Some("^manual$"),
                "post compact",
                100,
            )),
            Arc::new(make_handler(
                HookEventName::UserPromptSubmit,
                Some("^never$"),
                "prompt unconditional",
                110,
            )),
            Arc::new(make_handler(
                HookEventName::Stop,
                Some("^never$"),
                "stop unconditional",
                120,
            )),
            Arc::new(make_handler(
                HookEventName::PreToolUse,
                Some("^Write$"),
                "pre alias",
                10,
            )),
            Arc::new(make_handler(
                HookEventName::PreToolUse,
                Some("^Bash$"),
                "pre mismatch",
                20,
            )),
        ];
        let index = HandlerIndex::new(&handlers);
        let aliases = vec!["Write".to_string(), "Edit".to_string()];

        assert_eq!(
            selected_orders(
                &index,
                HookMatchContext::PreToolUse {
                    canonical_tool_name: "apply_patch",
                    matcher_aliases: &aliases,
                }
            ),
            vec![30, 10]
        );
        assert_eq!(
            selected_orders(
                &index,
                HookMatchContext::PermissionRequest {
                    canonical_tool_name: "apply_patch",
                    matcher_aliases: &aliases,
                }
            ),
            vec![40]
        );
        assert_eq!(
            selected_orders(
                &index,
                HookMatchContext::PostToolUse {
                    canonical_tool_name: "apply_patch",
                    matcher_aliases: &aliases,
                }
            ),
            vec![50]
        );
        assert_eq!(
            selected_orders(&index, HookMatchContext::SessionStart { source: "resume" }),
            vec![60]
        );
        assert_eq!(
            selected_orders(
                &index,
                HookMatchContext::SubagentStart {
                    agent_type: "explorer",
                }
            ),
            vec![70]
        );
        assert_eq!(
            selected_orders(
                &index,
                HookMatchContext::SubagentStop {
                    agent_type: "explorer",
                }
            ),
            vec![80]
        );
        assert_eq!(
            selected_orders(&index, HookMatchContext::PreCompact { trigger: "auto" }),
            vec![90]
        );
        assert_eq!(
            selected_orders(&index, HookMatchContext::PostCompact { trigger: "manual" }),
            vec![100]
        );
        assert_eq!(
            selected_orders(&index, HookMatchContext::UserPromptSubmit),
            vec![110]
        );
        assert_eq!(selected_orders(&index, HookMatchContext::Stop), vec![120]);
    }

    #[test]
    fn prepared_hook_plan_no_match_is_empty() {
        let handlers = vec![Arc::new(make_handler(
            HookEventName::PreToolUse,
            Some("^Bash$"),
            "mismatch",
            0,
        ))];
        let index = HandlerIndex::new(&handlers);
        let aliases = vec!["Write".to_string(), "Edit".to_string()];

        let plan = index.prepare(HookMatchContext::PreToolUse {
            canonical_tool_name: "apply_patch",
            matcher_aliases: &aliases,
        });

        assert_eq!(plan.event_name(), HookEventName::PreToolUse);
        assert!(plan.is_empty());
    }

    #[test]
    fn select_handlers_keeps_duplicate_stop_handlers() {
        let handlers = vec![
            make_handler(
                HookEventName::Stop,
                /*matcher*/ None,
                "echo same",
                /*display_order*/ 0,
            ),
            make_handler(
                HookEventName::Stop,
                /*matcher*/ None,
                "echo same",
                /*display_order*/ 1,
            ),
        ];

        let selected = select_handlers(&handlers, HookEventName::Stop, /*matcher_input*/ None);

        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].display_order, 0);
        assert_eq!(selected[1].display_order, 1);
    }

    #[test]
    fn select_handlers_keeps_overlapping_session_start_matchers() {
        let handlers = vec![
            make_handler(
                HookEventName::SessionStart,
                Some("start.*"),
                "echo same",
                /*display_order*/ 0,
            ),
            make_handler(
                HookEventName::SessionStart,
                Some("^startup$"),
                "echo same",
                /*display_order*/ 1,
            ),
        ];

        let selected = select_handlers(&handlers, HookEventName::SessionStart, Some("startup"));

        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].display_order, 0);
        assert_eq!(selected[1].display_order, 1);
    }

    #[test]
    fn compact_hooks_match_trigger() {
        let handlers = vec![
            make_handler(
                HookEventName::PreCompact,
                Some("manual"),
                "echo manual",
                /*display_order*/ 0,
            ),
            make_handler(
                HookEventName::PreCompact,
                Some("auto"),
                "echo auto",
                /*display_order*/ 1,
            ),
        ];

        let selected = select_handlers(&handlers, HookEventName::PreCompact, Some("manual"));

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].display_order, 0);
    }

    #[test]
    fn pre_tool_use_matches_tool_name() {
        let handlers = vec![
            make_handler(
                HookEventName::PreToolUse,
                Some("^Bash$"),
                "echo same",
                /*display_order*/ 0,
            ),
            make_handler(
                HookEventName::PreToolUse,
                Some("^Edit$"),
                "echo same",
                /*display_order*/ 1,
            ),
        ];

        let selected = select_handlers(&handlers, HookEventName::PreToolUse, Some("Bash"));

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].display_order, 0);
    }

    #[test]
    fn post_tool_use_matches_tool_name() {
        let handlers = vec![
            make_handler(
                HookEventName::PostToolUse,
                Some("^Bash$"),
                "echo same",
                /*display_order*/ 0,
            ),
            make_handler(
                HookEventName::PostToolUse,
                Some("^Edit$"),
                "echo same",
                /*display_order*/ 1,
            ),
        ];

        let selected = select_handlers(&handlers, HookEventName::PostToolUse, Some("Bash"));

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].display_order, 0);
    }

    #[test]
    fn pre_tool_use_star_matcher_matches_all_tools() {
        let handlers = vec![
            make_handler(
                HookEventName::PreToolUse,
                Some("*"),
                "echo same",
                /*display_order*/ 0,
            ),
            make_handler(
                HookEventName::PreToolUse,
                Some("^Edit$"),
                "echo same",
                /*display_order*/ 1,
            ),
        ];

        let selected = select_handlers(&handlers, HookEventName::PreToolUse, Some("Bash"));

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].display_order, 0);
    }

    #[test]
    fn pre_tool_use_regex_alternation_matches_each_tool_name() {
        let handlers = vec![make_handler(
            HookEventName::PreToolUse,
            Some("Edit|Write"),
            "echo same",
            /*display_order*/ 0,
        )];

        let selected_edit = select_handlers(&handlers, HookEventName::PreToolUse, Some("Edit"));
        let selected_write = select_handlers(&handlers, HookEventName::PreToolUse, Some("Write"));
        let selected_bash = select_handlers(&handlers, HookEventName::PreToolUse, Some("Bash"));

        assert_eq!(selected_edit.len(), 1);
        assert_eq!(selected_write.len(), 1);
        assert_eq!(selected_bash.len(), 0);
    }

    #[test]
    fn pre_tool_use_aliases_match_once_per_handler() {
        let handlers = vec![
            make_handler(
                HookEventName::PreToolUse,
                Some("^apply_patch$"),
                "echo apply_patch",
                /*display_order*/ 0,
            ),
            make_handler(
                HookEventName::PreToolUse,
                Some("^Write$"),
                "echo write",
                /*display_order*/ 1,
            ),
            make_handler(
                HookEventName::PreToolUse,
                Some("^Edit$"),
                "echo edit",
                /*display_order*/ 2,
            ),
            make_handler(
                HookEventName::PreToolUse,
                Some("apply_patch|Write|Edit"),
                "echo combined",
                /*display_order*/ 3,
            ),
        ];

        let selected = select_handlers_for_matcher_inputs(
            &handlers,
            HookEventName::PreToolUse,
            &["apply_patch", "Write", "Edit"],
        );

        assert_eq!(selected.len(), 4);
        assert_eq!(
            selected
                .iter()
                .map(|handler| handler.display_order)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
        );
    }

    #[test]
    fn user_prompt_submit_ignores_matcher() {
        let handlers = vec![
            make_handler(
                HookEventName::UserPromptSubmit,
                Some("^hello"),
                "echo first",
                /*display_order*/ 0,
            ),
            make_handler(
                HookEventName::UserPromptSubmit,
                Some("["),
                "echo second",
                /*display_order*/ 1,
            ),
        ];

        let selected = select_handlers(
            &handlers,
            HookEventName::UserPromptSubmit,
            /*matcher_input*/ None,
        );

        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].display_order, 0);
        assert_eq!(selected[1].display_order, 1);
    }

    #[test]
    fn select_handlers_preserves_declaration_order() {
        let handlers = vec![
            make_handler(
                HookEventName::Stop,
                /*matcher*/ None,
                "first",
                /*display_order*/ 0,
            ),
            make_handler(
                HookEventName::Stop,
                /*matcher*/ None,
                "second",
                /*display_order*/ 1,
            ),
            make_handler(
                HookEventName::Stop,
                /*matcher*/ None,
                "third",
                /*display_order*/ 2,
            ),
        ];

        let selected = select_handlers(&handlers, HookEventName::Stop, /*matcher_input*/ None);

        assert_eq!(selected.len(), 3);
        assert_eq!(selected[0].command, "first");
        assert_eq!(selected[1].command, "second");
        assert_eq!(selected[2].command, "third");
    }
}
