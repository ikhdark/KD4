//! Assistant text parsing and client-visible plan/assistant item lifecycles.
//!
//! Owns buffered text, deferred item starts and durable partial-plan cleanup.
//! Sampling, tool execution and final-answer acceptance remain in the turn loop.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::final_message_candidate_from_output;
use super::update_final_message_candidate;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::stream_events_utils::FinalizedAssistantOutput;
use crate::stream_events_utils::TurnItemContributorPolicy;
use crate::stream_events_utils::finalize_non_tool_response_item;
use crate::stream_events_utils::record_completed_response_item_with_finalized_facts;
use codex_protocol::items::PlanItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::PlanDeltaEvent;
use codex_utils_stream_parser::AssistantTextChunk;
use codex_utils_stream_parser::AssistantTextStreamParser;
use codex_utils_stream_parser::ProposedPlanSegment;
use codex_utils_stream_parser::extract_proposed_plan_text;

/// Ephemeral per-response state for streaming a single proposed plan.
/// This is intentionally not persisted or stored in session/state since it
/// only exists while a response is actively streaming. The final plan text
/// is extracted from the completed assistant message.
/// Tracks a single proposed plan item across a streaming response.
struct ProposedPlanItemState {
    item_id: String,
    started: bool,
    completed: bool,
    /// Text already delivered to clients, retained if the final item never arrives.
    streamed_text: String,
}

/// Aggregated state used only while streaming a plan-mode response.
/// Includes per-item parsers, deferred agent message bookkeeping, and the plan item lifecycle.
pub(super) struct PlanModeStreamState {
    /// Agent message items started by the model but deferred until we see non-plan text.
    pub(super) pending_agent_message_items: HashMap<String, TurnItem>,
    /// Agent message items whose start notification has been emitted.
    started_agent_message_items: BTreeMap<String, TurnItem>,
    /// Leading whitespace buffered until we see non-whitespace text for an item.
    leading_whitespace_by_item: HashMap<String, String>,
    /// Tracks plan item lifecycle while streaming plan output.
    plan_item_state: ProposedPlanItemState,
}

impl PlanModeStreamState {
    pub(super) fn new(turn_id: &str) -> Self {
        Self {
            pending_agent_message_items: HashMap::new(),
            started_agent_message_items: BTreeMap::new(),
            leading_whitespace_by_item: HashMap::new(),
            plan_item_state: ProposedPlanItemState::new(turn_id),
        }
    }
}

#[derive(Debug, Default)]
pub(in crate::session) struct AssistantMessageStreamParsers {
    plan_mode: bool,
    parsers_by_item: HashMap<String, AssistantTextStreamParser>,
}

pub(super) type ParsedAssistantTextDelta = AssistantTextChunk;

impl AssistantMessageStreamParsers {
    pub(in crate::session) fn new(plan_mode: bool) -> Self {
        Self {
            plan_mode,
            parsers_by_item: HashMap::new(),
        }
    }

    fn parser_mut(&mut self, item_id: &str) -> &mut AssistantTextStreamParser {
        if !self.parsers_by_item.contains_key(item_id) {
            self.parsers_by_item.insert(
                item_id.to_owned(),
                AssistantTextStreamParser::new(self.plan_mode),
            );
        }
        self.parsers_by_item
            .get_mut(item_id)
            .unwrap_or_else(|| unreachable!("stream parser was inserted above"))
    }

    pub(in crate::session) fn seed_item_text(&mut self, item_id: &str, text: &str) -> ParsedAssistantTextDelta {
        if text.is_empty() {
            return ParsedAssistantTextDelta::default();
        }
        self.parser_mut(item_id).push_str(text)
    }

    pub(in crate::session) fn parse_delta(&mut self, item_id: &str, delta: &str) -> ParsedAssistantTextDelta {
        self.parser_mut(item_id).push_str(delta)
    }

    pub(in crate::session) fn finish_item(&mut self, item_id: &str) -> ParsedAssistantTextDelta {
        let Some(mut parser) = self.parsers_by_item.remove(item_id) else {
            return ParsedAssistantTextDelta::default();
        };
        parser.finish()
    }

