//! Helpers for truncating rollouts by persisted turns or user-message boundaries.
//! Modern turns own all their inputs, including steering. Legacy rollouts without
//! an explicit start still use instruction records as boundaries.

use crate::context_manager::is_user_turn_boundary;
use crate::event_mapping;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::build_turns_from_rollout_items;
use codex_context_fragments::LegacyInterAgentMailReader;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::DiagnosticOrigins;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;

#[derive(Default)]
pub(crate) struct PriorUserTurnTracker {
    legacy_inter_agent_mail: LegacyInterAgentMailReader,
    explicit_turn: ExplicitTurn,
    rollback_turn_positions: Vec<usize>,
    user_turn_positions: Vec<usize>,
}

impl PriorUserTurnTracker {
    pub(crate) fn observe(&mut self, items: &[RolloutItem], idx: usize, item: &RolloutItem) {
        let legacy_mail = self.legacy_inter_agent_mail.observe(item).is_some();
        self.explicit_turn.observe(items, idx, item);
        if let Some(start) = self.explicit_turn.instruction_start(items, idx, item, legacy_mail) {
            push_instruction_start(&mut self.rollback_turn_positions, start);
        }
        if rollout_item_is_user_turn_boundary(item) || legacy_mail {
            self.user_turn_positions.push(idx);
        }
        if let RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) = item {
            let num_turns = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
            if let Some(rollback_start_idx) = apply_rollback_to_instruction_turn_positions(
                &mut self.rollback_turn_positions,
                num_turns,
            ) {
                self.user_turn_positions
                    .retain(|position| *position < rollback_start_idx);
            }
        }
    }

    pub(crate) fn has_prior_user_turns(&self) -> bool {
        !self.user_turn_positions.is_empty()
    }
}

fn rollout_item_is_user_turn_boundary(item: &RolloutItem) -> bool {
    match item {
        RolloutItem::ResponseItem(item) => is_user_turn_boundary(item),
        RolloutItem::InterAgentCommunication(_) => true,
        _ => false,
    }
}

/// Return the indices of user message boundaries in a rollout.
///
/// A user message boundary is a `RolloutItem::ResponseItem(ResponseItem::Message { .. })`
/// whose parsed turn item is `TurnItem::UserMessage`.
///
/// Rollouts can contain `ThreadRolledBack` markers. Those markers remove instruction turns,
/// including inter-agent instructions, so we track those boundaries separately from the real
/// user-message positions returned by this function.
pub(crate) fn user_message_positions_in_rollout(items: &[RolloutItem]) -> Vec<usize> {
    let mut legacy_inter_agent_mail = LegacyInterAgentMailReader::default();
    let mut explicit_turn = ExplicitTurn::default();
    let mut rollback_turn_positions = Vec::new();
    let mut user_positions = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let legacy_mail = legacy_inter_agent_mail.observe(item).is_some();
        explicit_turn.observe(items, idx, item);
        if let Some(start) = explicit_turn.instruction_start(items, idx, item, legacy_mail) {
            push_instruction_start(&mut rollback_turn_positions, start);
        }
        match item {
            RolloutItem::ResponseItem(item @ ResponseItem::Message { .. })
                if matches!(
                    event_mapping::parse_turn_item(item),
                    Some(TurnItem::UserMessage(_))
                ) =>
            {
                user_positions.push(idx);
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                let num_turns = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
                if let Some(rollback_start_idx) = apply_rollback_to_instruction_turn_positions(
                    &mut rollback_turn_positions,
                    num_turns,
                ) {
                    user_positions.retain(|position| *position < rollback_start_idx);
                }
            }
            _ => {}
        }
    }
    user_positions
}

#[derive(Clone, Copy)]
struct ForkTurnBoundary {
    start: usize,
    // Compaction replacement histories store model items, not TurnStarted.
    anchor: usize,
}

#[derive(Default)]
struct ForkBoundaries {
    turns: Vec<ForkTurnBoundary>,
    checkpoints: Vec<usize>,
}

