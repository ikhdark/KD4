use crate::protocol::item_builders::build_command_execution_begin_item;
use crate::protocol::item_builders::build_command_execution_end_item;
use crate::protocol::item_builders::build_file_change_approval_request_item;
use crate::protocol::item_builders::build_file_change_begin_item;
use crate::protocol::item_builders::build_file_change_end_item;
use crate::protocol::item_builders::review_output_text;
use crate::protocol::v2::CollabAgentState;
use crate::protocol::v2::CollabAgentTool;
use crate::protocol::v2::CollabAgentToolCallStatus;
use crate::protocol::v2::DynamicToolCallOutputContentItem;
use crate::protocol::v2::DynamicToolCallStatus;
use crate::protocol::v2::McpToolCallAppContext;
use crate::protocol::v2::McpToolCallError;
use crate::protocol::v2::McpToolCallResult;
use crate::protocol::v2::McpToolCallStatus;
use crate::protocol::v2::SurfacedToolResult;
use crate::protocol::v2::ThreadItem;
use crate::protocol::v2::Turn;
use crate::protocol::v2::TurnError as V2TurnError;
use crate::protocol::v2::TurnError;
use crate::protocol::v2::TurnItemsView;
use crate::protocol::v2::TurnStatus;
use crate::protocol::v2::TurnTiming;
use crate::protocol::v2::UserInput;
use crate::protocol::v2::WebSearchItem;
use crate::protocol::v2::web_search_action_from_core;
use codex_extension_items::image_generation::ImageGenerationItem;
use codex_protocol::items::parse_hook_prompt_message;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::AgentReasoningEvent;
use codex_protocol::protocol::AgentReasoningRawContentEvent;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::ContextCompactedEvent;
use codex_protocol::protocol::DynamicToolCallResponseEvent;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandBeginEvent;
use codex_protocol::protocol::ExecCommandEndEvent;
use codex_protocol::protocol::ImageGenerationBeginEvent;
use codex_protocol::protocol::ImageGenerationEndEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::McpToolCallBeginEvent;
use codex_protocol::protocol::McpToolCallEndEvent;
use codex_protocol::protocol::PatchApplyBeginEvent;
use codex_protocol::protocol::PatchApplyEndEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::protocol::ViewImageToolCallEvent;
use codex_protocol::protocol::WebSearchBeginEvent;
use codex_protocol::protocol::WebSearchEndEvent;
use indexmap::IndexSet;
use std::collections::HashMap;
use tracing::warn;
use uuid::Uuid;


/// Convert persisted [`RolloutItem`] entries into a sequence of [`Turn`] values.
///
/// Explicit lifecycle events preserve the original turn identifiers; legacy
/// histories without those boundaries receive deterministic rollout-based IDs.
pub fn build_turns_from_rollout_items(items: &[RolloutItem]) -> Vec<Turn> {
    let mut builder = ThreadHistoryBuilder::new();
    for item in items {
        builder.handle_rollout_item(item);
    }
    builder.finish()
}

/// Replay the same structural history while omitting tool payloads that this view cannot return.
/// Item slots remain present until replay finishes: legacy boundaries, synthetic IDs,
/// rollback and late scoped updates all depend on their identities and positions.
pub fn build_turns_from_rollout_items_with_items_view<'a>(
    items: impl IntoIterator<Item = &'a RolloutItem>,
    items_view: TurnItemsView,
) -> Vec<Turn> {
    let mut builder = ThreadHistoryBuilder::new();
    builder.omit_tool_payloads = items_view != TurnItemsView::Full;
    for item in items {
        builder.handle_rollout_item(item);
    }
    let mut turns = builder.finish();
    for turn in &mut turns {
        match items_view {
            TurnItemsView::Full => {}
            TurnItemsView::NotLoaded => turn.items = Vec::new(),
            TurnItemsView::Summary => {
                let user = turn.items.iter().position(|item| matches!(item, ThreadItem::UserMessage { .. }));
                let agent = turn.items.iter().rposition(|item| matches!(item, ThreadItem::AgentMessage { interruption: None, .. }))
                    .filter(|&index| user.is_none_or(|user| turn.items[user].id() != turn.items[index].id()));
                let (mut first_user, mut last_agent) = (None, None);
                for (index, item) in std::mem::take(&mut turn.items).into_iter().enumerate() {
                    if Some(index) == user {
                        first_user = Some(item);
                    } else if Some(index) == agent {
                        last_agent = Some(item);
                    }
                }
                turn.items = first_user.into_iter().chain(last_agent).collect();
            }
        }
        turn.items_view = items_view;
    }
    turns
}



/// A materialized `ThreadItem` snapshot that changed while handling one input.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadHistoryItemChange {
    pub turn_id: String,
    pub item: ThreadItem,
}

/// Lightweight turn metadata snapshot for projectors that track turn status without
/// re-reading the full item list.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadHistoryTurnChange {
    pub turn_id: String,
    pub status: TurnStatus,
    pub error: Option<TurnError>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub timing: Option<TurnTiming>,
    pub surfaced_result: Option<SurfacedToolResult>,
}

/// Incremental changes produced by opt-in `ThreadHistoryBuilder` handlers.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ThreadHistoryChangeSet {
    pub changed_items: Vec<ThreadHistoryItemChange>,
    pub changed_turns: Vec<ThreadHistoryTurnChange>,
    pub removed_turn_ids: Vec<String>,
}

impl ThreadHistoryChangeSet {
    pub fn is_empty(&self) -> bool {
        self.changed_items.is_empty()
            && self.changed_turns.is_empty()
            && self.removed_turn_ids.is_empty()
    }
}

impl ThreadHistoryTurnChange {
    fn from_pending_turn(turn: &PendingTurn) -> Self {
        Self {
            turn_id: turn.id.clone(),
            status: turn.status.clone(),
            error: turn.error.clone(),
            started_at: turn.started_at,
            completed_at: turn.completed_at,
            duration_ms: turn.duration_ms,
            timing: turn.timing.clone(),
            surfaced_result: turn.surfaced_result.clone(),
        }
    }

    fn from_turn(turn: &Turn) -> Self {
        Self {
            turn_id: turn.id.clone(),
            status: turn.status.clone(),
            error: turn.error.clone(),
            started_at: turn.started_at,
            completed_at: turn.completed_at,
            duration_ms: turn.duration_ms,
            timing: turn.timing.clone(),
            surfaced_result: turn.surfaced_result.clone(),
        }
    }
}

/// Track positions while reducing a batch, then copy each final snapshot once.
/// Ordered sets preserve first-change order; rollback removes invalid positions.
#[derive(Default)]
struct ThreadHistoryChangeAccumulator {
    changed_items: IndexSet<(usize, usize)>,
    changed_turns: IndexSet<usize>,
    removed_turn_ids: IndexSet<String>,
}

impl ThreadHistoryChangeAccumulator {
    #[expect(
        clippy::expect_used,
        reason = "The accumulator records live turn positions and rollback removes invalid positions before finishing"
    )]
    fn finish(self, builder: &ThreadHistoryBuilder) -> ThreadHistoryChangeSet {
        let changed_items = self
            .changed_items
            .into_iter()
            .map(|(turn_index, item_index)| {
                let (turn_id, items) = if turn_index == builder.turns.len() {
                    let turn = builder.current_turn.as_ref().expect("changed current turn");
                    (&turn.id, &turn.items)
                } else {
                    let turn = &builder.turns[turn_index];
                    (&turn.id, &turn.items)
                };
                ThreadHistoryItemChange {
                    turn_id: turn_id.clone(),
                    item: items[item_index].clone(),
                }
            })
            .collect();
        let changed_turns = self
            .changed_turns
            .into_iter()
            .map(|turn_index| {
                if turn_index == builder.turns.len() {
                    ThreadHistoryTurnChange::from_pending_turn(
                        builder.current_turn.as_ref().expect("changed current turn"),
                    )
                } else {
                    ThreadHistoryTurnChange::from_turn(&builder.turns[turn_index])
                }
            })
            .collect();
        ThreadHistoryChangeSet {
            changed_items,
            changed_turns,
            removed_turn_ids: self.removed_turn_ids.into_iter().collect(),
        }
    }
}

pub struct ThreadHistoryBuilder {
    omit_tool_payloads: bool,
    turns: Vec<Turn>,
    current_turn: Option<PendingTurn>,
    next_item_index: i64,
    pending_legacy_item_mirror: Option<ThreadItem>,
    pending_legacy_user_mirror: Option<UserMessageEvent>,
    rollback_eligible_turn_ids: std::collections::HashSet<String>,
    legacy_inter_agent_mail: codex_context_fragments::LegacyInterAgentMailReader,
    // Remember only removed wait identities so end-only legacy compatibility
    // cannot resurrect an operation that an explicit rollback discarded.
    rolled_back_wait_ids: std::collections::HashSet<String>,
    current_rollout_index: usize,
    next_rollout_index: usize,
    active_change_set: Option<ThreadHistoryChangeAccumulator>,
}