    fn drain_finished(&mut self) -> Vec<(String, ParsedAssistantTextDelta)> {
        let parsers_by_item = std::mem::take(&mut self.parsers_by_item);
        parsers_by_item
            .into_iter()
            .map(|(item_id, mut parser)| (item_id, parser.finish()))
            .collect()
    }
}

impl ProposedPlanItemState {
    fn new(turn_id: &str) -> Self {
        Self {
            item_id: format!("{turn_id}-plan"),
            started: false,
            completed: false,
            streamed_text: String::new(),
        }
    }

    async fn start(&mut self, sess: &Session, turn_context: &TurnContext) {
        if self.started || self.completed {
            return;
        }
        self.started = true;
        let item = TurnItem::Plan(PlanItem {
            id: self.item_id.clone(),
            text: String::new(),
        });
        sess.emit_turn_item_started(turn_context, &item).await;
    }

    async fn push_delta(&mut self, sess: &Session, turn_context: &TurnContext, delta: &str) {
        // A delta without a preceding ItemStarted event produces an invalid
        // client-visible item stream. Treat both out-of-phase cases as inert.
        if !self.started || self.completed {
            return;
        }
        if delta.is_empty() {
            return;
        }
        self.streamed_text.push_str(delta);
        let event = PlanDeltaEvent {
            thread_id: sess.thread_id.to_string(),
            turn_id: turn_context.sub_id.clone(),
            item_id: self.item_id.clone(),
            delta: delta.to_string(),
        };
        sess.send_event(turn_context, EventMsg::PlanDelta(event))
            .await;
    }

    async fn complete_with_text(
        &mut self,
        sess: &Session,
        turn_context: &TurnContext,
        text: String,
    ) {
        if self.completed || !self.started {
            return;
        }
        self.completed = true;
        let item = TurnItem::Plan(PlanItem {
            id: self.item_id.clone(),
            text,
        });
        sess.emit_turn_item_completed(turn_context, item).await;
    }
}

/// In plan mode we defer agent message starts until the parser emits non-plan
/// text. The parser buffers each line until it can rule out a tag prefix, so
/// plan-only outputs never show up as empty assistant messages.
async fn maybe_emit_pending_agent_message_start(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
) {
    if state.started_agent_message_items.contains_key(item_id) {
        return;
    }
    if let Some(item) = state.pending_agent_message_items.remove(item_id) {
        sess.emit_turn_item_started(turn_context, &item).await;
        state
            .started_agent_message_items
            .insert(item_id.to_string(), item);
    }
}

/// Agent messages are text-only today; concatenate all text entries.
fn agent_message_text(item: &codex_protocol::items::AgentMessageItem) -> String {
    item.content
        .iter()
        .map(|entry| match entry {
            codex_protocol::items::AgentMessageContent::Text { text } => text.as_str(),
        })
        .collect()
}

// A first prose batch can await ItemStarted publication after extracting buffered
// whitespace. Retain that accepted batch across sampling-worker abort, including
// the interval where its returned state is queued but the caller has not resumed.
struct OwnedPlanStreamState {
    state: Option<PlanModeStreamState>,
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    in_flight: Option<crate::session::DurableHistoryCommitInFlight>,
    runtime: tokio::runtime::Handle,
}

impl Drop for OwnedPlanStreamState {
    fn drop(&mut self) {
        let Some(mut state) = self.state.take() else {
            return;
        };
        let session = Arc::clone(&self.session);
        let turn = Arc::clone(&self.turn);
        let in_flight = self.in_flight.take();
        self.runtime.spawn(async move {
            let _in_flight = in_flight;
            complete_partial_plan_items(&session, &turn, &mut state).await;
        });
    }
}

pub(super) async fn complete_partial_plan_items(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
) {
    for (_, mut item) in std::mem::take(&mut state.started_agent_message_items) {
        if let TurnItem::AgentMessage(message) = &mut item {
            message.phase = None;
            message.interruption = Some("Stream interrupted before provider item finalization".to_string());
        }
        sess.emit_turn_item_completed(turn_context, item).await;
    }
    let plan_text = std::mem::take(&mut state.plan_item_state.streamed_text);
    state
        .plan_item_state
        .complete_with_text(sess, turn_context, plan_text)
        .await;
}

