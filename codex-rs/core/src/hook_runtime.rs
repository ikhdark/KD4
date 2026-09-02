use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use codex_analytics::CompactionTrigger;
use codex_analytics::HookRunFact;
use codex_analytics::build_track_events_context;
use codex_context_fragments::ModelContextBudget;
use codex_context_fragments::RenderedContextFragment;
use codex_hooks::ContextInjectingHookOutcome;
use codex_hooks::InterruptRequest;
use codex_hooks::PermissionRequestDecision;
use codex_hooks::PermissionRequestOutcome;
use codex_hooks::PermissionRequestRequest;
use codex_hooks::PostToolUseOutcome;
use codex_hooks::PostToolUsePlan;
use codex_hooks::PostToolUseRequest;
use codex_hooks::PreToolUseOutcome;
use codex_hooks::PreToolUseRequest;
use codex_hooks::StartHookTarget;
use codex_hooks::StopHookTarget;
use codex_hooks::StopOutcome;
use codex_hooks::SubagentHookContext;
use codex_hooks::UserPromptSubmitRequest;
use codex_otel::HOOK_RUN_DURATION_METRIC;
use codex_otel::HOOK_RUN_METRIC;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;
use codex_protocol::protocol::HookStartedEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::WarningEvent;
use codex_thread_store::ReadThreadParams;
use serde_json::Value;
use tracing::instrument;

use crate::context::ContextualUserFragment;
use crate::context::HookAdditionalContext;
use crate::event_mapping::parse_turn_item;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::hook_names::HookToolName;
use crate::tools::sandboxing::PermissionRequestPayload;

#[derive(Default)]
pub(crate) struct HookRuntimeOutcome {
    pub should_stop: bool,
    pub stop_reason: Option<String>,
    pub additional_contexts: Vec<String>,
}

pub(crate) enum PreToolUseHookResult {
    Continue { updated_input: Option<Value> },
    Blocked(String),
}

#[derive(Debug, Default)]
pub(crate) struct LegacyAfterAgentHookOutcome {
    pub(crate) aborted: bool,
}

pub(crate) struct TurnStopHookOutcome {
    pub(crate) stop: StopOutcome,
}

#[instrument(level = "trace", skip_all)]
pub(crate) async fn run_pending_session_start_hooks(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
) -> bool {
    while let Some(session_start_source) = sess.take_pending_session_start_source().await {
        // Pending session-start hooks are reused to dispatch thread-spawn subagent
        // starts. Other subagent sessions are internal/system work and do not run
        // start hooks.
        let target = match &turn_context.session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_role, .. })
                if matches!(
                    session_start_source,
                    codex_hooks::SessionStartSource::Startup
                ) =>
            {
                let context = subagent_hook_context(sess, agent_role);
                StartHookTarget::SubagentStart {
                    turn_id: turn_context.sub_id.clone(),
                    agent_id: context.agent_id,
                    agent_type: context.agent_type,
                }
            }
            SessionSource::SubAgent(_) => return false,
            _ => StartHookTarget::SessionStart {
                source: session_start_source,
            },
        };
        let request = codex_hooks::SessionStartRequest {
            session_id: sess.session_id().into(),
            cwd: turn_context.cwd().clone(),
            transcript_path: sess.hook_transcript_path().await,
            model: turn_context.model_info.slug.clone(),
            permission_mode: hook_permission_mode(turn_context),
            target,
        };
        let hooks = sess.hooks();
        let preview_runs = hooks.preview_session_start(&request);
        if run_context_injecting_hook(
            sess,
            turn_context,
            preview_runs,
            hooks.run_session_start(request, Some(turn_context.sub_id.clone())),
        )
        .await
        .record_additional_contexts(sess, turn_context, "SessionStart")
        .await
        {
            return true;
        }
    }

    false
}