/// Find effective fork turns. Only real user inputs and trusted triggering mail
/// count toward Last-N. Explicit turns contribute at most one boundary, while
/// older records without a start retain their per-instruction boundaries.
fn fork_boundaries(items: &[RolloutItem]) -> ForkBoundaries {
    let mut legacy_inter_agent_mail = LegacyInterAgentMailReader::default();
    let mut explicit_turn = ExplicitTurn::default();
    let mut rollback_turn_positions = Vec::new();
    let mut boundaries = ForkBoundaries::default();
    for (idx, item) in items.iter().enumerate() {
        let legacy_mail = legacy_inter_agent_mail.observe(item);
        explicit_turn.observe(items, idx, item);
        if let Some(start) = explicit_turn.instruction_start(items, idx, item, legacy_mail.is_some())
        {
            push_instruction_start(&mut rollback_turn_positions, start);
        }
        let forkable = match item {
            RolloutItem::ResponseItem(item) => {
                is_real_user_message_boundary(item)
                    || legacy_mail.is_some_and(|mail| mail.trigger_turn)
            }
            RolloutItem::InterAgentCommunication(communication) => communication.trigger_turn,
            RolloutItem::InterAgentCommunicationMetadata { trigger_turn, .. } => *trigger_turn,
            _ => false,
        };
        if forkable {
            let start = explicit_turn.start.unwrap_or(idx);
            if boundaries.turns.last().is_none_or(|turn| turn.start != start) {
                boundaries.turns.push(ForkTurnBoundary { start, anchor: idx });
            }
        }
        match item {
            RolloutItem::Compacted(_) => boundaries.checkpoints.push(idx),
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                let num_turns = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
                if let Some(start) = apply_rollback_to_instruction_turn_positions(
                    &mut rollback_turn_positions,
                    num_turns,
                ) {
                    boundaries
                        .turns
                        .truncate(boundaries.turns.partition_point(|turn| turn.start < start));
                    boundaries.checkpoints.truncate(
                        boundaries.checkpoints.partition_point(|index| *index < start),
                    );
                }
            }
            _ => {}
        }
    }
    boundaries
}

/// Filter owned history while repairing rollback counts for removed instructions.
pub(crate) fn filter_rollout_preserving_rollbacks(
    items: Vec<RolloutItem>,
    origins: &DiagnosticOrigins,
    keep: impl FnMut(&RolloutItem) -> bool,
) -> (Vec<RolloutItem>, DiagnosticOrigins) {
    let decisions = rollout_filter_decisions(&items, keep);
    // The owned source is consumed below. Retain only global diagnostics from
    // discarded records so rollback cannot erase evidence of source gaps.
    let notices = items.iter().zip(&decisions)
        .filter(|(_, decision)| !decision.0)
        .flat_map(|(item, _)| match item {
            RolloutItem::ResponseItem(item) => std::slice::from_ref(item),
            RolloutItem::Compacted(item) => item.replacement_history.as_deref().unwrap_or_default(),
            _ => &[],
        })
        .filter(|item| codex_rollout::is_rollout_reconstruction_notice(item))
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    let mut retained = (Vec::new(), DiagnosticOrigins::new());
    for (index, (item, decision)) in items.into_iter().zip(decisions).enumerate() {
        if let Some(item) = apply_rollout_filter_decision(item, decision) {
            push_selected_item(&mut retained, item, origins, index);
        }
    }
    (carry_source_integrity_notice(&notices, retained.0), retained.1)
}

/// Borrow a shared parent until selection is known; clone retained payloads only.
pub(crate) fn filter_borrowed_rollout_preserving_rollbacks(
    items: &[RolloutItem],
    origins: &DiagnosticOrigins,
    keep: impl FnMut(&RolloutItem) -> bool,
    mut clone_retained: impl FnMut(&RolloutItem) -> RolloutItem,
) -> (Vec<RolloutItem>, DiagnosticOrigins) {
    let decisions = rollout_filter_decisions(items, keep);
    let mut retained = (Vec::new(), DiagnosticOrigins::new());
    for (index, (item, decision)) in items.iter().zip(decisions).enumerate() {
        if let Some(item) = decision.0.then(|| clone_retained(item))
            .and_then(|item| apply_rollout_filter_decision(item, decision))
        {
            push_selected_item(&mut retained, item, origins, index);
        }
    }
    (carry_source_integrity_notice(items, retained.0), retained.1)
}