impl Default for ThreadHistoryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadHistoryBuilder {
    /// Legacy ends have no turn ID; every collaboration family needs the same
    /// retained-owner and rollback checks as Wait.
    pub fn legacy_collab_end_identity(event: &EventMsg) -> Option<(&str, CollabAgentTool)> {
        match event {
            EventMsg::CollabAgentSpawnEnd(p) => Some((&p.call_id, CollabAgentTool::SpawnAgent)),
            EventMsg::CollabAgentInteractionEnd(p) => {
                Some((&p.call_id, CollabAgentTool::SendInput))
            }
            EventMsg::CollabWaitingEnd(p) => Some((&p.call_id, CollabAgentTool::Wait)),
            EventMsg::CollabCloseEnd(p) => Some((&p.call_id, CollabAgentTool::CloseAgent)),
            EventMsg::CollabResumeEnd(p) => Some((&p.call_id, CollabAgentTool::ResumeAgent)),
            _ => None,
        }
    }

    pub fn new() -> Self {
        Self {
            omit_tool_payloads: false,
            turns: Vec::new(),
            current_turn: None,
            next_item_index: 1,
            pending_legacy_item_mirror: None,
            pending_legacy_user_mirror: None,
            rollback_eligible_turn_ids: std::collections::HashSet::new(),
            legacy_inter_agent_mail: Default::default(),
            rolled_back_wait_ids: std::collections::HashSet::new(),
            current_rollout_index: 0,
            next_rollout_index: 0,
            active_change_set: None,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn is_rollback_eligible_turn(&self, turn_id: &str) -> bool {
        self.rollback_eligible_turn_ids.contains(turn_id)
    }

    /// The live listener supplies the envelope's turn ID; raw payloads do not
    /// carry it themselves. Return eligibility even if an external index owns
    /// the turn, but never materialize an unknown/rolled-back owner here.
    pub fn record_raw_user_turn_boundary(
        &mut self,
        turn_id: &str,
        item: &codex_protocol::models::ResponseItem,
    ) -> bool {
        let eligible = codex_context_fragments::is_user_turn_boundary(item);
        if eligible && self.contains_turn(turn_id) {
            self.rollback_eligible_turn_ids.insert(turn_id.to_string());
        }
        eligible
    }

    fn mark_user_turn(&mut self) {
        let id = self.ensure_turn().id.clone();
        self.rollback_eligible_turn_ids.insert(id);
        self.record_changed_turn(self.turns.len());
    }

    pub fn finish(mut self) -> Vec<Turn> {
        self.finish_current_turn();
        self.turns
    }

    pub fn active_turn_snapshot(&self) -> Option<Turn> {
        self.current_turn
            .as_ref()
            .map(Turn::from)
            .or_else(|| self.turns.last().cloned())
    }

    /// Lifecycle-only consumers need not clone the active turn's item payloads.
    pub fn active_turn_status(&self) -> Option<TurnStatus> {
        self.current_turn.as_ref().map(|turn| turn.status.clone())
            .or_else(|| self.turns.last().map(|turn| turn.status.clone()))
    }

    /// Returns the current turn only while it is still in progress.
    ///
    /// Interrupted turns intentionally remain open briefly so late items can still be grouped
    /// into the correct turn. Consumers making lifecycle decisions must not treat that retained
    /// presentation snapshot as live state.
    pub fn in_progress_turn_snapshot(&self) -> Option<Turn> {
        match self.current_turn.as_ref() {
            Some(turn) => (turn.status == TurnStatus::InProgress).then(|| Turn::from(turn)),
            None => self
                .turns
                .last()
                .filter(|turn| turn.status == TurnStatus::InProgress)
                .cloned(),
        }
    }

    /// Returns the current turn id only while the turn is still in progress.
    pub fn in_progress_turn_id(&self) -> Option<&str> {
        self.current_turn
            .as_ref()
            .filter(|turn| turn.status == TurnStatus::InProgress)
            .map(|turn| turn.id.as_str())
    }

    /// Returns the active turn id without materializing a turn snapshot.
    pub fn active_turn_id(&self) -> Option<&str> {
        self.current_turn
            .as_ref()
            .map(|turn| turn.id.as_str())
            .or_else(|| self.turns.last().map(|turn| turn.id.as_str()))
    }

    /// Returns the index of the active turn snapshot within the finished turn list.
    ///
    /// When a turn is still open, this is the index it will occupy after
    /// `finish`. When no turn is open, it is the index of the last finished turn.
    pub fn active_turn_position(&self) -> Option<usize> {
        if self.current_turn.is_some() {
            Some(self.turns.len())
        } else if self.turns.is_empty() {
            None
        } else {
            Some(self.turns.len() - 1)
        }
    }

    pub fn has_active_turn(&self) -> bool {
        self.current_turn.is_some()
    }

    pub fn active_turn_id_if_explicit(&self) -> Option<String> {
        self.current_turn
            .as_ref()
            .filter(|turn| turn.opened_explicitly)
            .map(|turn| turn.id.clone())
    }

    pub fn active_turn_start_index(&self) -> Option<usize> {
        self.current_turn
            .as_ref()
            .map(|turn| turn.rollout_start_index)
    }

    /// Shared reducer for persisted rollout replay and in-memory current-turn
    /// tracking used by running thread resume/rejoin.
    ///
    /// This function should handle all EventMsg variants that can be persisted in a rollout file.
    /// See `should_persist_event_msg` in `codex-rs/core/rollout/policy.rs`.
    pub fn handle_event(&mut self, event: &EventMsg) {
        if let Some(mirror) = self.pending_legacy_user_mirror.take()
            && let EventMsg::UserMessage(payload) = event
            && payload.client_id == mirror.client_id
            && payload.message == mirror.message
            && payload.images == mirror.images
            && payload.image_details == mirror.image_details
            && payload.local_images == mirror.local_images
            && payload.local_image_details == mirror.local_image_details
            && payload.text_elements == mirror.text_elements
        {
            return;
        }
        // Core emits the legacy compatibility event immediately after its canonical item.
        // Consume only that mirror; later messages and compactions remain distinct items.
        if let Some(item) = self.pending_legacy_item_mirror.take() {
            let is_mirror = match (&item, event) {
                (ThreadItem::AgentMessage { text, phase, .. }, EventMsg::AgentMessage(payload)) => {
                    payload.message == *text && payload.phase == *phase
                }
                (ThreadItem::ContextCompaction { .. }, EventMsg::ContextCompacted(_)) => true,
                (ThreadItem::WebSearch(item), EventMsg::WebSearchEnd(payload)) => {
                    item.id == payload.call_id
                        && item.query == payload.query
                        && item.action == Some(web_search_action_from_core(payload.action.clone()))
                }
                _ => false,
            };
            if is_mirror {
                return;
            }
        }
        match event {
            EventMsg::UserMessage(payload) => self.handle_user_message(payload),
            EventMsg::AgentMessage(payload) => {
                self.handle_agent_message(payload.message.clone(), payload.phase.clone())
            }
            EventMsg::AgentReasoning(payload) => self.handle_agent_reasoning(payload),
            EventMsg::AgentReasoningRawContent(payload) => {
                self.handle_agent_reasoning_raw_content(payload)
            }
            EventMsg::WebSearchBegin(payload) => self.handle_web_search_begin(payload),
            EventMsg::WebSearchEnd(payload) => self.handle_web_search_end(payload),
            EventMsg::ExecCommandBegin(payload) => self.handle_exec_command_begin(payload),
            EventMsg::ExecCommandEnd(payload) => self.handle_exec_command_end(payload),
            EventMsg::ApplyPatchApprovalRequest(payload) => {
                self.handle_apply_patch_approval_request(payload)
            }
            EventMsg::PatchApplyBegin(payload) => self.handle_patch_apply_begin(payload),
            EventMsg::PatchApplyEnd(payload) => self.handle_patch_apply_end(payload),
            EventMsg::DynamicToolCallRequest(payload) => {
                self.handle_dynamic_tool_call_request(payload)
            }
            EventMsg::DynamicToolCallResponse(payload) => {
                self.handle_dynamic_tool_call_response(payload)
            }
            EventMsg::McpToolCallBegin(payload) => self.handle_mcp_tool_call_begin(payload),
            EventMsg::McpToolCallEnd(payload) => self.handle_mcp_tool_call_end(payload),
            EventMsg::ViewImageToolCall(payload) => self.handle_view_image_tool_call(payload),
            EventMsg::ImageGenerationBegin(payload) => self.handle_image_generation_begin(payload),
            EventMsg::ImageGenerationEnd(payload) => self.handle_image_generation_end(payload),
            EventMsg::CollabAgentSpawnBegin(payload) => {
                self.handle_collab_agent_spawn_begin(payload)
            }
            EventMsg::CollabAgentSpawnEnd(payload) => self.handle_collab_agent_spawn_end(payload),
            EventMsg::CollabAgentInteractionBegin(payload) => {
                self.handle_collab_agent_interaction_begin(payload)
            }
            EventMsg::CollabAgentInteractionEnd(payload) => {
                self.handle_collab_agent_interaction_end(payload)
            }
            EventMsg::SubAgentActivity(payload) => self.handle_sub_agent_activity(payload),
            EventMsg::CollabWaitingBegin(payload) => self.handle_collab_waiting_begin(payload),
            EventMsg::CollabWaitingEnd(payload) => self.handle_collab_waiting_end(payload),
            EventMsg::CollabCloseBegin(payload) => self.handle_collab_close_begin(payload),
            EventMsg::CollabCloseEnd(payload) => self.handle_collab_close_end(payload),
            EventMsg::CollabResumeBegin(payload) => self.handle_collab_resume_begin(payload),
            EventMsg::CollabResumeEnd(payload) => self.handle_collab_resume_end(payload),
            EventMsg::ContextCompacted(payload) => self.handle_context_compacted(payload),
            EventMsg::EnteredReviewMode(payload) => self.handle_entered_review_mode(payload),
            EventMsg::ExitedReviewMode(payload) => self.handle_exited_review_mode(payload),
            EventMsg::ItemStarted(payload) => self.handle_item_started(payload),
            EventMsg::ItemCompleted(payload) => self.handle_item_completed(payload),
            EventMsg::HookStarted(_) | EventMsg::HookCompleted(_) => {}
            EventMsg::Error(payload) => self.handle_error(payload),
            EventMsg::TokenCount(_) => {}
            EventMsg::ThreadRolledBack(payload) => self.handle_thread_rollback(payload),
            EventMsg::TurnAborted(payload) => self.handle_turn_aborted(payload),
            EventMsg::TurnStarted(payload) => self.handle_turn_started(payload),
            EventMsg::TurnComplete(payload) => self.handle_turn_complete(payload),
            EventMsg::ImportedTurnClosed(payload) => {
                self.handle_turn_complete(payload);
                if let Some(index) = self
                    .turns
                    .iter()
                    .position(|turn| turn.id == payload.turn_id)
                {
                    // The stable wire status stays `completed`; completion facts
                    // the imported source never asserted remain absent.
                    let turn = &mut self.turns[index];
                    turn.status = TurnStatus::Completed;
                    turn.error = None;
                    turn.completed_at = None;
                    turn.duration_ms = None;
                    turn.timing = None;
                    turn.surfaced_result = None;
                    self.record_changed_turn(index);
                }
            }
            _ => {}
        }
    }

    /// Handles one live event and returns its incremental history projection.
    pub fn handle_event_with_changes(&mut self, event: &EventMsg) -> ThreadHistoryChangeSet {
        self.collect_changes(|builder| builder.handle_event(event))
    }

    /// Explicit turn ownership carried by item and terminal lifecycle events.
    pub fn scoped_event_turn_id(event: &EventMsg) -> Option<&str> {
        match event {
            EventMsg::ItemStarted(value) => Some(value.turn_id.as_str()),
            EventMsg::ItemCompleted(value) => Some(value.turn_id.as_str()),
            EventMsg::ExecCommandBegin(value) => Some(value.turn_id.as_str()),
            EventMsg::ExecCommandEnd(value) => Some(value.turn_id.as_str()),
            EventMsg::ApplyPatchApprovalRequest(value) => Some(value.turn_id.as_str()),
            EventMsg::PatchApplyBegin(value) => Some(value.turn_id.as_str()),
            EventMsg::PatchApplyEnd(value) => Some(value.turn_id.as_str()),
            EventMsg::DynamicToolCallRequest(value) => Some(value.turn_id.as_str()),
            EventMsg::DynamicToolCallResponse(value) => Some(value.turn_id.as_str()),
            EventMsg::TurnComplete(value) | EventMsg::ImportedTurnClosed(value) => {
                Some(value.turn_id.as_str())
            }
            EventMsg::TurnAborted(value) => value.turn_id.as_deref(),
            EventMsg::EnteredReviewMode(value) => value.turn_id.as_deref(),
            EventMsg::ExitedReviewMode(value) => value.turn_id.as_deref(),
            _ => None,
        }
        .filter(|id| !id.is_empty())
    }

    pub fn contains_turn(&self, turn_id: &str) -> bool {
        self.current_turn
            .as_ref()
            .is_some_and(|turn| turn.id == turn_id)
            || self.turns.iter().any(|turn| turn.id == turn_id)
    }

    /// Reduce a late scoped event against a turn retained by an external index.
    /// Moves its item buffer temporarily, rather than retaining or cloning a
    /// second copy of the index's completed history. The current turn and mirror
    /// suppression state remain owned by this reducer.
    pub fn handle_event_with_retained_turn(
        &mut self,
        event: &EventMsg,
        turn: &mut Turn,
    ) -> ThreadHistoryChangeSet {
        assert!(
            Self::scoped_event_turn_id(event) == Some(turn.id.as_str())
                || Self::legacy_collab_end_identity(event).is_some_and(|(call_id, operation)| turn
                    .items
                    .iter()
                    .any(|item| matches!(item,
                    ThreadItem::CollabAgentToolCall { id, tool, .. }
                    if id == call_id && *tool == operation)))
        );
        assert!(!self.contains_turn(&turn.id));
        let items = std::mem::take(&mut turn.items);
        let mut retained = turn.clone();
        retained.items = items;
        let position = self.turns.len();
        self.turns.push(retained);
        let changes = self.handle_event_with_changes(event);
        *turn = self.turns.remove(position);
        changes
    }

    pub fn handle_rollout_item(&mut self, item: &RolloutItem) {
        self.current_rollout_index = self.next_rollout_index;
        self.next_rollout_index += 1;
        let legacy_inter_agent_mail = self.legacy_inter_agent_mail.observe(item).is_some();
        match item {
            RolloutItem::EventMsg(event) => self.handle_event(event),
            RolloutItem::Compacted(payload) => self.handle_compacted(payload),
            RolloutItem::ResponseItem(item) => {
                self.handle_response_item(item);
                // Mail recorded before typed delivery is rollback-eligible input,
                // exactly like a typed `InterAgentCommunication` record.
                if legacy_inter_agent_mail {
                    self.mark_user_turn();
                }
            }
            RolloutItem::InterAgentCommunication(_) => self.mark_user_turn(),
            RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::ToolManifest(_)
            | RolloutItem::SamplingBoundary(_)
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::AdditionalContextState { .. }
            | RolloutItem::SessionMeta(_) => {}
        }
    }

    /// Handles a rollout item and returns the materialized items or turn metadata
    /// changed by that one append.
    pub fn handle_rollout_item_with_changes(
        &mut self,
        item: &RolloutItem,
    ) -> ThreadHistoryChangeSet {
        self.collect_changes(|builder| builder.handle_rollout_item(item))
    }

    /// Handles rollout items in order and returns a coalesced end-of-batch
    /// change set. Multiple changes to the same item or turn are deduplicated
    /// so only the latest snapshot is emitted.
    pub fn handle_rollout_items_with_changes(
        &mut self,
        items: &[RolloutItem],
    ) -> ThreadHistoryChangeSet {
        self.collect_changes(|builder| {
            for item in items {
                builder.handle_rollout_item(item);
            }
        })
    }

    fn collect_changes(&mut self, handle: impl FnOnce(&mut Self)) -> ThreadHistoryChangeSet {
        debug_assert!(self.active_change_set.is_none());
        self.active_change_set = Some(ThreadHistoryChangeAccumulator::default());
        handle(self);
        self.active_change_set
            .take()
            .unwrap_or_default()
            .finish(self)
    }

    fn handle_response_item(&mut self, item: &codex_protocol::models::ResponseItem) {
        if codex_context_fragments::is_user_turn_boundary(item) {
            // Raw-only old histories lack explicit/user-event boundaries. Keep
            // their user units, but never split modern turns or legacy mirrors.
            if self.current_turn.as_ref().is_some_and(|turn| {
                !turn.opened_explicitly
                    && turn.raw_user_boundary
                    && !turn
                        .items
                        .iter()
                        .any(|item| matches!(item, ThreadItem::UserMessage { .. }))
            }) {
                self.finish_current_turn();
            }
            self.mark_user_turn();
            self.ensure_turn().raw_user_boundary = true;
        }
        let codex_protocol::models::ResponseItem::Message {
            role, content, id, ..
        } = item
        else {
            return;
        };

        if role != "user" {
            return;
        }

        let Some(hook_prompt) = parse_hook_prompt_message(id.as_deref(), content) else {
            return;
        };

        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_tool_item(hook_prompt.id));
            return;
        }
        self.upsert_item_in_current_turn(ThreadItem::HookPrompt {
            id: hook_prompt.id,
            fragments: hook_prompt
                .fragments
                .into_iter()
                .map(crate::protocol::v2::HookPromptFragment::from)
                .collect(),
        });
    }