/// Runs matching `PreToolUse` hooks before a tool executes.
///
/// `tool_name` is the canonical name serialized to hook stdin. Matcher aliases
/// are internal compatibility names used only for selecting configured hook
/// handlers.
pub(crate) async fn run_pre_tool_use_hooks(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    tool_use_id: String,
    tool_name: &HookToolName,
    tool_input: &Value,
) -> PreToolUseHookResult {
    let request = PreToolUseRequest {
        session_id: sess.session_id().into(),
        turn_id: turn_context.sub_id.clone(),
        subagent: thread_spawn_subagent_hook_context(sess, turn_context),
        cwd: turn_context.cwd().clone(),
        transcript_path: sess.hook_transcript_path().await,
        model: turn_context.model_info.slug.clone(),
        permission_mode: hook_permission_mode(turn_context),
        tool_name: tool_name.name().to_string(),
        matcher_aliases: tool_name.matcher_aliases().to_vec(),
        tool_use_id,
        tool_input: tool_input.clone(),
    };
    let hooks = sess.hooks();
    let preview_runs = hooks.preview_pre_tool_use(&request);
    emit_hook_started_events(sess, turn_context, preview_runs).await;

    let PreToolUseOutcome {
        hook_events,
        should_block,
        block_reason,
        additional_contexts,
        updated_input,
    } = hooks.run_pre_tool_use(request).await;
    emit_hook_completed_events(sess, turn_context, hook_events).await;
    record_additional_contexts(sess, turn_context, additional_contexts).await;

    if !should_block {
        return PreToolUseHookResult::Continue { updated_input };
    }

    let Some(reason) = block_reason else {
        return PreToolUseHookResult::Continue {
            updated_input: None,
        };
    };

    if (tool_name.name() == "Bash" || tool_name.name() == "apply_patch")
        && let Some(command) = tool_input.get("command").and_then(Value::as_str)
    {
        PreToolUseHookResult::Blocked(format!(
            "Command blocked by PreToolUse hook: {reason}. Command: {command}"
        ))
    } else {
        PreToolUseHookResult::Blocked(format!(
            "Tool call blocked by PreToolUse hook: {reason}. Tool: {}",
            tool_name.name()
        ))
    }
}

// PermissionRequest hooks share the same preview/start/completed event flow as
// other hook types, but they return an optional decision instead of mutating
// tool input or post-run state.
pub(crate) async fn run_permission_request_hooks(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    run_id_suffix: &str,
    payload: PermissionRequestPayload,
) -> Option<PermissionRequestDecision> {
    let request = PermissionRequestRequest {
        session_id: sess.session_id().into(),
        turn_id: turn_context.sub_id.clone(),
        subagent: thread_spawn_subagent_hook_context(sess, turn_context),
        cwd: turn_context.cwd().to_path_buf(),
        transcript_path: sess.hook_transcript_path().await,
        model: turn_context.model_info.slug.clone(),
        permission_mode: hook_permission_mode(turn_context),
        tool_name: payload.tool_name.name().to_string(),
        matcher_aliases: payload.tool_name.matcher_aliases().to_vec(),
        run_id_suffix: run_id_suffix.to_string(),
        tool_input: payload.tool_input,
    };
    let hooks = sess.hooks();
    let preview_runs = hooks.preview_permission_request(&request);
    emit_hook_started_events(sess, turn_context, preview_runs).await;

    let PermissionRequestOutcome {
        hook_events,
        decision,
    } = hooks.run_permission_request(request).await;
    emit_hook_completed_events(sess, turn_context, hook_events).await;

    decision
}

/// Runs matching `PostToolUse` hooks after a tool has produced a successful output.
///
/// The `tool_name`, matcher aliases, `tool_input`, and `tool_response` values are
/// already adapted by the tool handler into the stable hook contract. Passing
/// raw internal tool data here would leak implementation details into user hook
/// matchers and hook logs.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_post_tool_use_hooks(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    plan: PostToolUsePlan,
    tool_use_id: String,
    tool_name: String,
    matcher_aliases: Vec<String>,
    tool_input: Value,
    tool_response: Value,
) -> PostToolUseOutcome {
    let request = PostToolUseRequest {
        session_id: sess.session_id().into(),
        turn_id: turn_context.sub_id.clone(),
        subagent: thread_spawn_subagent_hook_context(sess, turn_context),
        cwd: turn_context.cwd().clone(),
        transcript_path: sess.hook_transcript_path().await,
        model: turn_context.model_info.slug.clone(),
        permission_mode: hook_permission_mode(turn_context),
        tool_name,
        matcher_aliases,
        tool_use_id,
        tool_input,
        tool_response,
    };
    let hooks = sess.hooks();
    let preview_runs = hooks.preview_planned_post_tool_use(&plan, &request.tool_use_id);
    emit_hook_started_events(sess, turn_context, preview_runs).await;

    let outcome = hooks.run_planned_post_tool_use(plan, request).await;
    emit_hook_completed_events(sess, turn_context, outcome.hook_events.clone()).await;
    outcome
}

