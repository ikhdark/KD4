//! Snapshot selection, turn-boundary detection, and fork-history reconstruction.
//!
//! These pure history transformations are shared by manager forks and resume
//! recovery; thread lifecycle and persistence stay in the parent module.

use crate::rollout::truncation;
use crate::tasks::InterruptedTurnHistoryMarker;
use crate::tasks::interrupted_turn_history_marker;
use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkSnapshot {
    /// Fork the committed prefix prepared for the most recent physical model
    /// attempt. Metadata after the marker is deliberately excluded.
    ///
    /// Legacy marker-less histories fall back to the active turn's opening
    /// boundary, while histories already between turns remain unchanged.
    TruncateToLastSamplingBoundary,

    /// Fork a committed prefix ending strictly before the nth user message.
    ///
    /// When `n` is within range, this cuts before that 0-based user-message
    /// boundary. When `n` is out of range and the source thread is currently
    /// mid-turn, this instead cuts before the active turn's opening boundary
    /// so the fork drops the unfinished turn suffix. When `n` is out of range
    /// and the source thread is already at a turn boundary, this returns the
    /// full committed history unchanged.
    TruncateBeforeNthUserMessage(usize),

    /// Fork the current persisted history as if the source thread had been
    /// interrupted now.
    ///
    /// If the persisted snapshot ends mid-turn, this appends the same
    /// `<turn_aborted>` marker produced by a real interrupt. If the snapshot is
    /// already at a turn boundary, this returns the current persisted history
    /// unchanged.
    Interrupted,
}

/// Preserve legacy `fork_thread(usize, ...)` callsites by mapping them to the
/// existing truncate-before-nth-user-message snapshot mode.
impl From<usize> for ForkSnapshot {
    fn from(value: usize) -> Self {
        Self::TruncateBeforeNthUserMessage(value)
    }
}

fn truncate_before_nth_user_message(
    history: InitialHistory,
    n: usize,
    snapshot_state: &SnapshotTurnState,
) -> InitialHistory {
    let items = history.get_rollout_items();
    let user_positions = truncation::user_message_positions_in_rollout(items);
    let cut = if snapshot_state.ends_mid_turn && n >= user_positions.len() {
        snapshot_state
            .active_turn_start_index
            .or_else(|| user_positions.last().copied())
            .unwrap_or(items.len())
    } else if n < user_positions.len() {
        user_positions[n]
    } else {
        items.len()
    };
    truncate_owned_initial_history(history, cut)
}

#[derive(Debug, Eq, PartialEq)]
struct SnapshotTurnState {
    ends_mid_turn: bool,
    active_turn_id: Option<String>,
    active_turn_start_index: Option<usize>,
}

/// The turn a persisted rollout ends inside when the process stopped before
/// recording a completion or an interruption, as `Some(explicit turn id)`.
///
/// Unlike fork snapshots, a resumed rollout with only response items and no
/// lifecycle events is treated as finished: older rollouts recorded completed
/// exchanges that way, and no evidence says the last request was cut short.
pub(crate) fn resumed_rollout_unfinished_turn(
    rollout_items: &[RolloutItem],
) -> Option<Option<String>> {
    let mut builder = ThreadHistoryBuilder::new();
    for item in rollout_items {
        builder.handle_rollout_item(item);
    }
    if !builder.has_active_turn() {
        return None;
    }
    let active_turn_id = builder.active_turn_id_if_explicit();
    if active_turn_id.is_some() {
        let finished = builder
            .active_turn_snapshot()
            .as_ref()
            .is_some_and(|turn| turn.status != TurnStatus::InProgress);
        return (!finished).then_some(active_turn_id);
    }
    builder
        .active_turn_start_index()
        .filter(|index| implicit_legacy_turn_is_unfinished(rollout_items, *index))
        .map(|_| None)
}

