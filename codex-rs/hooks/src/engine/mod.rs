pub(crate) mod command_runner;
pub(crate) mod discovery;
pub(crate) mod dispatcher;
pub(crate) mod output_parser;

use crate::events::common::ContextInjectingHookOutcome;
use crate::events::common::StatelessHookOutcome;
use crate::events::compact::PostCompactRequest;
use crate::events::compact::PreCompactRequest;
use crate::events::interrupt::InterruptOutcome;
use crate::events::interrupt::InterruptRequest;
use crate::events::permission_request::PermissionRequestOutcome;
use crate::events::permission_request::PermissionRequestRequest;
use crate::events::post_tool_use::PostToolUseOutcome;
use crate::events::post_tool_use::PostToolUsePlan;
use crate::events::post_tool_use::PostToolUseRequest;
use crate::events::pre_tool_use::PreToolUseOutcome;
use crate::events::pre_tool_use::PreToolUseRequest;
use crate::events::session_start::SessionStartRequest;
use crate::events::stop::StopOutcome;
use crate::events::stop::StopRequest;
use crate::events::user_prompt_submit::UserPromptSubmitRequest;
use crate::output_spill::HookOutputSpiller;
use codex_config::ConfigLayerStack;
use codex_config::HookRunScope;
use codex_plugin::PluginHookSource;
use codex_protocol::ThreadId;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookHandlerType;
use codex_protocol::protocol::HookRunSummary;
use codex_protocol::protocol::HookSource;
use codex_protocol::protocol::HookTrustStatus;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

#[derive(Debug, Clone)]
pub(crate) struct CommandShell {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfiguredHandler {
    pub event_name: codex_protocol::protocol::HookEventName,
    pub matcher: Option<crate::events::common::HookMatcher>,
    pub command: String,
    pub timeout_sec: u64,
    pub status_message: Option<String>,
    pub source_path: AbsolutePathBuf,
    pub source: HookSource,
    pub display_order: i64,
    pub env: HashMap<String, String>,
}

impl ConfiguredHandler {
    pub fn run_id(&self) -> String {
        hook_run_id(self.event_name, self.display_order, &self.source_path)
    }
}

/// Stable handler identity shared by run summaries and per-scope run gating.
pub(crate) fn hook_run_id(
    event_name: HookEventName,
    display_order: i64,
    source_path: &AbsolutePathBuf,
) -> String {
    format!(
        "{}:{}:{}",
        event_name_label(event_name),
        display_order,
        source_path.display()
    )
}

fn event_name_label(event_name: HookEventName) -> &'static str {
    match event_name {
        HookEventName::PreToolUse => "pre-tool-use",
        HookEventName::PermissionRequest => "permission-request",
        HookEventName::PostToolUse => "post-tool-use",
        HookEventName::PreCompact => "pre-compact",
        HookEventName::PostCompact => "post-compact",
        HookEventName::SessionStart => "session-start",
        HookEventName::UserPromptSubmit => "user-prompt-submit",
        HookEventName::SubagentStart => "subagent-start",
        HookEventName::SubagentStop => "subagent-stop",
        HookEventName::Stop => "stop",
        HookEventName::Interrupt => "interrupt",
    }
}

/// A handler's `once_per` scope plus its persisted hook key.
///
/// Run ids shift when handlers are inserted or reordered; the content-based key
/// does not, so recorded runs stay attached to the same handler across rebuilds.
#[derive(Debug, Clone)]
pub(crate) struct OncePerLimit {
    pub scope: HookRunScope,
    pub key: String,
}

/// Admits `once_per` handlers only until they have run in the current scope.
///
/// Every event applies it to its matched handlers. A handler without a
/// configured scope is always admitted. Previews only check admission so they
/// can report the handlers the following run will spawn; runs claim atomically.
pub(crate) struct ScopedRunGate<'a> {
    once_per: &'a HashMap<String, OncePerLimit>,
    scoped_runs: &'a Mutex<HashSet<String>>,
    session_id: String,
    turn_id: String,
}