#[expect(
    clippy::expect_used,
    reason = "Owned state exists until its single handoff; a failed publication task cannot supply a replacement state"
)]
async fn handle_plan_segments(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    state: &mut PlanModeStreamState,
    item_id: &str,
    segments: Vec<ProposedPlanSegment>,
) {
    let starts_prose = !state.started_agent_message_items.contains_key(item_id)
        && segments.iter().any(|segment| {
            matches!(segment,
            ProposedPlanSegment::Normal(delta) if delta.chars().any(|ch| !ch.is_whitespace()))
        });
    if !starts_prose {
        handle_plan_segments_inner(sess, turn_context, state, item_id, segments).await;
        return;
    }
    sess.durable_history_commits_in_flight
        .fetch_add(1, Ordering::AcqRel);
    let mut owned = OwnedPlanStreamState {
        state: Some(std::mem::replace(
            state,
            PlanModeStreamState::new(&turn_context.sub_id),
        )),
        session: Arc::clone(sess),
        turn: Arc::clone(turn_context),
        in_flight: Some(crate::session::DurableHistoryCommitInFlight {
            session: Arc::clone(sess),
        }),
        runtime: tokio::runtime::Handle::current(),
    };
    let session = Arc::clone(sess);
    let turn = Arc::clone(turn_context);
    let item_id = item_id.to_string();
    let (send_state, receive_state) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        handle_plan_segments_inner(
            &session,
            &turn,
            owned.state.as_mut().expect("owned plan state"),
            &item_id,
            segments,
        )
        .await;
        // On an abandoned receiver, Drop closes the actual published prefix.
        let _ = send_state.send(owned);
    });
    let mut returned = receive_state
        .await
        .expect("owned plan publication task must return its state");
    *state = returned.state.take().expect("returned plan state");
}

/// Split the stream into normal assistant text vs. proposed plan content.
/// Normal text becomes AgentMessage deltas; plan content becomes PlanDelta +
/// TurnItem::Plan.
async fn handle_plan_segments_inner(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
    segments: Vec<ProposedPlanSegment>,
) {
    for segment in segments {
        match segment {
            ProposedPlanSegment::Normal(delta) => {
                if delta.is_empty() {
                    continue;
                }
                let has_non_whitespace = delta.chars().any(|ch| !ch.is_whitespace());
                if !has_non_whitespace && !state.started_agent_message_items.contains_key(item_id) {
                    let entry = state
                        .leading_whitespace_by_item
                        .entry(item_id.to_string())
                        .or_default();
                    entry.push_str(&delta);
                    continue;
                }
                let delta = if !state.started_agent_message_items.contains_key(item_id) {
                    if let Some(prefix) = state.leading_whitespace_by_item.remove(item_id) {
                        format!("{prefix}{delta}")
                    } else {
                        delta
                    }
                } else {
                    delta
                };
                maybe_emit_pending_agent_message_start(sess, turn_context, state, item_id).await;

                let event = AgentMessageContentDeltaEvent {
                    thread_id: sess.thread_id.to_string(),
                    turn_id: turn_context.sub_id.clone(),
                    item_id: item_id.to_string(),
                    delta,
                };
                if let Some(item) = state.started_agent_message_items.get_mut(item_id) {
                    apply_partial_agent_message_delta(item, &event);
                }
                sess.send_event(turn_context, EventMsg::AgentMessageContentDelta(event))
                    .await;
            }
            ProposedPlanSegment::ProposedPlanStart => {
                if !state.plan_item_state.completed {
                    state.plan_item_state.start(sess, turn_context).await;
                }
            }
            ProposedPlanSegment::ProposedPlanDelta(delta) => {
                if !state.plan_item_state.completed {
                    if !state.plan_item_state.started {
                        state.plan_item_state.start(sess, turn_context).await;
                    }
                    state
                        .plan_item_state
                        .push_delta(sess, turn_context, &delta)
                        .await;
                }
            }
            ProposedPlanSegment::ProposedPlanEnd => {}
        }
    }
}