#[instrument(level = "trace", skip_all)]
pub(crate) async fn run_turn_stop_hooks(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    stop_hook_active: bool,
    last_assistant_message: Option<String>,
) -> TurnStopHookOutcome {
    // Resolve the stop hook kind from the session source before building the
    // request. Root turns run Stop; thread-spawned child turns run SubagentStop.
    let event_name = match &turn_context.session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }) => HookEventName::SubagentStop,
        // Internal/synthetic subagents do not expose user-configured lifecycle
        // hooks, so there is no Stop or SubagentStop request to dispatch.
        SessionSource::SubAgent(_) => {
            return TurnStopHookOutcome {
                stop: StopOutcome::default(),
            };
        }
        _ => HookEventName::Stop,
    };
    let hooks = sess.hooks();
    if !hooks.has_handler_for(event_name) {
        return TurnStopHookOutcome {
            stop: StopOutcome::default(),
        };
    }
    let (target, transcript_path) = match &turn_context.session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            agent_role,
            parent_thread_id,
            ..
        }) => {
            let context = subagent_hook_context(sess, agent_role);
            let agent_transcript_path = sess.hook_transcript_path().await;
            let parent_transcript_path = match sess
                .services
                .thread_store
                .read_thread(ReadThreadParams {
                    thread_id: *parent_thread_id,
                    include_archived: true,
                    include_history: false,
                })
                .await
            {
                Ok(thread) => thread.rollout_path,
                Err(error) => {
                    tracing::warn!(
                        parent_thread_id = %parent_thread_id,
                        error = %error,
                        "failed to resolve parent transcript path for subagent hook"
                    );
                    None
                }
            };
            (
                StopHookTarget::SubagentStop {
                    agent_id: context.agent_id,
                    agent_type: context.agent_type,
                    agent_transcript_path,
                },
                parent_transcript_path,
            )
        }
        SessionSource::SubAgent(_) => unreachable!("internal subagents returned before dispatch"),
        _ => (StopHookTarget::Stop, sess.hook_transcript_path().await),
    };
    let request = codex_hooks::StopRequest {
        session_id: sess.session_id().into(),
        turn_id: turn_context.sub_id.clone(),
        cwd: turn_context.cwd().clone(),
        transcript_path,
        model: turn_context.model_info.slug.clone(),
        permission_mode: hook_permission_mode(turn_context),
        stop_hook_active,
        last_assistant_message,
        target,
    };
    emit_hook_started_events(sess, turn_context, hooks.preview_stop(&request)).await;

    let mut outcome = hooks.run_stop(request).await;
    emit_hook_completed_events(sess, turn_context, std::mem::take(&mut outcome.hook_events)).await;
    TurnStopHookOutcome { stop: outcome }
}

#[instrument(level = "trace", skip_all)]
pub(crate) async fn run_turn_interrupt_hooks(sess: &Arc<Session>, turn_context: &Arc<TurnContext>) {
    // User-configured Interrupt hooks describe the root turn lifecycle. Internal
    // and thread-spawned subagent sessions have their own parent-owned terminal
    // lifecycle and must not dispatch a second Interrupt notification.
    if matches!(&turn_context.session_source, SessionSource::SubAgent(_)) {
        return;
    }

    let hooks = sess.hooks();
    let preview_runs = hooks.preview_interrupt();
    if preview_runs.is_empty() {
        return;
    }

    let request = InterruptRequest {
        session_id: sess.session_id().into(),
        turn_id: turn_context.sub_id.clone(),
        cwd: turn_context.cwd().clone(),
        transcript_path: sess.hook_transcript_path().await,
        model: turn_context.model_info.slug.clone(),
        permission_mode: hook_permission_mode(turn_context),
    };

    // Interrupt hooks may inspect the transcript, so make the durable history
    // written by terminal preparation visible before the process starts.
    if let Err(error) = sess.flush_rollout().await {
        tracing::warn!(
            turn_id = %turn_context.sub_id,
            %error,
            "failed to flush rollout before Interrupt hooks"
        );
    }

    emit_hook_started_events(sess, turn_context, preview_runs).await;
    let outcome = hooks.run_interrupt(request).await;
    emit_hook_completed_events(sess, turn_context, outcome.hook_events).await;
}

pub(crate) async fn run_pre_compact_hooks(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    trigger: CompactionTrigger,
) -> PreCompactHookOutcome {
    let request = codex_hooks::PreCompactRequest {
        session_id: sess.session_id().into(),
        turn_id: turn_context.sub_id.clone(),
        subagent: thread_spawn_subagent_hook_context(sess, turn_context),
        cwd: turn_context.cwd().clone(),
        transcript_path: sess.hook_transcript_path().await,
        model: turn_context.model_info.slug.clone(),
        trigger: compaction_trigger_label(trigger).to_string(),
    };
    let preview_runs = sess.hooks().preview_pre_compact(&request);
    emit_hook_started_events(sess, turn_context, preview_runs).await;

    let outcome = sess.hooks().run_pre_compact(request).await;
    emit_hook_completed_events(sess, turn_context, outcome.hook_events).await;
    if outcome.should_stop {
        PreCompactHookOutcome::Stopped {
            reason: outcome.stop_reason,
        }
    } else {
        PreCompactHookOutcome::Continue
    }
}