    fn handle_user_message(&mut self, payload: &UserMessageEvent) {
        // User messages should stay in explicitly opened turns. For backward
        // compatibility with older streams that did not open turns explicitly,
        // close any implicit/inactive turn and start a fresh one for this input.
        if let Some(turn) = self.current_turn.as_ref()
            && !turn.opened_explicitly
            && !(turn.items.is_empty() && (turn.saw_compaction || turn.raw_user_boundary))
        {
            self.finish_current_turn();
        }
        let id = self.next_item_id();
        let content = self.build_user_inputs(payload);
        self.mark_user_turn();
        self.push_item_in_current_turn(ThreadItem::UserMessage {
            id,
            client_id: payload.client_id.clone(),
            content,
        });
        if let Some(turn) = self.current_turn.as_mut()
            && !turn.opened_explicitly
        {
            turn.status = TurnStatus::InProgress;
            self.record_changed_turn(self.turns.len());
        }
    }

    fn handle_agent_message(&mut self, text: String, phase: Option<MessagePhase>) {
        if text.is_empty() {
            return;
        }

        let id = self.next_item_id();
        self.push_item_in_current_turn(ThreadItem::AgentMessage { id, text, phase, interruption: None });
        if let Some(turn) = self.current_turn.as_mut()
            && !turn.opened_explicitly
            && turn.status == TurnStatus::InProgress
        {
            turn.status = TurnStatus::Completed;
            self.record_changed_turn(self.turns.len());
        }
    }