fn snapshot_turn_state(history: &InitialHistory) -> SnapshotTurnState {
    let rollout_items = history.get_rollout_items();
    let mut builder = ThreadHistoryBuilder::new();
    for item in rollout_items {
        builder.handle_rollout_item(item);
    }
    let active_turn_id = builder.active_turn_id_if_explicit();
    if builder.has_active_turn() && active_turn_id.is_some() {
        if builder
            .active_turn_status()
            .is_some_and(|status| status != TurnStatus::InProgress)
        {
            return SnapshotTurnState {
                ends_mid_turn: false,
                active_turn_id: None,
                active_turn_start_index: None,
            };
        }

        return SnapshotTurnState {
            ends_mid_turn: true,
            active_turn_id,
            active_turn_start_index: builder.active_turn_start_index(),
        };
    }

    if builder.has_active_turn()
        && let Some(active_turn_start_index) = builder.active_turn_start_index()
        && implicit_legacy_turn_is_unfinished(rollout_items, active_turn_start_index)
    {
        return SnapshotTurnState {
            ends_mid_turn: true,
            active_turn_id: None,
            active_turn_start_index: Some(active_turn_start_index),
        };
    }

    let Some(last_user_position) = truncation::user_message_positions_in_rollout(rollout_items)
        .last()
        .copied()
    else {
        return SnapshotTurnState {
            ends_mid_turn: false,
            active_turn_id: None,
            active_turn_start_index: None,
        };
    };

    // Synthetic fork/resume histories can contain user/assistant response items
    // without explicit turn lifecycle events. If the persisted snapshot has no
    // terminating boundary after its last user message, treat it as mid-turn.
    SnapshotTurnState {
        ends_mid_turn: !rollout_items[last_user_position + 1..].iter().any(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(
                    EventMsg::TurnComplete(_)
                        | EventMsg::TurnAborted(_)
                        | EventMsg::ImportedTurnClosed(_)
                )
            )
        }),
        active_turn_id: None,
        active_turn_start_index: None,
    }
}

fn implicit_legacy_turn_is_unfinished(
    rollout_items: &[RolloutItem],
    active_turn_start_index: usize,
) -> bool {
    // Legacy input opens the implicit turn itself, or joins the raw user item
    // that opened it as that item's mirror.
    let has_legacy_input = match rollout_items.get(active_turn_start_index) {
        Some(RolloutItem::EventMsg(EventMsg::UserMessage(_))) => true,
        Some(RolloutItem::ResponseItem(item))
            if crate::context_manager::is_user_turn_boundary(item) =>
        {
            rollout_items[active_turn_start_index + 1..]
                .iter()
                .any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::UserMessage(_))))
        }
        _ => false,
    };
    if !has_legacy_input {
        return false;
    }

    !rollout_items[active_turn_start_index + 1..]
        .iter()
        .any(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::AgentMessage(message))
                    if !message.message.is_empty()
            ) || matches!(
                item,
                RolloutItem::EventMsg(
                    EventMsg::TurnComplete(_)
                        | EventMsg::TurnAborted(_)
                        | EventMsg::ImportedTurnClosed(_)
                )
            )
        })
}

/// Reconstruct the client-visible history of an interrupted fork without
/// introducing model-only interruption guidance. Pathless forks have no rollout
/// to read back, but must report the same turn boundaries as persisted forks.
pub fn interrupted_fork_history_for_display(history: InitialHistory) -> InitialHistory {
    fork_history_from_snapshot(
        ForkSnapshot::Interrupted,
        history,
        InterruptedTurnHistoryMarker::Disabled,
    )
}

pub(super) fn fork_history_from_snapshot(
    snapshot: ForkSnapshot,
    history: InitialHistory,
    interrupted_marker: InterruptedTurnHistoryMarker,
) -> InitialHistory {
    match snapshot {
        ForkSnapshot::TruncateToLastSamplingBoundary => truncate_to_last_sampling_boundary(history),
        ForkSnapshot::TruncateBeforeNthUserMessage(nth_user_message) => {
            let snapshot_state = snapshot_turn_state(&history);
            truncate_before_nth_user_message(history, nth_user_message, &snapshot_state)
        }
        ForkSnapshot::Interrupted => {
            let snapshot_state = snapshot_turn_state(&history);
            let history = match history {
                InitialHistory::New => InitialHistory::New,
                InitialHistory::Cleared => InitialHistory::Cleared,
                InitialHistory::Forked(history) => InitialHistory::Forked(history),
                InitialHistory::Resumed(resumed) => {
                    InitialHistory::Forked(Arc::unwrap_or_clone(resumed.history))
                }
            };
            if snapshot_state.ends_mid_turn {
                append_interrupted_boundary(
                    history,
                    snapshot_state.active_turn_id,
                    interrupted_marker,
                )
            } else {
                history
            }
        }
    }
}