pub(crate) enum PreCompactHookOutcome {
    Continue,
    Stopped { reason: Option<String> },
}

pub(crate) enum PostCompactHookOutcome {
    Continue,
    Stopped { reason: Option<String> },
}

pub(crate) async fn run_pre_compact_hook_gate(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    trigger: CompactionTrigger,
) -> bool {
    match run_pre_compact_hooks(sess, turn_context, trigger).await {
        PreCompactHookOutcome::Continue => false,
        PreCompactHookOutcome::Stopped { reason } => {
            emit_hook_stop_reason(sess, turn_context, "PreCompact", reason.as_deref()).await;
            true
        }
    }
}

pub(crate) async fn run_post_compact_hooks(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    trigger: CompactionTrigger,
    compaction_summary: Option<&str>,
) -> PostCompactHookOutcome {
    let request = codex_hooks::PostCompactRequest {
        session_id: sess.session_id().into(),
        turn_id: turn_context.sub_id.clone(),
        subagent: thread_spawn_subagent_hook_context(sess, turn_context),
        cwd: turn_context.cwd().clone(),
        transcript_path: sess.hook_transcript_path().await,
        model: turn_context.model_info.slug.clone(),
        trigger: compaction_trigger_label(trigger).to_string(),
        compaction_summary: compaction_summary.map(str::to_owned),
    };
    let preview_runs = sess.hooks().preview_post_compact(&request);
    emit_hook_started_events(sess, turn_context, preview_runs).await;

    let outcome = sess.hooks().run_post_compact(request).await;
    emit_hook_completed_events(sess, turn_context, outcome.hook_events).await;
    if outcome.should_stop {
        PostCompactHookOutcome::Stopped {
            reason: outcome.stop_reason,
        }
    } else {
        PostCompactHookOutcome::Continue
    }
}

pub(crate) async fn run_post_compact_hook_gate(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    trigger: CompactionTrigger,
    compaction_summary: Option<&str>,
) -> bool {
    match run_post_compact_hooks(sess, turn_context, trigger, compaction_summary).await {
        PostCompactHookOutcome::Continue => false,
        PostCompactHookOutcome::Stopped { reason } => {
            emit_hook_stop_reason(sess, turn_context, "PostCompact", reason.as_deref()).await;
            true
        }
    }
}

#[instrument(level = "trace", skip_all)]
pub(crate) async fn run_legacy_after_agent_hook(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    input: &[ResponseItem],
    last_assistant_message: Option<String>,
) -> LegacyAfterAgentHookOutcome {
    let mut abort_message = None;
    let input_messages = input
        .iter()
        .filter_map(|item| match parse_turn_item(item) {
            Some(TurnItem::UserMessage(user_message)) => Some(user_message.message()),
            _ => None,
        })
        .collect();
    let hooks = sess.hooks();
    for hook_outcome in hooks
        .dispatch(codex_hooks::HookPayload {
            session_id: sess.session_id().into(),
            cwd: turn_context.cwd().clone(),
            client: turn_context.app_server_client_name.clone(),
            triggered_at: chrono::Utc::now(),
            hook_event: codex_hooks::HookEvent::AfterAgent {
                event: codex_hooks::HookEventAfterAgent {
                    thread_id: sess.thread_id,
                    turn_id: turn_context.sub_id.clone(),
                    input_messages,
                    last_assistant_message,
                },
            },
        })
        .await
    {
        let hook_name = hook_outcome.hook_name;
        let (error, should_abort) = match hook_outcome.result {
            codex_hooks::HookResult::Success => continue,
            codex_hooks::HookResult::FailedContinue(error) => (error, false),
            codex_hooks::HookResult::FailedAbort(error) => (error, true),
        };
        let action = if should_abort {
            "aborting operation"
        } else {
            "continuing"
        };
        tracing::warn!(
            turn_id = %turn_context.sub_id,
            hook_name = %hook_name,
            error = %error,
            "after_agent hook failed; {action}"
        );
        if should_abort && abort_message.is_none() {
            abort_message = Some(format!(
                "after_agent hook '{hook_name}' failed and aborted turn completion: {error}"
            ));
        }
    }
    if let Some(message) = abort_message {
        let event = EventMsg::Error(codex_protocol::protocol::ErrorEvent {
            message,
            codex_error_info: Some(CodexErrorInfo::Other),
        });
        sess.send_event(turn_context, event).await;
        LegacyAfterAgentHookOutcome { aborted: true }
    } else {
        LegacyAfterAgentHookOutcome { aborted: false }
    }
}