    fn handle_agent_reasoning(&mut self, payload: &AgentReasoningEvent) {
        if payload.text.is_empty() {
            return;
        }
        let mut text = if self.omit_tool_payloads { String::new() } else { payload.text.clone() };

        let updated_index = {
            let turn = self.ensure_turn();
            if let Some(ThreadItem::Reasoning { summary, .. }) = turn.items.last_mut() {
                summary.push(std::mem::take(&mut text));
                Some(turn.items.len() - 1)
            } else {
                None
            }
        };
        if let Some(item_index) = updated_index {
            self.record_changed_item(self.turns.len(), item_index);
            return;
        }

        // Otherwise, create a new reasoning item.
        let id = self.next_item_id();
        self.push_item_in_current_turn(ThreadItem::Reasoning {
            id,
            summary: vec![text],
            content: Vec::new(),
        });
    }

    fn handle_agent_reasoning_raw_content(&mut self, payload: &AgentReasoningRawContentEvent) {
        if payload.text.is_empty() {
            return;
        }
        let mut text = if self.omit_tool_payloads { String::new() } else { payload.text.clone() };

        let updated_index = {
            let turn = self.ensure_turn();
            if let Some(ThreadItem::Reasoning { content, .. }) = turn.items.last_mut() {
                content.push(std::mem::take(&mut text));
                Some(turn.items.len() - 1)
            } else {
                None
            }
        };
        if let Some(item_index) = updated_index {
            self.record_changed_item(self.turns.len(), item_index);
            return;
        }

        // Otherwise, create a new reasoning item.
        let id = self.next_item_id();
        self.push_item_in_current_turn(ThreadItem::Reasoning {
            id,
            summary: Vec::new(),
            content: vec![text],
        });
    }

    fn handle_item_started(&mut self, payload: &ItemStartedEvent) {
        // User input becomes canonical on ItemCompleted. A started item may
        // be empty and has no legacy-mirror receipt yet; publishing it here
        // duplicates the legacy UserMessage event in older/live histories.
        if matches!(
            &payload.item,
            codex_protocol::items::TurnItem::UserMessage(_)
        ) {
            return;
        }
        self.handle_materialized_item_lifecycle(&payload.turn_id, &payload.item);
    }

    fn handle_item_completed(&mut self, payload: &ItemCompletedEvent) {
        if let codex_protocol::items::TurnItem::UserMessage(item) = &payload.item {
            self.handle_materialized_item_lifecycle(&payload.turn_id, &payload.item);
            self.pending_legacy_user_mirror = Some(item.as_legacy_user_message_event());
            return;
        }
        if matches!(
            &payload.item,
            codex_protocol::items::TurnItem::AgentMessage(_)
                | codex_protocol::items::TurnItem::ContextCompaction(_)
                | codex_protocol::items::TurnItem::Extension(
                    codex_extension_items::ExtensionItem::WebSearch(_)
                )
        ) {
            let item = self.materialize_item(&payload.item);
            if !matches!(&item, ThreadItem::AgentMessage { text, .. } if text.is_empty()) {
                self.upsert_item_in_turn_id(&payload.turn_id, item.clone());
                if self.contains_turn(&payload.turn_id) {
                    self.pending_legacy_item_mirror = (!matches!(item,
                        ThreadItem::AgentMessage { interruption: Some(_), .. })).then_some(item);
                }
            }
        } else {
            self.handle_materialized_item_lifecycle(&payload.turn_id, &payload.item);
        }
    }

    fn handle_materialized_item_lifecycle(
        &mut self,
        turn_id: &str,
        item: &codex_protocol::items::TurnItem,
    ) {
        let is_review_mode_item = matches!(
            item,
            codex_protocol::items::TurnItem::EnteredReviewMode(_)
                | codex_protocol::items::TurnItem::ExitedReviewMode(_)
        );
        let should_upsert = match item {
            codex_protocol::items::TurnItem::Plan(plan) => !plan.text.is_empty(),
            codex_protocol::items::TurnItem::Sleep(_)
            | codex_protocol::items::TurnItem::HookPrompt(_)
            | codex_protocol::items::TurnItem::CommandExecution(_)
            | codex_protocol::items::TurnItem::UserMessage(_)
            | codex_protocol::items::TurnItem::ImageView(_)
            | codex_protocol::items::TurnItem::DynamicToolCall(_)
            | codex_protocol::items::TurnItem::CollabAgentToolCall(_)
            | codex_protocol::items::TurnItem::SubAgentActivity(_)
            | codex_protocol::items::TurnItem::Extension(_)
            | codex_protocol::items::TurnItem::ContextCompaction(_)
            | codex_protocol::items::TurnItem::EnteredReviewMode(_)
            | codex_protocol::items::TurnItem::ExitedReviewMode(_) => true,
            codex_protocol::items::TurnItem::AgentMessage(_)
            | codex_protocol::items::TurnItem::Reasoning(_)
            | codex_protocol::items::TurnItem::WebSearch(_)
            | codex_protocol::items::TurnItem::ImageGeneration(_)
            | codex_protocol::items::TurnItem::FileChange(_)
            | codex_protocol::items::TurnItem::McpToolCall(_) => false,
        };

        if should_upsert {
            if matches!(item, codex_protocol::items::TurnItem::UserMessage(_))
                && self.contains_turn(turn_id)
            {
                self.rollback_eligible_turn_ids.insert(turn_id.to_string());
            }
            let item = self.materialize_item(item);
            if is_review_mode_item {
                self.upsert_review_mode_item(Some(turn_id), item);
            } else {
                self.upsert_item_in_turn_id(turn_id, item);
            }
        }
    }

    fn handle_web_search_begin(&mut self, payload: &WebSearchBeginEvent) {
        if self.current_turn_has_item(&payload.call_id) {
            return;
        }
        let item = ThreadItem::WebSearch(WebSearchItem {
            status: None,
            error: None,
            id: payload.call_id.clone(),
            query: String::new(),
            action: None,
        });
        self.upsert_item_in_current_turn(item);
    }

    fn handle_web_search_end(&mut self, payload: &WebSearchEndEvent) {
        let item = ThreadItem::WebSearch(WebSearchItem {
            status: None,
            error: None,
            id: payload.call_id.clone(),
            query: payload.query.clone(),
            action: Some(web_search_action_from_core(payload.action.clone())),
        });
        self.upsert_item_in_current_turn(item);
    }

    fn handle_exec_command_begin(&mut self, payload: &ExecCommandBeginEvent) {
        if self.omit_tool_payloads {
            self.upsert_item_in_turn_id(&payload.turn_id, Self::omitted_tool_item(payload.call_id.clone()));
            return;
        }
        let item = build_command_execution_begin_item(payload);
        self.upsert_item_in_turn_id(&payload.turn_id, item);
    }

    fn handle_exec_command_end(&mut self, payload: &ExecCommandEndEvent) {
        if self.omit_tool_payloads {
            self.upsert_item_in_turn_id(&payload.turn_id, Self::omitted_tool_item(payload.call_id.clone()));
            return;
        }
        let item = build_command_execution_end_item(payload);
        // Command completions can arrive out of order. Unified exec may return
        // while a PTY is still running, then emit ExecCommandEnd later from a
        // background exit watcher when that process finally exits. By then, a
        // newer user turn may already have started. Route by event turn_id so
        // replay preserves the original turn association.
        self.upsert_item_in_turn_id(&payload.turn_id, item);
    }