impl<'a> ScopedRunGate<'a> {
    pub(crate) fn new(
        once_per: &'a HashMap<String, OncePerLimit>,
        scoped_runs: &'a Mutex<HashSet<String>>,
        session_id: &str,
        turn_id: &str,
    ) -> Self {
        Self {
            once_per,
            scoped_runs,
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
        }
    }

    /// A gate with no `once_per` limits, for tests that exercise event modules directly.
    #[cfg(test)]
    pub(crate) fn unscoped() -> ScopedRunGate<'static> {
        static ONCE_PER: std::sync::LazyLock<HashMap<String, OncePerLimit>> =
            std::sync::LazyLock::new(HashMap::new);
        static SCOPED_RUNS: std::sync::LazyLock<Mutex<HashSet<String>>> =
            std::sync::LazyLock::new(Mutex::default);
        ScopedRunGate::new(&ONCE_PER, &SCOPED_RUNS, "", "")
    }

    fn scope_key(&self, handler: &ConfiguredHandler) -> Option<String> {
        if self.once_per.is_empty() {
            return None;
        }
        let limit = self.once_per.get(&handler.run_id())?;
        let (scope, scope_id) = match limit.scope {
            HookRunScope::Turn => ("turn", self.turn_id.as_str()),
            HookRunScope::Session => ("session", self.session_id.as_str()),
        };
        Some(format!("{}|{scope}:{scope_id}", limit.key))
    }

    pub(crate) fn admits(&self, handler: &ConfiguredHandler) -> bool {
        self.scope_key(handler).is_none_or(|key| {
            !self
                .scoped_runs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&key)
        })
    }

    /// Admits and records a handler under one lock, so concurrent tool calls in
    /// the same scope cannot both run it.
    pub(crate) fn claim(&self, handler: &ConfiguredHandler) -> bool {
        self.scope_key(handler).is_none_or(|key| {
            self.scoped_runs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookListEntry {
    pub key: String,
    pub event_name: HookEventName,
    pub handler_type: HookHandlerType,
    pub matcher: Option<String>,
    pub command: Option<String>,
    pub timeout_sec: u64,
    pub status_message: Option<String>,
    pub source_path: AbsolutePathBuf,
    pub source: HookSource,
    pub plugin_id: Option<String>,
    pub display_order: i64,
    pub enabled: bool,
    pub is_managed: bool,
    pub current_hash: String,
    pub trust_status: HookTrustStatus,
}

#[derive(Clone)]
pub(crate) struct ClaudeHooksEngine {
    handlers: Vec<ConfiguredHandler>,
    warnings: Vec<String>,
    shell: CommandShell,
    output_spiller: HookOutputSpiller,
    once_per: HashMap<String, OncePerLimit>,
    scoped_runs: Arc<Mutex<HashSet<String>>>,
}

impl ClaudeHooksEngine {
    pub(crate) fn new(
        enabled: bool,
        bypass_hook_trust: bool,
        config_layer_stack: Option<&ConfigLayerStack>,
        plugin_hook_sources: Vec<PluginHookSource>,
        plugin_hook_load_warnings: Vec<String>,
        shell: CommandShell,
    ) -> Self {
        if !enabled {
            return Self {
                handlers: Vec::new(),
                warnings: Vec::new(),
                shell,
                output_spiller: HookOutputSpiller::new(),
                once_per: HashMap::new(),
                scoped_runs: Arc::new(Mutex::new(HashSet::new())),
            };
        }

        let discovered = discovery::discover_handlers(
            config_layer_stack,
            plugin_hook_sources,
            plugin_hook_load_warnings,
            bypass_hook_trust,
        );
        Self {
            handlers: discovered.handlers,
            warnings: discovered.warnings,
            shell,
            output_spiller: HookOutputSpiller::new(),
            once_per: discovered.once_per,
            scoped_runs: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Shares `once_per` run history with the engine this one replaces.
    pub(crate) fn inherit_scoped_runs(&mut self, previous: &Self) {
        self.scoped_runs = Arc::clone(&previous.scoped_runs);
    }

    fn scoped_run_gate(&self, session_id: ThreadId, turn_id: &str) -> ScopedRunGate<'_> {
        ScopedRunGate::new(
            &self.once_per,
            &self.scoped_runs,
            &session_id.to_string(),
            turn_id,
        )
    }

    pub(crate) fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub(crate) fn has_handler_for(&self, event_name: HookEventName) -> bool {
        self.handlers
            .iter()
            .any(|handler| handler.event_name == event_name)
    }

    pub(crate) fn preview_session_start(
        &self,
        request: &SessionStartRequest,
        turn_id: Option<&str>,
    ) -> Vec<HookRunSummary> {
        let gate = self.scoped_run_gate(request.session_id, request.scope_turn_id(turn_id));
        crate::events::session_start::preview(&self.handlers, request, &gate)
    }

    pub(crate) fn preview_pre_tool_use(&self, request: &PreToolUseRequest) -> Vec<HookRunSummary> {
        let gate = self.scoped_run_gate(request.session_id, &request.turn_id);
        crate::events::pre_tool_use::preview(&self.handlers, request, &gate)
    }

    pub(crate) fn preview_permission_request(
        &self,
        request: &PermissionRequestRequest,
    ) -> Vec<HookRunSummary> {
        let gate = self.scoped_run_gate(request.session_id, &request.turn_id);
        crate::events::permission_request::preview(&self.handlers, request, &gate)
    }

    pub(crate) fn plan_post_tool_use(
        &self,
        tool_name: &str,
        matcher_aliases: &[String],
    ) -> PostToolUsePlan {
        crate::events::post_tool_use::plan(&self.handlers, tool_name, matcher_aliases)
    }

    pub(crate) fn preview_planned_post_tool_use(
        &self,
        plan: &PostToolUsePlan,
        request: &PostToolUseRequest,
    ) -> Vec<HookRunSummary> {
        let gate = self.scoped_run_gate(request.session_id, &request.turn_id);
        crate::events::post_tool_use::preview(plan, &request.tool_use_id, &gate)
    }

    pub(crate) async fn run_session_start(
        &self,
        request: SessionStartRequest,
        turn_id: Option<String>,
    ) -> ContextInjectingHookOutcome {
        let session_id = request.session_id;
        let scope_turn_id = request.scope_turn_id(turn_id.as_deref()).to_string();
        let gate = self.scoped_run_gate(session_id, &scope_turn_id);
        let mut outcome =
            crate::events::session_start::run(&self.handlers, &self.shell, request, turn_id, &gate)
                .await;
        outcome.additional_contexts = self
            .maybe_spill_texts(session_id, outcome.additional_contexts)
            .await;
        outcome
    }

    pub(crate) async fn run_pre_tool_use(&self, request: PreToolUseRequest) -> PreToolUseOutcome {
        let session_id = request.session_id;
        let gate = self.scoped_run_gate(session_id, &request.turn_id);
        let mut outcome =
            crate::events::pre_tool_use::run(&self.handlers, &self.shell, request, &gate).await;
        outcome.additional_contexts = self
            .maybe_spill_texts(session_id, outcome.additional_contexts)
            .await;
        outcome
    }

    pub(crate) async fn run_permission_request(
        &self,
        request: PermissionRequestRequest,
    ) -> PermissionRequestOutcome {
        let gate = self.scoped_run_gate(request.session_id, &request.turn_id);
        crate::events::permission_request::run(&self.handlers, &self.shell, request, &gate).await
    }

    pub(crate) async fn run_planned_post_tool_use(
        &self,
        plan: PostToolUsePlan,
        request: PostToolUseRequest,
    ) -> PostToolUseOutcome {
        let session_id = request.session_id;
        let gate = self.scoped_run_gate(session_id, &request.turn_id);
        let mut outcome =
            crate::events::post_tool_use::run(plan, &self.shell, request, &gate).await;
        outcome.additional_contexts = self
            .maybe_spill_texts(session_id, outcome.additional_contexts)
            .await;
        outcome.feedback_message = self
            .maybe_spill_text(session_id, outcome.feedback_message)
            .await;
        outcome
    }

    pub(crate) fn preview_pre_compact(&self, request: &PreCompactRequest) -> Vec<HookRunSummary> {
        let gate = self.scoped_run_gate(request.session_id, &request.turn_id);
        crate::events::compact::preview_pre(&self.handlers, request, &gate)
    }

    pub(crate) async fn run_pre_compact(&self, request: PreCompactRequest) -> StatelessHookOutcome {
        let gate = self.scoped_run_gate(request.session_id, &request.turn_id);
        crate::events::compact::run_pre(&self.handlers, &self.shell, request, &gate).await
    }

    pub(crate) fn preview_post_compact(&self, request: &PostCompactRequest) -> Vec<HookRunSummary> {
        let gate = self.scoped_run_gate(request.session_id, &request.turn_id);
        crate::events::compact::preview_post(&self.handlers, request, &gate)
    }

    pub(crate) async fn run_post_compact(
        &self,
        request: PostCompactRequest,
    ) -> StatelessHookOutcome {
        let gate = self.scoped_run_gate(request.session_id, &request.turn_id);
        crate::events::compact::run_post(&self.handlers, &self.shell, request, &gate).await
    }

    pub(crate) fn preview_user_prompt_submit(
        &self,
        request: &UserPromptSubmitRequest,
    ) -> Vec<HookRunSummary> {
        let gate = self.scoped_run_gate(request.session_id, &request.turn_id);
        crate::events::user_prompt_submit::preview(&self.handlers, &gate)
    }

    pub(crate) async fn run_user_prompt_submit(
        &self,
        request: UserPromptSubmitRequest,
    ) -> ContextInjectingHookOutcome {
        let session_id = request.session_id;
        let gate = self.scoped_run_gate(session_id, &request.turn_id);
        let mut outcome =
            crate::events::user_prompt_submit::run(&self.handlers, &self.shell, request, &gate)
                .await;
        outcome.additional_contexts = self
            .maybe_spill_texts(session_id, outcome.additional_contexts)
            .await;
        outcome
    }

    pub(crate) fn preview_stop(&self, request: &StopRequest) -> Vec<HookRunSummary> {
        let gate = self.scoped_run_gate(request.session_id, &request.turn_id);
        crate::events::stop::preview(&self.handlers, request, &gate)
    }

    pub(crate) async fn run_stop(&self, request: StopRequest) -> StopOutcome {
        let session_id = request.session_id;
        let gate = self.scoped_run_gate(session_id, &request.turn_id);
        let mut outcome =
            crate::events::stop::run(&self.handlers, &self.shell, request, &gate).await;
        outcome.continuation_fragments = self
            .maybe_spill_prompt_fragments(session_id, outcome.continuation_fragments)
            .await;
        outcome
    }

    pub(crate) fn preview_interrupt(
        &self,
        session_id: ThreadId,
        turn_id: &str,
    ) -> Vec<HookRunSummary> {
        let gate = self.scoped_run_gate(session_id, turn_id);
        crate::events::interrupt::preview(&self.handlers, &gate)
    }

    pub(crate) async fn run_interrupt(&self, request: InterruptRequest) -> InterruptOutcome {
        let gate = self.scoped_run_gate(request.session_id, &request.turn_id);
        crate::events::interrupt::run(&self.handlers, &self.shell, request, &gate).await
    }

    async fn maybe_spill_texts(&self, session_id: ThreadId, texts: Vec<String>) -> Vec<String> {
        self.output_spiller
            .maybe_spill_texts(session_id, texts)
            .await
    }

    async fn maybe_spill_text(&self, session_id: ThreadId, text: Option<String>) -> Option<String> {
        match text {
            Some(text) => Some(self.output_spiller.maybe_spill_text(session_id, text).await),
            None => None,
        }
    }

    async fn maybe_spill_prompt_fragments(
        &self,
        session_id: ThreadId,
        fragments: Vec<codex_protocol::items::HookPromptFragment>,
    ) -> Vec<codex_protocol::items::HookPromptFragment> {
        self.output_spiller
            .maybe_spill_prompt_fragments(session_id, fragments)
            .await
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