pub(super) fn append_partial_reasoning_text(parts: &mut Vec<String>, index: i64, delta: &str) {
    let Ok(index) = usize::try_from(index) else {
        return;
    };
    if let Some(part) = parts.get_mut(index) {
        part.push_str(delta);
    } else if index == parts.len() {
        parts.push(delta.to_string());
    }
}

pub(super) fn apply_partial_agent_message_delta(item: &mut TurnItem, event: &AgentMessageContentDeltaEvent) {
    let TurnItem::AgentMessage(message) = item else {
        return;
    };
    if !event.delta.is_empty() {
        match message.content.last_mut() {
            Some(codex_protocol::items::AgentMessageContent::Text { text }) => {
                text.push_str(&event.delta);
            }
            None => message
                .content
                .push(codex_protocol::items::AgentMessageContent::Text {
                    text: event.delta.clone(),
                }),
        }
    }
}

pub(super) async fn emit_streamed_assistant_text_delta(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    plan_mode_state: Option<&mut PlanModeStreamState>,
    item_id: &str,
    parsed: ParsedAssistantTextDelta,
) -> Option<AgentMessageContentDeltaEvent> {
    if parsed.is_empty() {
        return None;
    }
    if let Some(state) = plan_mode_state {
        if !parsed.plan_segments.is_empty() {
            handle_plan_segments(sess, turn_context, state, item_id, parsed.plan_segments).await;
        }
        return None;
    }
    if parsed.visible_text.is_empty() {
        return None;
    }
    let event = AgentMessageContentDeltaEvent {
        thread_id: sess.thread_id.to_string(),
        turn_id: turn_context.sub_id.clone(),
        item_id: item_id.to_string(),
        delta: parsed.visible_text,
    };
    sess.send_event(
        turn_context,
        EventMsg::AgentMessageContentDelta(event.clone()),
    )
    .await;
    Some(event)
}

/// Flush buffered assistant text parser state when an assistant message item ends.
pub(super) async fn flush_assistant_text_segments_for_item(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    plan_mode_state: Option<&mut PlanModeStreamState>,
    parsers: &mut AssistantMessageStreamParsers,
    item_id: &str,
) {
    let parsed = parsers.finish_item(item_id);
    let _ =
        emit_streamed_assistant_text_delta(sess, turn_context, plan_mode_state, item_id, parsed)
            .await;
}

/// Flush any remaining buffered assistant text parser state at response completion.
pub(super) async fn flush_assistant_text_segments_all(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    mut plan_mode_state: Option<&mut PlanModeStreamState>,
    parsers: &mut AssistantMessageStreamParsers,
) -> Vec<AgentMessageContentDeltaEvent> {
    let mut emitted = Vec::new();
    for (item_id, parsed) in parsers.drain_finished() {
        if let Some(event) = emit_streamed_assistant_text_delta(
            sess,
            turn_context,
            plan_mode_state.as_deref_mut(),
            &item_id,
            parsed,
        )
        .await
        {
            emitted.push(event);
        }
    }
    emitted
}

/// Emit completion for plan items by parsing the finalized assistant message.
async fn maybe_complete_plan_item_from_message(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item: &ResponseItem,
) {
    if let ResponseItem::Message { role, content, .. } = item
        && role == "assistant"
    {
        let mut text = String::new();
        for entry in content {
            if let ContentItem::OutputText { text: chunk } = entry {
                text.push_str(chunk);
            }
        }
        if let Some(plan_text) = extract_proposed_plan_text(&text) {
            if !state.plan_item_state.started {
                state.plan_item_state.start(sess, turn_context).await;
            }
            state
                .plan_item_state
                .complete_with_text(sess, turn_context, plan_text)
                .await;
        }
    }
}