fn push_selected_item(
    (items, retained_origins): &mut (Vec<RolloutItem>, DiagnosticOrigins),
    item: RolloutItem,
    origins: &DiagnosticOrigins,
    source_index: usize,
) {
    if let Some(origin) = origins.get(&source_index) {
        retained_origins.insert(items.len(), origin.clone());
    }
    items.push(item);
}

fn rollout_filter_decisions(
    items: &[RolloutItem],
    mut keep: impl FnMut(&RolloutItem) -> bool,
) -> Vec<(bool, Option<u32>)> {
    let mut legacy_mail = LegacyInterAgentMailReader::default();
    let mut instruction_turns: Vec<(bool, usize)> = Vec::new();
    let mut explicit_turn = ExplicitTurn::default();
    let mut replay_effects: Vec<usize> = Vec::new();
    let mut decisions: Vec<(bool, Option<u32>)> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let legacy = legacy_mail.observe(item).is_some();
        let retained = keep(item);
        explicit_turn.observe(items, index, item);
        let instruction_start = explicit_turn.instruction_start(items, index, item, legacy);
        match item {
            RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::Compacted(_)
            | RolloutItem::ContextReplay { .. }
            | RolloutItem::SamplingBoundary(_)
            | RolloutItem::AdditionalContextState { .. }
            | RolloutItem::ResponseItem(_) if retained && instruction_start.is_none() => {
                replay_effects.push(index);
            }
            _ => {}
        }
        if let Some(start) = instruction_start {
            // If any instruction survives filtering, replay still counts the turn.
            match instruction_turns.last_mut() {
                Some((keep, previous)) if *previous == start => *keep |= retained,
                _ => instruction_turns.push((retained, start)),
            }
        }
        let rollback = if let RolloutItem::EventMsg(EventMsg::ThreadRolledBack(event)) = item {
            let count = usize::try_from(event.num_turns).unwrap_or(usize::MAX);
            let start = instruction_turns.len().saturating_sub(count);
            let context_start = if count > instruction_turns.len() {
                Some(0)
            } else {
                instruction_turns.get(start).map(|(_, start)| *start)
            };
            if let Some(context_start) = context_start {
                // Retire rollback-owned context and model history before filtering
                // away mail boundaries or zero-count markers. Retained instruction
                // boundaries stay until the repaired marker consumes them.
                while let Some(&effect_index) = replay_effects.last() {
                    if effect_index < context_start {
                        break;
                    }
                    replay_effects.pop();
                    decisions[effect_index].0 = false;
                }
            }
            let retained_count = instruction_turns[start..]
                .iter()
                .filter(|(keep, _)| *keep)
                .count();
            instruction_turns.truncate(start);
            // This cannot exceed the original u32 rollback count.
            Some(u32::try_from(retained_count).unwrap_or(event.num_turns))
        } else {
            None
        };
        decisions.push((retained && rollback != Some(0), rollback));
    }
    decisions
}

fn apply_rollout_filter_decision(
    mut item: RolloutItem,
    (keep, rollback): (bool, Option<u32>),
) -> Option<RolloutItem> {
    if !keep { return None; }
    if let (RolloutItem::EventMsg(EventMsg::ThreadRolledBack(event)), Some(count)) = (&mut item, rollback) {
        event.num_turns = count;
    }
    Some(item)
}

#[derive(Default)]
struct ExplicitTurn {
    start: Option<usize>,
}