pub(crate) async fn inspect_pending_input(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    pending_input_item: &TurnInput,
) -> HookRuntimeOutcome {
    match pending_input_item {
        TurnInput::UserInput { content, .. } => {
            let hooks = sess.hooks();
            if !hooks.has_handler_for(HookEventName::UserPromptSubmit) {
                return HookRuntimeOutcome::default();
            }
            let request = UserPromptSubmitRequest {
                session_id: sess.session_id().into(),
                turn_id: turn_context.sub_id.clone(),
                subagent: thread_spawn_subagent_hook_context(sess, turn_context),
                cwd: turn_context.cwd().clone(),
                transcript_path: sess.hook_transcript_path().await,
                model: turn_context.model_info.slug.clone(),
                permission_mode: hook_permission_mode(turn_context),
                prompt: UserMessageItem::new(content).message(),
            };
            let preview_runs = hooks.preview_user_prompt_submit(&request);
            run_context_injecting_hook(
                sess,
                turn_context,
                preview_runs,
                hooks.run_user_prompt_submit(request),
            )
            .await
        }
        TurnInput::ResponseItem(_) | TurnInput::InternalResponseItem(_) => {
            HookRuntimeOutcome::default()
        }
        TurnInput::InterAgentCommunication(_) => HookRuntimeOutcome::default(),
    }
}

pub(crate) async fn record_pending_input(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    pending_input: TurnInput,
    additional_contexts: Vec<String>,
) {
    match pending_input {
        TurnInput::UserInput { content, client_id } => {
            sess.record_user_prompt_and_emit_turn_item(
                turn_context.as_ref(),
                content.as_slice(),
                client_id,
            )
            .await;
        }
        TurnInput::ResponseItem(item) | TurnInput::InternalResponseItem(item) => {
            sess.record_conversation_items(turn_context, std::slice::from_ref(&item))
                .await;
        }
        TurnInput::InterAgentCommunication(communication) => {
            sess.record_inter_agent_communication(turn_context, communication)
                .await;
        }
    }
    record_additional_contexts(sess, turn_context, additional_contexts).await;
}

#[cfg(test)]
mod compaction_hook_gate_contract_tests {
    #[test]
    fn local_and_remote_compaction_use_the_shared_hook_gates() {
        for source in [
            include_str!("compact.rs"),
            include_str!("compact_remote_v2.rs"),
        ] {
            assert!(source.contains("run_pre_compact_hook_gate"));
            assert!(source.contains("run_post_compact_hook_gate"));
            assert!(!source.contains("emit_hook_stop_reason"));
        }
    }
}

async fn run_context_injecting_hook<Fut>(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    preview_runs: Vec<HookRunSummary>,
    outcome_future: Fut,
) -> HookRuntimeOutcome
where
    Fut: Future<Output = ContextInjectingHookOutcome>,
{
    emit_hook_started_events(sess, turn_context, preview_runs).await;

    let ContextInjectingHookOutcome {
        hook_events,
        should_stop,
        stop_reason,
        additional_contexts,
    } = outcome_future.await;
    emit_hook_completed_events(sess, turn_context, hook_events).await;
    HookRuntimeOutcome {
        should_stop,
        stop_reason,
        additional_contexts,
    }
}

impl HookRuntimeOutcome {
    async fn record_additional_contexts(
        self,
        sess: &Arc<Session>,
        turn_context: &Arc<TurnContext>,
        hook_name: &'static str,
    ) -> bool {
        if hook_name == "SessionStart" {
            record_session_start_additional_contexts(sess, turn_context, self.additional_contexts)
                .await;
        } else {
            record_additional_contexts(sess, turn_context, self.additional_contexts).await;
        }

        if self.should_stop {
            emit_hook_stop_reason(sess, turn_context, hook_name, self.stop_reason.as_deref()).await;
        }

        self.should_stop
    }
}

pub(crate) async fn emit_hook_stop_reason(
    sess: &Session,
    turn_context: &TurnContext,
    hook_name: &str,
    stop_reason: Option<&str>,
) {
    let Some(stop_reason) = stop_reason.filter(|reason| !reason.trim().is_empty()) else {
        return;
    };
    sess.send_event(
        turn_context,
        EventMsg::Warning(WarningEvent {
            message: format!("{hook_name} hook stopped the operation: {stop_reason}"),
        }),
    )
    .await;
}