    fn handle_apply_patch_approval_request(&mut self, payload: &ApplyPatchApprovalRequestEvent) {
        let item = if self.omit_tool_payloads {
            Self::omitted_tool_item(payload.call_id.clone())
        } else {
            build_file_change_approval_request_item(payload)
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    fn handle_patch_apply_begin(&mut self, payload: &PatchApplyBeginEvent) {
        let item = if self.omit_tool_payloads {
            Self::omitted_tool_item(payload.call_id.clone())
        } else {
            build_file_change_begin_item(payload)
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    fn handle_patch_apply_end(&mut self, payload: &PatchApplyEndEvent) {
        let item = if self.omit_tool_payloads {
            Self::omitted_tool_item(payload.call_id.clone())
        } else {
            build_file_change_end_item(payload)
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    fn handle_dynamic_tool_call_request(
        &mut self,
        payload: &codex_protocol::dynamic_tools::DynamicToolCallRequest,
    ) {
        if self.omit_tool_payloads {
            let item = Self::omitted_tool_item(payload.call_id.clone());
            if payload.turn_id.is_empty() {
                self.upsert_item_in_current_turn(item);
            } else {
                self.upsert_item_in_turn_id(&payload.turn_id, item);
            }
            return;
        }
        let item = ThreadItem::DynamicToolCall {
            id: payload.call_id.clone(),
            namespace: payload.namespace.clone(),
            tool: payload.tool.clone(),
            arguments: payload.arguments.clone(),
            status: DynamicToolCallStatus::InProgress,
            content_items: None,
            success: None,
            error: None,
            duration_ms: None,
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    fn handle_dynamic_tool_call_response(&mut self, payload: &DynamicToolCallResponseEvent) {
        if self.omit_tool_payloads {
            let item = Self::omitted_tool_item(payload.call_id.clone());
            if payload.turn_id.is_empty() {
                self.upsert_item_in_current_turn(item);
            } else {
                self.upsert_item_in_turn_id(&payload.turn_id, item);
            }
            return;
        }
        let status = if payload.success {
            DynamicToolCallStatus::Completed
        } else {
            DynamicToolCallStatus::Failed
        };
        let duration_ms = i64::try_from(payload.duration.as_millis()).ok();
        let item = ThreadItem::DynamicToolCall {
            id: payload.call_id.clone(),
            namespace: payload.namespace.clone(),
            tool: payload.tool.clone(),
            arguments: payload.arguments.clone(),
            status,
            content_items: Some(convert_dynamic_tool_content_items(&payload.content_items)),
            success: Some(payload.success),
            error: payload.error.clone(),
            duration_ms,
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    fn handle_mcp_tool_call_begin(&mut self, payload: &McpToolCallBeginEvent) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_tool_item(payload.call_id.clone()));
            return;
        }
        let item = ThreadItem::McpToolCall {
            id: payload.call_id.clone(),
            server: payload.invocation.server.clone(),
            tool: payload.invocation.tool.clone(),
            status: McpToolCallStatus::InProgress,
            arguments: payload
                .invocation
                .arguments
                .clone()
                .unwrap_or(serde_json::Value::Null),
            app_context: payload
                .connector_id
                .clone()
                .map(|connector_id| McpToolCallAppContext {
                    connector_id,
                    link_id: payload.link_id.clone(),
                    resource_uri: payload.mcp_app_resource_uri.clone(),
                    app_name: payload.app_name.clone(),
                    template_id: payload.template_id.clone(),
                    action_name: payload.action_name.clone(),
                }),
            mcp_app_resource_uri: payload.mcp_app_resource_uri.clone(),
            plugin_id: payload.plugin_id.clone(),
            result: None,
            error: None,
            duration_ms: None,
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_mcp_tool_call_end(&mut self, payload: &McpToolCallEndEvent) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_tool_item(payload.call_id.clone()));
            return;
        }
        let status = if payload.is_success() {
            McpToolCallStatus::Completed
        } else {
            McpToolCallStatus::Failed
        };
        let duration_ms = i64::try_from(payload.duration.as_millis()).ok();
        let (result, error) = match &payload.result {
            Ok(value) => (
                Some(Box::new(McpToolCallResult {
                    content: value.content.clone(),
                    structured_content: value.structured_content.clone(),
                    meta: value.meta.clone(),
                })),
                None,
            ),
            Err(message) => (
                None,
                Some(McpToolCallError {
                    message: message.clone(),
                }),
            ),
        };
        let item = ThreadItem::McpToolCall {
            id: payload.call_id.clone(),
            server: payload.invocation.server.clone(),
            tool: payload.invocation.tool.clone(),
            status,
            arguments: payload
                .invocation
                .arguments
                .clone()
                .unwrap_or(serde_json::Value::Null),
            app_context: payload
                .connector_id
                .clone()
                .map(|connector_id| McpToolCallAppContext {
                    connector_id,
                    link_id: payload.link_id.clone(),
                    resource_uri: payload.mcp_app_resource_uri.clone(),
                    app_name: payload.app_name.clone(),
                    template_id: payload.template_id.clone(),
                    action_name: payload.action_name.clone(),
                }),
            mcp_app_resource_uri: payload.mcp_app_resource_uri.clone(),
            plugin_id: payload.plugin_id.clone(),
            result,
            error,
            duration_ms,
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_view_image_tool_call(&mut self, payload: &ViewImageToolCallEvent) {
        let item = ThreadItem::ImageView {
            environment: payload.environment.clone(),
            id: payload.call_id.clone(),
            path: payload.path.clone().into(),
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_image_generation_begin(&mut self, payload: &ImageGenerationBeginEvent) {
        if self.current_turn_has_item(&payload.call_id) {
            return;
        }
        let item = ThreadItem::ImageGeneration(ImageGenerationItem {
            id: payload.call_id.clone(),
            status: String::new(),
            revised_prompt: None,
            result: None,
            error: None,
            saved_path: None,
            provider_usage: None,
        });
        self.upsert_item_in_current_turn(item);
    }

    fn handle_image_generation_end(&mut self, payload: &ImageGenerationEndEvent) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_tool_item(payload.call_id.clone()));
            return;
        }
        // Legacy ends cannot express usage. Keep the same turn/call's receipt,
        // without copying or comparing image bytes or suppressing later updates.
        let provider_usage = self.current_turn.as_ref().and_then(|turn| {
            let index = *turn.item_indexes.get(&payload.call_id)?;
            match &turn.items[index] {
                ThreadItem::ImageGeneration(item) => item.provider_usage.clone(),
                _ => None,
            }
        });
        let item = ThreadItem::ImageGeneration(ImageGenerationItem {
            id: payload.call_id.clone(),
            status: payload.status.clone(),
            revised_prompt: payload.revised_prompt.clone(),
            result: payload.result.clone(),
            error: payload.error.clone(),
            saved_path: payload.saved_path.clone(),
            provider_usage,
        });
        self.upsert_item_in_current_turn(item);
    }

    fn handle_collab_agent_spawn_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabAgentSpawnBeginEvent,
    ) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(&payload.call_id, CollabAgentTool::SpawnAgent, CollabAgentToolCallStatus::InProgress));
            return;
        }
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::SpawnAgent,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: Vec::new(),
            prompt: Some(payload.prompt.clone()),
            model: payload.model.clone(),
            reasoning_effort: payload.reasoning_effort.clone(),
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_collab_agent_spawn_end(
        &mut self,
        payload: &codex_protocol::protocol::CollabAgentSpawnEndEvent,
    ) {
        let has_receiver = payload.new_thread_id.is_some();
        let status = match &payload.status {
            AgentStatus::Errored(_) | AgentStatus::NotFound => CollabAgentToolCallStatus::Failed,
            _ if has_receiver => CollabAgentToolCallStatus::Completed,
            _ => CollabAgentToolCallStatus::Failed,
        };
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(&payload.call_id, CollabAgentTool::SpawnAgent, status));
            return;
        }
        let (receiver_thread_ids, agents_states) = match &payload.new_thread_id {
            Some(id) => {
                let receiver_id = id.to_string();
                let received_status = CollabAgentState::from(payload.status.clone());
                (
                    vec![receiver_id.clone()],
                    [(receiver_id, received_status)].into_iter().collect(),
                )
            }
            None => (Vec::new(), HashMap::new()),
        };
        self.upsert_item_in_current_turn(ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::SpawnAgent,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids,
            prompt: Some(payload.prompt.clone()),
            model: payload.model.clone(),
            reasoning_effort: payload.reasoning_effort.clone(),
            agents_states,
        });
    }

    fn handle_collab_agent_interaction_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabAgentInteractionBeginEvent,
    ) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(&payload.call_id, CollabAgentTool::SendInput, CollabAgentToolCallStatus::InProgress));
            return;
        }
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::SendInput,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![payload.receiver_thread_id.to_string()],
            prompt: Some(payload.prompt.clone()),
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_collab_agent_interaction_end(
        &mut self,
        payload: &codex_protocol::protocol::CollabAgentInteractionEndEvent,
    ) {
        let status = match &payload.status {
            AgentStatus::Errored(_) | AgentStatus::NotFound => CollabAgentToolCallStatus::Failed,
            _ => CollabAgentToolCallStatus::Completed,
        };
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(&payload.call_id, CollabAgentTool::SendInput, status));
            return;
        }
        let receiver_id = payload.receiver_thread_id.to_string();
        let received_status = CollabAgentState::from(payload.status.clone());
        self.upsert_item_in_current_turn(ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::SendInput,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![receiver_id.clone()],
            prompt: Some(payload.prompt.clone()),
            model: None,
            reasoning_effort: None,
            agents_states: [(receiver_id, received_status)].into_iter().collect(),
        });
    }

    fn handle_sub_agent_activity(
        &mut self,
        payload: &codex_protocol::protocol::SubAgentActivityEvent,
    ) {
        self.upsert_item_in_current_turn(ThreadItem::SubAgentActivity {
            id: payload.event_id.clone(),
            kind: payload.kind.into(),
            agent_thread_id: payload.agent_thread_id.to_string(),
            agent_path: String::from(payload.agent_path.clone()),
        });
    }

    fn handle_collab_waiting_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabWaitingBeginEvent,
    ) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(&payload.call_id, CollabAgentTool::Wait, CollabAgentToolCallStatus::InProgress));
            return;
        }
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::Wait,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: payload
                .receiver_thread_ids
                .iter()
                .map(ToString::to_string)
                .collect(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_collab_waiting_end(
        &mut self,
        payload: &codex_protocol::protocol::CollabWaitingEndEvent,
    ) {
        // Modern item events carry an explicit terminal outcome that this legacy
        // event cannot represent (for example, a failed wait with no receivers).
        // A terminal snapshot also covers waits reconciled by TurnAborted.
        let wait_status = |item: &ThreadItem| match item {
            ThreadItem::CollabAgentToolCall {
                id,
                tool: CollabAgentTool::Wait,
                status,
                ..
            } if id == &payload.call_id => Some(status.clone()),
            _ => None,
        };
        let current_owner = self.current_turn.as_ref().and_then(|turn| {
            turn.item_indexes.get(&payload.call_id).and_then(|&index| {
                wait_status(&turn.items[index]).map(|status| (turn.id.clone(), status))
            })
        });
        let owner = if current_owner.is_some() {
            current_owner
        } else {
            let mut owners = self.turns.iter().filter_map(|turn| {
                turn.items
                    .iter()
                    .find_map(&wait_status)
                    .map(|status| (turn.id.clone(), status))
            });
            let first = owners.next();
            // An unscoped legacy event cannot disambiguate reused historical IDs.
            if owners.next().is_some() {
                return;
            }
            first
        };
        if owner.is_none() && self.rolled_back_wait_ids.contains(&payload.call_id) {
            return;
        }
        if owner
            .as_ref()
            .is_some_and(|(_, status)| *status != CollabAgentToolCallStatus::InProgress)
        {
            return;
        }
        let status = if payload
            .statuses
            .values()
            .any(|status| matches!(status, AgentStatus::Errored(_) | AgentStatus::NotFound))
        {
            CollabAgentToolCallStatus::Failed
        } else {
            CollabAgentToolCallStatus::Completed
        };
        if self.omit_tool_payloads {
            let item = Self::omitted_collab_item(&payload.call_id, CollabAgentTool::Wait, status);
            if let Some((turn_id, _)) = owner {
                self.upsert_item_in_turn_id(&turn_id, item);
            } else {
                self.upsert_item_in_current_turn(item);
            }
            return;
        }
        let mut receiver_thread_ids: Vec<String> =
            payload.statuses.keys().map(ToString::to_string).collect();
        receiver_thread_ids.sort();
        let agents_states = payload
            .statuses
            .iter()
            .map(|(id, status)| (id.to_string(), CollabAgentState::from(status.clone())))
            .collect();
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::Wait,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids,
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states,
        };
        if let Some((turn_id, _)) = owner {
            self.upsert_item_in_turn_id(&turn_id, item);
        } else {
            self.upsert_item_in_current_turn(item);
        }
    }

    fn handle_collab_close_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabCloseBeginEvent,
    ) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(&payload.call_id, CollabAgentTool::CloseAgent, CollabAgentToolCallStatus::InProgress));
            return;
        }
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::CloseAgent,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![payload.receiver_thread_id.to_string()],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_collab_close_end(&mut self, payload: &codex_protocol::protocol::CollabCloseEndEvent) {
        let status = match &payload.status {
            AgentStatus::Errored(_) | AgentStatus::NotFound => CollabAgentToolCallStatus::Failed,
            _ => CollabAgentToolCallStatus::Completed,
        };
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(&payload.call_id, CollabAgentTool::CloseAgent, status));
            return;
        }
        let receiver_id = payload.receiver_thread_id.to_string();
        let agents_states = [(
            receiver_id.clone(),
            CollabAgentState::from(payload.status.clone()),
        )]
        .into_iter()
        .collect();
        self.upsert_item_in_current_turn(ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::CloseAgent,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![receiver_id],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states,
        });
    }

    fn handle_collab_resume_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabResumeBeginEvent,
    ) {
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(&payload.call_id, CollabAgentTool::ResumeAgent, CollabAgentToolCallStatus::InProgress));
            return;
        }
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::ResumeAgent,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![payload.receiver_thread_id.to_string()],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_collab_resume_end(
        &mut self,
        payload: &codex_protocol::protocol::CollabResumeEndEvent,
    ) {
        let status = match &payload.status {
            AgentStatus::Errored(_) | AgentStatus::NotFound => CollabAgentToolCallStatus::Failed,
            _ => CollabAgentToolCallStatus::Completed,
        };
        if self.omit_tool_payloads {
            self.upsert_item_in_current_turn(Self::omitted_collab_item(&payload.call_id, CollabAgentTool::ResumeAgent, status));
            return;
        }
        let receiver_id = payload.receiver_thread_id.to_string();
        let agents_states = [(
            receiver_id.clone(),
            CollabAgentState::from(payload.status.clone()),
        )]
        .into_iter()
        .collect();
        self.upsert_item_in_current_turn(ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::ResumeAgent,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![receiver_id],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states,
        });
    }

    fn handle_context_compacted(&mut self, _payload: &ContextCompactedEvent) {
        let id = self.next_item_id();
        // The deprecated ContextCompacted event carries no detail; v2 turn items
        // populate these fields at the compaction site instead.
        self.push_item_in_current_turn(ThreadItem::ContextCompaction {
            id,
            tokens_before: None,
            tokens_after: None,
            omitted_image_count: None,
            retained_image_count: None,
            omitted_text: None,
        });
    }

    fn handle_entered_review_mode(
        &mut self,
        payload: &codex_protocol::protocol::EnteredReviewModeEvent,
    ) {
        if self.omit_tool_payloads {
            let id = payload.item_id.clone().unwrap_or_else(|| self.next_item_id());
            self.upsert_review_mode_item(payload.turn_id.as_deref(), Self::omitted_tool_item(id));
            return;
        }
        let review = payload
            .user_facing_hint
            .clone()
            .unwrap_or_else(|| "Review requested.".to_string());
        let id = payload
            .item_id
            .clone()
            .unwrap_or_else(|| self.next_item_id());
        self.upsert_review_mode_item(
            payload.turn_id.as_deref(),
            ThreadItem::EnteredReviewMode { id, review },
        );
    }

    fn handle_exited_review_mode(
        &mut self,
        payload: &codex_protocol::protocol::ExitedReviewModeEvent,
    ) {
        if self.omit_tool_payloads {
            let id = payload.item_id.clone().unwrap_or_else(|| self.next_item_id());
            self.upsert_review_mode_item(payload.turn_id.as_deref(), Self::omitted_tool_item(id));
            return;
        }
        let review = review_output_text(payload.review_output.as_ref());
        let id = payload
            .item_id
            .clone()
            .unwrap_or_else(|| self.next_item_id());
        self.upsert_review_mode_item(
            payload.turn_id.as_deref(),
            ThreadItem::ExitedReviewMode { id, review },
        );
    }

    fn upsert_review_mode_item(&mut self, turn_id: Option<&str>, item: ThreadItem) {
        let Some(turn_id) = turn_id else {
            self.upsert_item_in_current_turn(item);
            return;
        };
        let current_turn_matches = self
            .current_turn
            .as_ref()
            .is_some_and(|turn| turn.id == turn_id);
        if !current_turn_matches && !self.turns.iter().any(|turn| turn.id == turn_id) {
            self.finish_current_turn();
            let turn = self.new_turn(Some(turn_id.to_string()));
            self.record_changed_turn(self.turns.len());
            self.current_turn = Some(turn);
        }
        self.upsert_item_in_turn_id(turn_id, item);
    }

    fn handle_error(&mut self, payload: &ErrorEvent) {
        if !payload.affects_turn_status() {
            return;
        }
        if let Some(turn) = self.current_turn.as_mut() {
            // An explicit abort already closed this turn; it stays current only
            // to collect late items. As after `turn_complete`, a later unscoped
            // error belongs to another request (for example, a rejected
            // follow-up turn start) and must not relabel the aborted outcome.
            if turn.opened_explicitly && turn.closed_by_abort {
                return;
            }
            turn.status = TurnStatus::Failed;
            turn.error = Some(V2TurnError {
                message: payload.message.clone(),
                codex_error_info: payload.codex_error_info.clone().map(Into::into),
                additional_details: None,
            });
            self.record_changed_turn(self.turns.len());
        }
    }

    fn handle_turn_aborted(&mut self, payload: &TurnAbortedEvent) {
        let abort_status = if matches!(
            payload.reason,
            codex_protocol::protocol::TurnAbortReason::InternalError
                | codex_protocol::protocol::TurnAbortReason::ProcessLost
        ) {
            TurnStatus::Failed
        } else {
            TurnStatus::Interrupted
        };
        let apply_abort = |turn: &mut PendingTurn| {
            turn.status = abort_status.clone();
            turn.completed_at = payload.completed_at;
            turn.duration_ms = payload.duration_ms;
            turn.timing = payload.timing.clone();
            turn.surfaced_result = None;
            turn.closed_by_abort = true;
        };
        if let Some(turn_id) = payload.turn_id.as_deref() {
            // Prefer an exact ID match so we interrupt the turn explicitly targeted by the event.
            if let Some(turn) = self.current_turn.as_mut().filter(|turn| turn.id == turn_id) {
                apply_abort(turn);
                let aborted_turn_id = turn.id.clone();
                self.record_changed_turn(self.turns.len());
                self.fail_running_wait_items(&aborted_turn_id);
                return;
            }

            if let Some(turn_index) = self.turns.iter().position(|turn| turn.id == turn_id) {
                let turn = &mut self.turns[turn_index];
                turn.status = abort_status.clone();
                turn.completed_at = payload.completed_at;
                turn.duration_ms = payload.duration_ms;
                turn.timing = payload.timing.clone();
                turn.surfaced_result = None;
                let aborted_turn_id = turn.id.clone();
                self.record_changed_turn(turn_index);
                self.fail_running_wait_items(&aborted_turn_id);
                return;
            }
        }

        // Legacy events may lack an ID or explicit turn boundaries. An unknown
        // target must never interrupt a different explicitly opened turn.
        if let Some(turn) = self
            .current_turn
            .as_mut()
            .filter(|turn| payload.turn_id.is_none() || !turn.opened_explicitly)
        {
            apply_abort(turn);
            let aborted_turn_id = turn.id.clone();
            self.record_changed_turn(self.turns.len());
            self.fail_running_wait_items(&aborted_turn_id);
        }
    }

    fn fail_running_wait_items(&mut self, turn_id: &str) {
        let tracking_changes = self.is_tracking_changes();
        let (turn_index, items) =
            if let Some(turn) = self.current_turn.as_mut().filter(|turn| turn.id == turn_id) {
                (self.turns.len(), &mut turn.items)
            } else if let Some(index) = self.turns.iter().position(|turn| turn.id == turn_id) {
                (index, &mut self.turns[index].items)
            } else {
                return;
            };
        let mut changed_items = Vec::new();
        for (index, item) in items.iter_mut().enumerate() {
            if let ThreadItem::CollabAgentToolCall {
                tool: CollabAgentTool::Wait,
                status,
                ..
            } = item
                && *status == CollabAgentToolCallStatus::InProgress
            {
                *status = CollabAgentToolCallStatus::Failed;
                if tracking_changes {
                    changed_items.push(index);
                }
            }
        }
        for index in changed_items {
            self.record_changed_item(turn_index, index);
        }
    }

    fn handle_turn_started(&mut self, payload: &TurnStartedEvent) {
        self.finish_current_turn();
        let turn = self
            .new_turn(Some(payload.turn_id.clone()))
            .with_status(TurnStatus::InProgress)
            .with_started_at(payload.started_at)
            .opened_explicitly();
        self.record_changed_turn(self.turns.len());
        self.current_turn = Some(turn);
    }

    fn handle_turn_complete(&mut self, payload: &TurnCompleteEvent) {
        let terminal_error = payload.error.as_ref().map(|error| V2TurnError {
            message: error.message.clone(),
            codex_error_info: error.codex_error_info.clone().map(Into::into),
            additional_details: None,
        });
        let apply_completion = |turn: &mut PendingTurn| {
            if let Some(error) = terminal_error.as_ref() {
                turn.status = TurnStatus::Failed;
                turn.error = Some(error.clone());
            } else if matches!(turn.status, TurnStatus::Completed | TurnStatus::InProgress) {
                turn.status = TurnStatus::Completed;
            }
            turn.completed_at = payload.completed_at;
            turn.duration_ms = payload.duration_ms;
            turn.timing = payload.timing.clone();
            turn.surfaced_result = payload.surfaced_result.clone();
        };

        // Prefer an exact ID match from the active turn and then close it.
        if let Some(current_turn) = self
            .current_turn
            .as_mut()
            .filter(|turn| turn.id == payload.turn_id)
        {
            apply_completion(current_turn);
            self.record_changed_turn(self.turns.len());
            self.finish_current_turn();
            return;
        }

        if let Some(turn_index) = self
            .turns
            .iter()
            .position(|turn| turn.id == payload.turn_id)
        {
            let turn = &mut self.turns[turn_index];
            if let Some(error) = terminal_error.as_ref() {
                turn.status = TurnStatus::Failed;
                turn.error = Some(error.clone());
            } else if matches!(turn.status, TurnStatus::Completed | TurnStatus::InProgress) {
                turn.status = TurnStatus::Completed;
            }
            turn.completed_at = payload.completed_at;
            turn.duration_ms = payload.duration_ms;
            turn.timing = payload.timing.clone();
            turn.surfaced_result = payload.surfaced_result.clone();
            self.record_changed_turn(turn_index);
            return;
        }

        // Legacy histories can lack an explicit turn boundary, so retain the
        // historical fallback only for an implicitly opened turn. Never attach
        // an unmatched historical completion to a different explicit active turn
        // (for example, the first new turn in a fork).
        if let Some(current_turn) = self
            .current_turn
            .as_mut()
            .filter(|turn| !turn.opened_explicitly)
        {
            apply_completion(current_turn);
            self.record_changed_turn(self.turns.len());
            self.finish_current_turn();
        }
    }

    /// Marks the current turn as containing a persisted compaction marker.
    ///
    /// This keeps compaction-only legacy turns from being dropped by
    /// `finish_current_turn` when they have no renderable items and were not
    /// explicitly opened.
    fn handle_compacted(&mut self, _payload: &CompactedItem) {
        self.ensure_turn().saw_compaction = true;
    }

    fn handle_thread_rollback(&mut self, payload: &ThreadRolledBackEvent) {
        self.finish_current_turn();

        let n = usize::try_from(payload.num_turns).unwrap_or(usize::MAX);
        let mut remaining = n;
        let mut retained_len = self.turns.len();
        while retained_len > 0 && remaining > 0 {
            retained_len -= 1;
            if self.is_rollback_eligible_turn(&self.turns[retained_len].id) {
                remaining -= 1;
            }
        }
        self.rolled_back_wait_ids.extend(
            self.turns[retained_len..]
                .iter()
                .flat_map(|turn| &turn.items)
                .filter_map(|item| match item {
                    ThreadItem::CollabAgentToolCall { id, .. } => Some(id.clone()),
                    _ => None,
                }),
        );
        if let Some(changes) = self.active_change_set.as_mut() {
            changes.removed_turn_ids.extend(
                self.turns[retained_len..]
                    .iter()
                    .map(|turn| turn.id.clone()),
            );
            changes
                .changed_items
                .retain(|(turn_index, _)| *turn_index < retained_len);
            changes
                .changed_turns
                .retain(|turn_index| *turn_index < retained_len);
        }

        for turn in &self.turns[retained_len..] {
            self.rollback_eligible_turn_ids.remove(&turn.id);
        }
        self.turns.truncate(retained_len);

        let item_count: usize = self.turns.iter().map(|t| t.items.len()).sum();
        self.next_item_index = i64::try_from(item_count.saturating_add(1)).unwrap_or(i64::MAX);
    }

    fn finish_current_turn(&mut self) {
        if let Some(turn) = self.current_turn.take() {
            if turn.items.is_empty()
                && !turn.opened_explicitly
                && !turn.saw_compaction
                && !self.is_rollback_eligible_turn(&turn.id)
            {
                if let Some(changes) = self.active_change_set.as_mut() {
                    changes.changed_turns.shift_remove(&self.turns.len());
                }
                return;
            }
            self.turns.push(Turn::from(turn));
        }
    }

    fn new_turn(&mut self, id: Option<String>) -> PendingTurn {
        let id = id.unwrap_or_else(|| {
            if self.next_rollout_index == 0 {
                Uuid::now_v7().to_string()
            } else {
                format!("rollout-{}", self.current_rollout_index)
            }
        });
        PendingTurn {
            id,
            next_synthetic_item_index: 1,
            items: Vec::new(),
            item_indexes: HashMap::new(),
            error: None,
            status: TurnStatus::Completed,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            timing: None,
            surfaced_result: None,
            opened_explicitly: false,
            closed_by_abort: false,
            saw_compaction: false,
            raw_user_boundary: false,
            rollout_start_index: self.current_rollout_index,
        }
    }

    fn ensure_turn(&mut self) -> &mut PendingTurn {
        if self.current_turn.is_none() {
            let turn = self.new_turn(/*id*/ None);
            self.record_changed_turn(self.turns.len());
            self.current_turn = Some(turn);
        }

        if let Some(turn) = self.current_turn.as_mut() {
            return turn;
        }

        unreachable!("current turn must exist after initialization");
    }

    fn push_item_in_current_turn(&mut self, item: ThreadItem) {
        let turn = self.ensure_turn();
        let index = turn.items.len();
        turn.item_indexes
            .entry(item.id().to_string())
            .or_insert(index);
        turn.items.push(item);
        self.record_changed_item(self.turns.len(), index);
    }

    fn upsert_item_in_turn_id(&mut self, turn_id: &str, item: ThreadItem) {
        if let Some(turn) = self.current_turn.as_mut().filter(|turn| turn.id == turn_id) {
            let index = turn.upsert_item(item);
            self.record_changed_item(self.turns.len(), index);
        } else if let Some(turn_index) = self.turns.iter().position(|turn| turn.id == turn_id) {
            let index = upsert_turn_item(&mut self.turns[turn_index].items, item);
            self.record_changed_item(turn_index, index);
        } else {
            warn!(
                item_id = item.id(),
                "dropping turn-scoped item for unknown turn id `{turn_id}`"
            );
        }
    }

    fn upsert_item_in_current_turn(&mut self, item: ThreadItem) {
        if let ThreadItem::CollabAgentToolCall {
            id, tool, status, ..
        } = &item
            && *status != CollabAgentToolCallStatus::InProgress
        {
            // Receiver lifecycle cannot replace an authoritative call outcome.
            // Resolve unscoped legacy ends just as Wait does, including late
            // ends, ambiguous reused IDs and identities removed by rollback.
            let status_of = |candidate: &ThreadItem| match candidate {
                ThreadItem::CollabAgentToolCall {
                    id: candidate_id,
                    tool: candidate_tool,
                    status,
                    ..
                } if candidate_id == id && candidate_tool == tool => Some(status.clone()),
                _ => None,
            };
            let current = self.current_turn.as_ref().and_then(|turn| {
                turn.item_indexes
                    .get(id)
                    .and_then(|&index| status_of(&turn.items[index]))
                    .map(|status| (turn.id.clone(), status))
            });
            let owner = if current.is_some() {
                current
            } else {
                let mut owners = self.turns.iter().filter_map(|turn| {
                    turn.items
                        .iter()
                        .find_map(&status_of)
                        .map(|status| (turn.id.clone(), status))
                });
                let first = owners.next();
                if owners.next().is_some() {
                    return;
                }
                first
            };
            if owner
                .as_ref()
                .is_some_and(|(_, status)| *status != CollabAgentToolCallStatus::InProgress)
                || (owner.is_none() && self.rolled_back_wait_ids.contains(id))
            {
                return;
            }
            if let Some((turn_id, _)) = owner {
                self.upsert_item_in_turn_id(&turn_id, item);
                return;
            }
        }
        let index = self.ensure_turn().upsert_item(item);
        self.record_changed_item(self.turns.len(), index);
    }

    /// Legacy begin events carry only a call id. Extension tools emit their canonical
    /// started item first, so replacing it would erase that item's state.
    fn current_turn_has_item(&self, item_id: &str) -> bool {
        self.current_turn
            .as_ref()
            .is_some_and(|turn| turn.item_indexes.contains_key(item_id))
    }

    fn is_tracking_changes(&self) -> bool {
        self.active_change_set.is_some()
    }

    fn record_changed_item(&mut self, turn_index: usize, item_index: usize) {
        if let Some(changes) = self.active_change_set.as_mut() {
            changes.changed_items.insert((turn_index, item_index));
        }
    }

    fn record_changed_turn(&mut self, turn_index: usize) {
        if let Some(changes) = self.active_change_set.as_mut() {
            changes.changed_turns.insert(turn_index);
        }
    }

    fn next_item_id(&mut self) -> String {
        if let Some(turn) = self.current_turn.as_mut()
            && turn.opened_explicitly
        {
            // A live suffix and full replay have different global positions.
            // Explicit turn identity and its own ordinal do not depend on them.
            let id = format!("item-{}-{}", turn.id, turn.next_synthetic_item_index);
            turn.next_synthetic_item_index += 1;
            return id;
        }
        let id = format!("item-{}", self.next_item_index);
        self.next_item_index += 1;
        id
    }

    fn build_user_inputs(&self, payload: &UserMessageEvent) -> Vec<UserInput> {
        let mut content = Vec::new();
        if !payload.message.trim().is_empty() {
            content.push(UserInput::Text {
                text: payload.message.clone(),
                text_elements: payload
                    .text_elements
                    .iter()
                    .cloned()
                    .map(Into::into)
                    .collect(),
            });
        }
        if let Some(images) = &payload.images {
            for (idx, image) in images.iter().enumerate() {
                content.push(UserInput::Image {
                    url: image.clone(),
                    detail: payload.image_details.get(idx).copied().flatten(),
                });
            }
        }
        for (idx, path) in payload.local_images.iter().enumerate() {
            content.push(UserInput::LocalImage {
                path: path.clone(),
                detail: payload.local_image_details.get(idx).copied().flatten(),
            });
        }
        content
    }

    fn omitted_tool_item(id: String) -> ThreadItem {
        ThreadItem::Plan { id, text: String::new() }
    }

    fn omitted_collab_item(id: &str, tool: CollabAgentTool, status: CollabAgentToolCallStatus) -> ThreadItem {
        ThreadItem::CollabAgentToolCall {
            id: id.to_string(), tool, status, sender_thread_id: String::new(),
            receiver_thread_ids: Vec::new(), prompt: None, model: None,
            reasoning_effort: None, agents_states: HashMap::new(),
        }
    }

    fn materialize_item(&self, item: &codex_protocol::items::TurnItem) -> ThreadItem {
        use codex_protocol::items::TurnItem as CoreItem;
        if !self.omit_tool_payloads {
            return ThreadItem::from(item.clone());
        }
        match item {
            // Keep exact canonical/legacy mirror evidence and user/assistant summaries.
            CoreItem::UserMessage(_) | CoreItem::AgentMessage(_) | CoreItem::WebSearch(_)
            | CoreItem::Extension(codex_extension_items::ExtensionItem::WebSearch(_)) => {
                ThreadItem::from(item.clone())
            }
            CoreItem::ContextCompaction(_) => ThreadItem::ContextCompaction {
                id: item.id(), tokens_before: None, tokens_after: None,
                omitted_image_count: None, retained_image_count: None, omitted_text: None,
            },
            CoreItem::Reasoning(_) => ThreadItem::Reasoning {
                id: item.id(), summary: Vec::new(), content: Vec::new(),
            },
            CoreItem::CollabAgentToolCall(call) => ThreadItem::CollabAgentToolCall {
                id: call.id.clone(), tool: call.tool.into(), status: call.status.into(),
                sender_thread_id: String::new(), receiver_thread_ids: Vec::new(),
                prompt: None, model: None, reasoning_effort: None, agents_states: HashMap::new(),
            },
            _ => Self::omitted_tool_item(item.id()),
        }
    }
}