impl ExplicitTurn {
    fn observe(&mut self, items: &[RolloutItem], idx: usize, item: &RolloutItem) {
        let active_id = self.start.and_then(|start| match &items[start] {
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => Some(event.turn_id.as_str()),
            _ => None,
        });
        match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(_)) => self.start = Some(idx),
            RolloutItem::EventMsg(
                EventMsg::TurnComplete(event) | EventMsg::ImportedTurnClosed(event),
            ) if active_id == Some(event.turn_id.as_str()) => self.start = None,
            RolloutItem::EventMsg(EventMsg::TurnAborted(event))
                if event.turn_id.as_deref().is_none_or(|id| active_id == Some(id)) =>
            {
                self.start = None;
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(event)) if event.num_turns > 0 => {
                self.start = None;
            }
            _ => {}
        }
    }

    fn instruction_start(
        &self,
        items: &[RolloutItem],
        idx: usize,
        item: &RolloutItem,
        legacy_mail: bool,
    ) -> Option<usize> {
        let explicit_input = self.start.is_some() && match item {
            RolloutItem::EventMsg(EventMsg::UserMessage(_)) => true,
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => matches!(
                &event.item,
                TurnItem::UserMessage(message) if crate::event_mapping::is_user_input_item_id(&message.id)
            ),
            _ => false,
        };
        (explicit_input || is_rollback_instruction_turn_boundary(items, idx, item, legacy_mail))
            .then_some(self.start.unwrap_or(idx))
    }
}

fn push_instruction_start(positions: &mut Vec<usize>, start: usize) {
    if positions.last().copied() != Some(start) {
        positions.push(start);
    }
}

/// `legacy_inter_agent_mail` is the `LegacyInterAgentMailReader` verdict for `item`.
fn is_rollback_instruction_turn_boundary(
    items: &[RolloutItem],
    idx: usize,
    item: &RolloutItem,
    legacy_inter_agent_mail: bool,
) -> bool {
    match item {
        RolloutItem::ResponseItem(item) => {
            let has_delivery_metadata = matches!(item, ResponseItem::AgentMessage { .. })
                && idx.checked_sub(1).is_some_and(|previous_idx| {
                    matches!(
                        items.get(previous_idx),
                        Some(RolloutItem::InterAgentCommunicationMetadata { .. })
                    )
                });
            (is_user_turn_boundary(item) || legacy_inter_agent_mail) && !has_delivery_metadata
        }
        RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. } => true,
        _ => false,
    }
}

fn apply_rollback_to_instruction_turn_positions(
    rollback_turn_positions: &mut Vec<usize>,
    num_turns: usize,
) -> Option<usize> {
    if num_turns == 0 {
        return None;
    }
    let rollback_start_idx = rollback_turn_positions
        .len()
        .checked_sub(num_turns)
        .map(|rollback_start| rollback_turn_positions[rollback_start])
        .or_else(|| rollback_turn_positions.first().copied())?;
    let new_len = rollback_turn_positions.len().saturating_sub(num_turns);
    rollback_turn_positions.truncate(new_len);
    Some(rollback_start_idx)
}

/// A crop cannot prove the source complete: the loader may have skipped records
/// anywhere in it. Carry only its authenticated diagnostic, never excluded turns.
pub(crate) fn carry_source_integrity_notice(
    source: &[RolloutItem],
    mut retained: Vec<RolloutItem>,
) -> Vec<RolloutItem> {
    if retained.is_empty() {
        return retained;
    }
    for item in source {
        let notices: &[ResponseItem] = match item {
            RolloutItem::ResponseItem(item) => std::slice::from_ref(item),
            RolloutItem::Compacted(item) => item.replacement_history.as_deref().unwrap_or_default(),
            _ => &[],
        };
        for notice in notices
            .iter()
            .filter(|item| codex_rollout::is_rollout_reconstruction_notice(item))
        {
            let already_retained = retained.iter().any(|item| match item {
                RolloutItem::ResponseItem(item) => item == notice,
                RolloutItem::Compacted(item) => item
                    .replacement_history
                    .as_ref()
                    .is_some_and(|items| items.contains(notice)),
                _ => false,
            });
            if !already_retained {
                retained.push(RolloutItem::ResponseItem(notice.clone()));
            }
        }
    }
    retained
}