pub(crate) async fn record_additional_contexts(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    additional_contexts: Vec<String>,
) {
    let developer_messages =
        prepare_additional_context_items(sess, turn_context, additional_contexts).await;
    if developer_messages.is_empty() {
        return;
    }

    sess.record_conversation_items(turn_context, developer_messages.as_slice())
        .await;
}

async fn record_session_start_additional_contexts(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    additional_contexts: Vec<String>,
) {
    let developer_messages =
        prepare_additional_context_items(sess, turn_context, additional_contexts).await;
    if developer_messages.is_empty() {
        return;
    }
    let history = sess.clone_history().await;
    let developer_messages =
        dedupe_existing_developer_contexts(history.raw_items(), developer_messages);
    if developer_messages.is_empty() {
        return;
    }
    sess.record_conversation_items(turn_context, developer_messages.as_slice())
        .await;
}

fn dedupe_existing_developer_contexts(
    existing: &[ResponseItem],
    candidates: Vec<ResponseItem>,
) -> Vec<ResponseItem> {
    let existing_text = existing
        .iter()
        .filter_map(single_developer_input_text)
        .collect::<HashSet<_>>();
    candidates
        .into_iter()
        .filter(|candidate| {
            single_developer_input_text(candidate).is_none_or(|text| !existing_text.contains(text))
        })
        .collect()
}

fn single_developer_input_text(item: &ResponseItem) -> Option<&str> {
    let ResponseItem::Message { role, content, .. } = item else {
        return None;
    };
    if role != "developer" {
        return None;
    }
    match content.as_slice() {
        [codex_protocol::models::ContentItem::InputText { text }] => Some(text),
        _ => None,
    }
}

pub(crate) async fn prepare_additional_context_items(
    _sess: &Arc<Session>,
    _turn_context: &Arc<TurnContext>,
    additional_contexts: Vec<String>,
) -> Vec<ResponseItem> {
    additional_context_messages(additional_contexts)
}

fn additional_context_messages(additional_contexts: Vec<String>) -> Vec<ResponseItem> {
    let mut budget = ModelContextBudget::default();
    additional_contexts
        .into_iter()
        .map(HookAdditionalContext::new)
        .filter_map(|fragment| {
            budget.take(&fragment.render()).map(|text| {
                ContextualUserFragment::into(RenderedContextFragment::new("developer", text))
            })
        })
        .collect()
}

async fn emit_hook_started_events(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    preview_runs: Vec<HookRunSummary>,
) {
    for run in preview_runs {
        sess.send_event(
            turn_context,
            EventMsg::HookStarted(HookStartedEvent {
                turn_id: Some(turn_context.sub_id.clone()),
                run,
            }),
        )
        .await;
    }
}

pub(crate) async fn emit_hook_completed_events(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    completed_events: Vec<HookCompletedEvent>,
) {
    for completed in completed_events {
        emit_hook_completed_metrics(turn_context, &completed);
        track_hook_completed_analytics(sess, turn_context, &completed);
        sess.send_event(turn_context, EventMsg::HookCompleted(completed))
            .await;
    }
}

fn emit_hook_completed_metrics(turn_context: &TurnContext, completed: &HookCompletedEvent) {
    let tags = hook_run_metric_tags(&completed.run);
    turn_context
        .session_telemetry
        .counter(HOOK_RUN_METRIC, /*inc*/ 1, &tags);
    if let Some(duration_ms) = completed.run.duration_ms
        && let Ok(duration_ms) = u64::try_from(duration_ms)
    {
        turn_context.session_telemetry.record_duration(
            HOOK_RUN_DURATION_METRIC,
            Duration::from_millis(duration_ms),
            &tags,
        );
    }
}

fn track_hook_completed_analytics(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    completed: &HookCompletedEvent,
) {
    let (tracking, hook) =
        hook_run_analytics_payload(sess.thread_id.to_string(), turn_context, completed);
    sess.services
        .analytics_events_client
        .track_hook_run(tracking, hook);
}

fn hook_run_analytics_payload(
    thread_id: String,
    turn_context: &TurnContext,
    completed: &HookCompletedEvent,
) -> (codex_analytics::TrackEventsContext, HookRunFact) {
    (
        build_track_events_context(
            turn_context.model_info.slug.clone(),
            thread_id,
            completed
                .turn_id
                .clone()
                .unwrap_or_else(|| turn_context.sub_id.clone()),
            turn_context.originator.clone(),
        ),
        HookRunFact {
            event_name: completed.run.event_name,
            hook_source: completed.run.source,
            status: completed.run.status,
        },
    )
}