/// Emit a completed agent message in plan mode, respecting deferred starts.
async fn emit_agent_message_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    agent_message: codex_protocol::items::AgentMessageItem,
    state: &mut PlanModeStreamState,
) {
    let agent_message_id = agent_message.id.clone();
    let text = agent_message_text(&agent_message);
    if text.trim().is_empty() {
        state.pending_agent_message_items.remove(&agent_message_id);
        if let Some(started_item) = state.started_agent_message_items.remove(&agent_message_id) {
            sess.emit_turn_item_completed(turn_context, started_item)
                .await;
        }
        return;
    }

    maybe_emit_pending_agent_message_start(sess, turn_context, state, &agent_message_id).await;

    if !state
        .started_agent_message_items
        .contains_key(&agent_message_id)
    {
        let start_item = state
            .pending_agent_message_items
            .remove(&agent_message_id)
            .unwrap_or_else(|| {
                TurnItem::AgentMessage(codex_protocol::items::AgentMessageItem {
                    id: agent_message_id.clone(),
                    content: Vec::new(),
                    phase: None,
                    interruption: None,
                })
            });
        sess.emit_turn_item_started(turn_context, &start_item).await;
        state
            .started_agent_message_items
            .insert(agent_message_id.clone(), start_item);
    }

    sess.emit_turn_item_completed(turn_context, TurnItem::AgentMessage(agent_message))
        .await;
    state.started_agent_message_items.remove(&agent_message_id);
}

/// Emit completion for a plan-mode turn item, handling agent messages specially.
async fn emit_turn_item_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    turn_item: TurnItem,
    previously_active_item: Option<&TurnItem>,
    state: &mut PlanModeStreamState,
) {
    match turn_item {
        TurnItem::AgentMessage(agent_message) => {
            emit_agent_message_in_plan_mode(sess, turn_context, agent_message, state).await;
        }
        _ => {
            if previously_active_item.is_none() {
                sess.emit_turn_item_started(turn_context, &turn_item).await;
            }
            sess.emit_turn_item_completed(turn_context, turn_item).await;
        }
    }
}

/// Handle a completed assistant response item in plan mode, returning true if handled.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_assistant_item_done_in_plan_mode(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    turn_store: &codex_extension_api::ExtensionData,
    item: &ResponseItem,
    state: &mut PlanModeStreamState,
    previously_active_item: Option<&TurnItem>,
    last_agent_message: &mut Option<String>,
    assistant_output: &mut Option<FinalizedAssistantOutput>,
    final_message_candidate: &mut Option<codex_protocol::protocol::AcceptedFinalMessage>,
) -> std::io::Result<bool> {
    if let ResponseItem::Message { role, .. } = item
        && role == "assistant"
    {
        let finalized_turn_item = finalize_non_tool_response_item(
            sess,
            TurnItemContributorPolicy::Run(turn_store),
            item,
            /*plan_mode*/ true,
        )
        .await;
        let finalized_facts = finalized_turn_item.as_ref().map(|item| item.facts.clone());
        record_completed_response_item_with_finalized_facts(
            sess, turn_context, item, finalized_facts.as_ref(),
        ).await?;
        maybe_complete_plan_item_from_message(sess, turn_context, state, item).await;
        if let Some(finalized_turn_item) = finalized_turn_item {
            emit_turn_item_in_plan_mode(
                sess,
                turn_context,
                finalized_turn_item.turn_item,
                previously_active_item,
                state,
            )
            .await;
        }
        let final_last_agent_message = finalized_facts
            .as_ref()
            .and_then(|facts| facts.last_agent_message.clone());
        if let Some(candidate) = finalized_facts
            .as_ref()
            .and_then(|facts| facts.assistant_output.clone())
            .and_then(final_message_candidate_from_output)
        {
            update_final_message_candidate(final_message_candidate, candidate);
        } else if final_message_candidate.is_none()
            && let ResponseItem::Message { content, .. } = item
        {
            let text = content
                .iter()
                .filter_map(|entry| match entry {
                    ContentItem::OutputText { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            if let Some(text) = extract_proposed_plan_text(&text) {
                *final_message_candidate = Some(codex_protocol::protocol::AcceptedFinalMessage {
                    item_id: state.plan_item_state.item_id.clone(),
                    text,
                    origin: codex_protocol::protocol::AcceptedFinalMessageOrigin::Plan,
                    completion_attempt_id: String::new(),
                });
            }
        }

        if let Some(agent_message) = final_last_agent_message {
            *last_agent_message = Some(agent_message);
        }
        if let Some(output) = finalized_facts.and_then(|facts| facts.assistant_output) {
            *assistant_output = Some(output);
        }
        return Ok(true);
    }
    Ok(false)
}