fn convert_dynamic_tool_content_items(
    items: &[codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem],
) -> Vec<DynamicToolCallOutputContentItem> {
    items
        .iter()
        .cloned()
        .map(|item| match item {
            codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem::InputText { text } => {
                DynamicToolCallOutputContentItem::InputText { text }
            }
            codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem::InputImage {
                image_url,
            } => DynamicToolCallOutputContentItem::InputImage { image_url },
        })
        .collect()
}

fn replace_turn_item(existing: &mut ThreadItem, mut item: ThreadItem) {
    // Legacy command events cannot express these relationships. Their updates
    // must not erase identities already supplied by a first-class item event.
    if let (
        ThreadItem::CommandExecution {
            output_metadata,
            parent_call_id,
            parent_cell_id,
            runtime_tool_call_id,
            execution_id,
            ..
        },
        ThreadItem::CommandExecution {
            output_metadata: previous_metadata,
            parent_call_id: previous_parent,
            parent_cell_id: previous_cell,
            runtime_tool_call_id: previous_runtime,
            execution_id: previous_execution,
            ..
        },
    ) = (&mut item, &mut *existing)
    {
        if output_metadata.is_none() {
            *output_metadata = previous_metadata.take();
        }
        if parent_call_id.is_none() {
            *parent_call_id = previous_parent.take();
        }
        if parent_cell_id.is_none() {
            *parent_cell_id = previous_cell.take();
        }
        if runtime_tool_call_id.is_none() {
            *runtime_tool_call_id = previous_runtime.take();
        }
        if execution_id.is_none() {
            *execution_id = previous_execution.take();
        }
    }
    *existing = item;
}