fn truncate_to_last_sampling_boundary(history: InitialHistory) -> InitialHistory {
    let items = history.get_rollout_items();
    if let Some(boundary) = items
        .iter()
        .rposition(|item| matches!(item, RolloutItem::SamplingBoundary(_)))
    {
        return truncate_owned_initial_history(history, boundary + 1);
    }
    let snapshot_state = snapshot_turn_state(&history);
    let items = history.get_rollout_items();
    let cut = if snapshot_state.ends_mid_turn {
        snapshot_state
            .active_turn_start_index
            .or_else(|| {
                truncation::user_message_positions_in_rollout(items)
                    .last()
                    .copied()
            })
            .unwrap_or(0)
    } else {
        items.len()
    };
    truncate_owned_initial_history(history, cut)
}

fn truncate_owned_initial_history(history: InitialHistory, cut: usize) -> InitialHistory {
    let source = history.get_rollout_items();
    let cut = cut.min(source.len());
    // Capture only authenticated global notices from the discarded suffix.
    let notices = if cut == 0 || cut == source.len() {
        Vec::new()
    } else {
        source[cut..]
            .iter()
            .flat_map(|item| match item {
                RolloutItem::ResponseItem(item) => std::slice::from_ref(item),
                RolloutItem::Compacted(item) => {
                    item.replacement_history.as_deref().unwrap_or_default()
                }
                _ => &[],
            })
            .filter(|item| codex_rollout::is_rollout_reconstruction_notice(item))
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect()
    };
    let retained = match history {
        InitialHistory::New | InitialHistory::Cleared => Vec::new(),
        InitialHistory::Forked(mut items) => {
            items.truncate(cut);
            items
        }
        InitialHistory::Resumed(resumed) => match Arc::try_unwrap(resumed.history) {
            Ok(mut items) => {
                items.truncate(cut);
                items
            }
            Err(shared) => shared[..cut].to_vec(),
        },
    };
    if retained.is_empty() {
        InitialHistory::New
    } else {
        InitialHistory::Forked(truncation::carry_source_integrity_notice(
            &notices, retained,
        ))
    }
}

/// Append the same persisted interrupt boundary used by the live interrupt path
/// to an existing fork snapshot after the source thread has been confirmed to
/// be mid-turn.
fn append_interrupted_boundary(
    history: InitialHistory,
    turn_id: Option<String>,
    interrupted_marker: InterruptedTurnHistoryMarker,
) -> InitialHistory {
    let aborted_event = RolloutItem::EventMsg(EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id,
        reason: TurnAbortReason::Interrupted,
        completed_at: None,
        duration_ms: None,
        timing: None,
    }));

    match history {
        InitialHistory::New | InitialHistory::Cleared => {
            let mut history = Vec::new();
            if let Some(marker) = interrupted_turn_history_marker(interrupted_marker) {
                history.push(RolloutItem::ResponseItem(marker));
            }
            history.push(aborted_event);
            InitialHistory::Forked(history)
        }
        InitialHistory::Forked(mut history) => {
            if let Some(marker) = interrupted_turn_history_marker(interrupted_marker) {
                history.push(RolloutItem::ResponseItem(marker));
            }
            history.push(aborted_event);
            InitialHistory::Forked(history)
        }
        InitialHistory::Resumed(resumed) => {
            let mut history = Arc::unwrap_or_clone(resumed.history);
            if let Some(marker) = interrupted_turn_history_marker(interrupted_marker) {
                history.push(RolloutItem::ResponseItem(marker));
            }
            history.push(aborted_event);
            InitialHistory::Forked(history)
        }
    }
}