fn hook_run_metric_tags(run: &HookRunSummary) -> [(&'static str, &'static str); 3] {
    let hook_name = run.event_name.as_pascal_case_label();
    let hook_source = run.source.as_snake_case_label();
    let status = match run.status {
        HookRunStatus::Running => "running",
        HookRunStatus::Completed => "completed",
        HookRunStatus::Failed => "failed",
        HookRunStatus::Blocked => "blocked",
        HookRunStatus::Stopped => "stopped",
    };

    [
        ("hook_name", hook_name),
        ("source", hook_source),
        ("status", status),
    ]
}

fn hook_permission_mode(turn_context: &TurnContext) -> String {
    match turn_context.approval_policy.value() {
        AskForApproval::Never => "bypassPermissions",
        AskForApproval::UnlessTrusted | AskForApproval::OnRequest | AskForApproval::Granular(_) => {
            "default"
        }
    }
    .to_string()
}

fn thread_spawn_subagent_hook_context(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
) -> Option<SubagentHookContext> {
    match &turn_context.session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_role, .. }) => {
            Some(subagent_hook_context(sess, agent_role))
        }
        _ => None,
    }
}

fn subagent_hook_context(sess: &Arc<Session>, agent_role: &Option<String>) -> SubagentHookContext {
    SubagentHookContext {
        agent_id: sess.thread_id().to_string(),
        agent_type: agent_role
            .clone()
            .unwrap_or_else(|| crate::agent::role::DEFAULT_ROLE_NAME.to_string()),
    }
}

fn compaction_trigger_label(value: CompactionTrigger) -> &'static str {
    match value {
        CompactionTrigger::Manual => "manual",
        CompactionTrigger::Auto => "auto",
    }
}

#[cfg(test)]
mod tests {
    use super::additional_context_messages;
    use super::dedupe_existing_developer_contexts;
    use super::hook_run_analytics_payload;
    use super::hook_run_metric_tags;
    use super::prepare_additional_context_items;
    use crate::session::tests::make_session_and_context;
    use codex_protocol::models::ContentItem;
    use codex_protocol::protocol::HookCompletedEvent;
    use codex_protocol::protocol::HookEventName;
    use codex_protocol::protocol::HookExecutionMode;
    use codex_protocol::protocol::HookHandlerType;
    use codex_protocol::protocol::HookRunStatus;
    use codex_protocol::protocol::HookRunSummary;
    use codex_protocol::protocol::HookScope;
    use codex_protocol::protocol::HookSource;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use pretty_assertions::assert_eq;

    #[test]
    fn context_injecting_hook_outcome_preserves_runtime_fields() {
        let outcome = codex_hooks::ContextInjectingHookOutcome {
            hook_events: Vec::new(),
            should_stop: true,
            stop_reason: Some("prompt blocked".to_string()),
            additional_contexts: vec!["injected".to_string()],
        };

        assert!(outcome.hook_events.is_empty());
        assert!(outcome.should_stop);
        assert_eq!(outcome.stop_reason.as_deref(), Some("prompt blocked"));
        assert_eq!(outcome.additional_contexts, vec!["injected"]);
    }