fn upsert_turn_item(items: &mut Vec<ThreadItem>, item: ThreadItem) -> usize {
    if let Some(existing_item_index) = items
        .iter()
        .position(|existing_item| existing_item.id() == item.id())
    {
        replace_turn_item(&mut items[existing_item_index], item);
        return existing_item_index;
    }
    let inserted_item_index = items.len();
    items.push(item);
    inserted_item_index
}

struct PendingTurn {
    id: String,
    next_synthetic_item_index: i64,
    items: Vec<ThreadItem>,
    item_indexes: HashMap<String, usize>,
    error: Option<TurnError>,
    status: TurnStatus,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    timing: Option<TurnTiming>,
    surfaced_result: Option<SurfacedToolResult>,
    /// True when this turn originated from an explicit `turn_started`/`turn_complete`
    /// boundary, so we preserve it even if it has no renderable items.
    opened_explicitly: bool,
    /// True once a `turn_aborted` event closed this turn. It may stay current
    /// to collect late items, but later unscoped errors belong to other requests.
    closed_by_abort: bool,
    /// True when this turn includes a persisted `RolloutItem::Compacted`, which
    /// should keep the turn from being dropped even without normal items.
    saw_compaction: bool,
    raw_user_boundary: bool,
    /// Index of the rollout item that opened this turn during replay.
    rollout_start_index: usize,
}