/// Return a rollout prefix ending after the requested persisted terminal turn.
///
/// The turn must still be present in the effective post-rollback history and
/// must have an explicit persisted TurnStarted boundary. Synthetic IDs
/// generated while projecting legacy rollouts are intentionally unsupported
/// because they do not provide a stable raw rollout boundary for a fork.
pub fn truncate_rollout_after_turn_id(
    items: &[RolloutItem],
    last_turn_id: &str,
) -> CodexResult<Vec<RolloutItem>> {
    let turns = build_turns_from_rollout_items(items);
    let turn = turns
        .iter()
        .find(|turn| turn.id == last_turn_id)
        .ok_or_else(|| {
            CodexErr::InvalidRequest(format!(
                "lastTurnId '{last_turn_id}' was not found in the source thread"
            ))
        })?;

    let target_start_index = items
        .iter()
        .position(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::TurnStarted(event))
                    if event.turn_id == last_turn_id
            )
        })
        .ok_or_else(|| {
            CodexErr::InvalidRequest(format!(
                "lastTurnId '{last_turn_id}' is not a persisted canonical turn in the source thread"
            ))
        })?;

    if matches!(turn.status, TurnStatus::InProgress) {
        return Err(CodexErr::InvalidRequest(format!(
            "lastTurnId '{last_turn_id}' identifies an in-progress turn"
        )));
    }

    let mut active_turn_id = last_turn_id;
    let terminal_index = items
        .iter()
        .enumerate()
        .skip(target_start_index.saturating_add(1))
        .find_map(|(index, item)| match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                active_turn_id = &event.turn_id;
                None
            }
            RolloutItem::EventMsg(
                EventMsg::TurnComplete(event) | EventMsg::ImportedTurnClosed(event),
            ) if event.turn_id == last_turn_id => Some(index),
            RolloutItem::EventMsg(EventMsg::TurnAborted(event))
                if event.turn_id.as_deref().unwrap_or(active_turn_id) == last_turn_id =>
            {
                Some(index)
            }
            _ => None,
        })
        .ok_or_else(|| {
            CodexErr::InvalidRequest(format!(
                "lastTurnId '{last_turn_id}' has no persisted terminal boundary"
            ))
        })?;
    // A terminal may be appended after a successor started. A raw prefix then
    // contains that successor's input and checkpoints: refuse it rather than
    // silently fork a different logical history.
    if items[target_start_index + 1..terminal_index]
        .iter()
        .any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))))
    {
        return Err(CodexErr::InvalidRequest(format!(
            "lastTurnId '{last_turn_id}' has an interleaved successor before its terminal boundary; cannot safely fork this prefix"
        )));
    }
    Ok(carry_source_integrity_notice(
        items,
        items[..=terminal_index].to_vec(),
    ))
}