    #[test]
    fn additional_context_messages_stay_separate_and_ordered() {
        let messages = additional_context_messages(vec![
            "first tide note".to_string(),
            "second tide note".to_string(),
        ]);

        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages
                .iter()
                .map(|message| match message {
                    codex_protocol::models::ResponseItem::Message { role, content, .. } => {
                        let text = content
                            .iter()
                            .map(|item| match item {
                                ContentItem::InputText { text } => text.as_str(),
                                ContentItem::InputImage { .. } | ContentItem::OutputText { .. } => {
                                    panic!("expected input text content, got {item:?}")
                                }
                            })
                            .collect::<String>();
                        (role.as_str(), text)
                    }
                    other => panic!("expected developer message, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            vec![
                ("developer", "first tide note".to_string()),
                ("developer", "second tide note".to_string()),
            ],
        );
    }

    #[tokio::test]
    async fn retired_continuity_marker_is_plain_hook_context() {
        let (session, turn_context) = make_session_and_context().await;
        let session = std::sync::Arc::new(session);
        let turn_context = std::sync::Arc::new(turn_context);
        let legacy_context =
            "<kd4_continuity_capsule_v1>{not-json}</kd4_continuity_capsule_v1>".to_string();

        let messages =
            prepare_additional_context_items(&session, &turn_context, vec![legacy_context.clone()])
                .await;

        assert_eq!(messages, additional_context_messages(vec![legacy_context]));
    }

    #[test]
    fn session_start_context_dedupes_markerless_resumed_history() {
        let existing = additional_context_messages(vec!["same startup context".to_string()]);
        let candidates = additional_context_messages(vec![
            "same startup context".to_string(),
            "new startup context".to_string(),
        ]);

        let deduped = dedupe_existing_developer_contexts(&existing, candidates);

        assert_eq!(
            deduped,
            additional_context_messages(vec!["new startup context".to_string()])
        );
    }

    #[test]
    fn additional_context_messages_share_one_hard_budget() {
        let messages = additional_context_messages(vec!["a".repeat(30_000), "b".repeat(30_000)]);
        let rendered_bytes = messages
            .iter()
            .map(|message| match message {
                codex_protocol::models::ResponseItem::Message { content, .. } => content
                    .iter()
                    .map(|item| match item {
                        ContentItem::InputText { text } => text.len(),
                        _ => 0,
                    })
                    .sum::<usize>(),
                _ => 0,
            })
            .sum::<usize>();
        assert!(rendered_bytes <= 40_000);
    }

    #[tokio::test]
    async fn hook_run_analytics_payload_uses_completed_turn_id() {
        let (_session, turn_context) = make_session_and_context().await;
        let completed = HookCompletedEvent {
            turn_id: Some("turn-from-hook".to_string()),
            run: sample_hook_run(HookRunStatus::Blocked, HookSource::Project),
        };

        let (tracking, hook) =
            hook_run_analytics_payload("thread-123".to_string(), &turn_context, &completed);

        assert_eq!(tracking.thread_id, "thread-123");
        assert_eq!(tracking.turn_id, "turn-from-hook");
        assert_eq!(tracking.model_slug, turn_context.model_info.slug);
        assert_eq!(hook.event_name, HookEventName::Stop);
        assert_eq!(hook.hook_source, HookSource::Project);
        assert_eq!(hook.status, HookRunStatus::Blocked);
    }

    #[tokio::test]
    async fn hook_run_analytics_payload_falls_back_to_turn_context_id() {
        let (_session, turn_context) = make_session_and_context().await;
        let completed = HookCompletedEvent {
            turn_id: None,
            run: sample_hook_run(HookRunStatus::Failed, HookSource::Unknown),
        };

        let (tracking, hook) =
            hook_run_analytics_payload("thread-123".to_string(), &turn_context, &completed);

        assert_eq!(tracking.turn_id, turn_context.sub_id);
        assert_eq!(hook.hook_source, HookSource::Unknown);
        assert_eq!(hook.status, HookRunStatus::Failed);
    }

    #[test]
    fn hook_run_metric_tags_match_analytics_shape() {
        let run = sample_hook_run(HookRunStatus::Blocked, HookSource::Project);

        assert_eq!(
            hook_run_metric_tags(&run),
            [
                ("hook_name", "Stop"),
                ("source", "project"),
                ("status", "blocked"),
            ]
        );

        let cloud_requirements =
            sample_hook_run(HookRunStatus::Blocked, HookSource::CloudRequirements);

        assert_eq!(
            hook_run_metric_tags(&cloud_requirements),
            [
                ("hook_name", "Stop"),
                ("source", "cloud_requirements"),
                ("status", "blocked"),
            ]
        );
    }

    #[test]
    fn hook_run_metric_tags_include_expanded_hook_sources() {
        let run = sample_hook_run(HookRunStatus::Completed, HookSource::LegacyManagedConfigMdm);

        assert_eq!(
            hook_run_metric_tags(&run),
            [
                ("hook_name", "Stop"),
                ("source", "legacy_managed_config_mdm"),
                ("status", "completed"),
            ]
        );
    }

    fn sample_hook_run(status: HookRunStatus, source: HookSource) -> HookRunSummary {
        HookRunSummary {
            id: "stop:0:/tmp/hooks.json".to_string(),
            event_name: HookEventName::Stop,
            handler_type: HookHandlerType::Command,
            execution_mode: HookExecutionMode::Sync,
            scope: HookScope::Turn,
            source_path: test_path_buf("/tmp/hooks.json").abs(),
            source,
            display_order: 0,
            status,
            status_message: None,
            started_at: 10,
            completed_at: Some(37),
            duration_ms: Some(27),
            entries: Vec::new(),
        }
    }
}