impl PendingTurn {
    fn upsert_item(&mut self, item: ThreadItem) -> usize {
        if let Some(&index) = self.item_indexes.get(item.id()) {
            replace_turn_item(&mut self.items[index], item);
            index
        } else {
            let index = self.items.len();
            self.item_indexes.insert(item.id().to_string(), index);
            self.items.push(item);
            index
        }
    }

    fn opened_explicitly(mut self) -> Self {
        self.opened_explicitly = true;
        self
    }

    fn with_status(mut self, status: TurnStatus) -> Self {
        self.status = status;
        self
    }

    fn with_started_at(mut self, started_at: Option<i64>) -> Self {
        self.started_at = started_at;
        self
    }
}

impl From<PendingTurn> for Turn {
    fn from(value: PendingTurn) -> Self {
        Self {
            id: value.id,
            items: value.items,
            items_view: TurnItemsView::Full,
            error: value.error,
            status: value.status,
            started_at: value.started_at,
            completed_at: value.completed_at,
            duration_ms: value.duration_ms,
            timing: value.timing,
            surfaced_result: value.surfaced_result,
        }
    }
}

impl From<&PendingTurn> for Turn {
    fn from(value: &PendingTurn) -> Self {
        Self {
            id: value.id.clone(),
            items: value.items.clone(),
            items_view: TurnItemsView::Full,
            error: value.error.clone(),
            status: value.status.clone(),
            started_at: value.started_at,
            completed_at: value.completed_at,
            duration_ms: value.duration_ms,
            timing: value.timing.clone(),
            surfaced_result: value.surfaced_result.clone(),
        }
    }
}