/// Return a suffix of `items` that keeps the last `n_from_end` fork turns.
///
/// If fewer than or equal to `n_from_end` fork turns exist, this keeps from the first fork-turn
/// boundary and still drops pre-turn startup context.
/// Checkpoints in the suffix must be cut at that same boundary, not allowed to
/// replace the child history with the parent's excluded prefix. If the boundary
/// is no longer identifiable, refuse the fork rather than lose checkpoint-only
/// content or silently restore excluded turns. A checkpoint's handoff state
/// (a local summary or opaque provider checkpoint) is its only record of the
/// retained turns' summarized work, so it survives the cut wherever the
/// checkpoint placed it.
pub(crate) fn truncate_rollout_to_last_n_fork_turns(
    items: &[RolloutItem],
    origins: &DiagnosticOrigins,
    n_from_end: usize,
) -> CodexResult<(Vec<RolloutItem>, DiagnosticOrigins)> {
    if n_from_end == 0 {
        return Ok((Vec::new(), DiagnosticOrigins::new()));
    }

    let boundaries = fork_boundaries(items);
    let Some(ForkTurnBoundary { start: keep_idx, anchor }) = boundaries
        .turns
        .get(boundaries.turns.len().saturating_sub(n_from_end))
        .copied()
    else {
        return Ok((Vec::new(), DiagnosticOrigins::new()));
    };
    let mut retained = (Vec::with_capacity(items.len() - keep_idx), DiagnosticOrigins::new());
    // A pre-input compaction belongs to the excluded history, even if planning
    // wrote it after TurnStarted. Keep the opening event, not that checkpoint.
    for (index, item) in items.iter().enumerate().skip(keep_idx) {
        if index >= anchor || !matches!(item, RolloutItem::Compacted(_)) {
            push_selected_item(&mut retained, item.clone(), origins, index);
        }
    }
    let (mut retained, retained_origins) = retained;
    let anchor_offset = retained.len() - (items.len() - anchor);
    if boundaries.checkpoints.last().is_none_or(|index| *index < anchor) {
        return Ok((carry_source_integrity_notice(items, retained), retained_origins));
    }
    let cannot_bound_checkpoint = || {
        CodexErr::InvalidRequest(
        "Cannot fork last-N turns: a compaction checkpoint does not uniquely identify the retained turn boundary. Use full-history or task-capsule mode instead.".into(),
    )
    };
    let boundary = match &items[anchor] {
        RolloutItem::ResponseItem(item) => std::borrow::Cow::Borrowed(item),
        RolloutItem::InterAgentCommunication(communication) => {
            std::borrow::Cow::Owned(communication.to_model_input_item())
        }
        RolloutItem::InterAgentCommunicationMetadata { .. } => {
            let Some(RolloutItem::ResponseItem(item)) = items.get(anchor + 1) else {
                return Err(cannot_bound_checkpoint());
            };
            std::borrow::Cow::Borrowed(item)
        }
        _ => return Err(cannot_bound_checkpoint()),
    };
    let matches_boundary = |item: &ResponseItem| match (boundary.id(), item.id()) {
        (Some(expected), Some(actual)) => expected == actual,
        (None, None) => boundary.as_ref() == item,
        _ => false,
    };
    // Legacy messages lack stable IDs. Repeated raw messages make even a
    // single checkpoint match ambiguous; never guess using text similarity.
    let matching_positions = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let matched = match item {
                RolloutItem::ResponseItem(item) => matches_boundary(item),
                RolloutItem::InterAgentCommunication(communication) => {
                    matches_boundary(&communication.to_model_input_item())
                }
                _ => false,
            };
            matched.then_some(index)
        })
        .collect::<Vec<_>>();
    for (offset, item) in retained[anchor_offset..].iter_mut().enumerate() {
        let checkpoint_index = anchor + offset;
        // A rolled-back checkpoint is retired by the fork filter; it must
        // neither determine what is inherited nor veto a valid cut.
        if let RolloutItem::Compacted(compacted) = item
            && boundaries.checkpoints.binary_search(&checkpoint_index).is_ok()
        {
            if matching_positions.partition_point(|index| *index < checkpoint_index) != 1 {
                return Err(cannot_bound_checkpoint());
            }
            let history = compacted
                .replacement_history
                .as_mut()
                .ok_or_else(cannot_bound_checkpoint)?;
            let mut matches = history
                .iter()
                .enumerate()
                .filter_map(|(index, item)| matches_boundary(item).then_some(index));
            let start = matches.next().ok_or_else(cannot_bound_checkpoint)?;
            if matches.next().is_some() {
                return Err(cannot_bound_checkpoint());
            }
            let mut index = 0;
            history.retain(|item| {
                // Source-integrity diagnostics are global, not excluded turns.
                // Local compaction inserts its handoff before the retained task
                // input; dropping it would erase the retained turns' work.
                let keep = index >= start
                    || codex_rollout::is_rollout_reconstruction_notice(item)
                    || is_checkpoint_handoff(item);
                index += 1;
                keep
            });
        }
    }
    Ok((carry_source_integrity_notice(items, retained), retained_origins))
}

fn is_real_user_message_boundary(item: &ResponseItem) -> bool {
    matches!(
        event_mapping::parse_turn_item(item),
        Some(TurnItem::UserMessage(_))
    )
}

/// Checkpoint-only handoff state. A summary is recognized by its harness-owned
/// ID and prefix, never by text alone, so a user message cannot claim it.
fn is_checkpoint_handoff(item: &ResponseItem) -> bool {
    crate::compact::is_compaction_summary_item(item)
        || matches!(
            item,
            ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
        )
}
