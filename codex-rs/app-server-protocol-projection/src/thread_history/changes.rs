//! Incremental history snapshots and end-of-batch change coalescing.
//!
//! The parent reducer owns event order, rollback, and the replay state lifetime.

use super::PendingTurn;
use super::ThreadHistoryBuilder;
use codex_app_server_protocol_types::SurfacedToolResult;
use codex_app_server_protocol_types::ThreadItem;
use codex_app_server_protocol_types::Turn;
use codex_app_server_protocol_types::TurnError;
use codex_app_server_protocol_types::TurnStatus;
use codex_app_server_protocol_types::TurnTiming;
use indexmap::IndexSet;

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
pub(super) struct ThreadHistoryChangeAccumulator {
    pub(super) changed_items: IndexSet<(usize, usize)>,
    pub(super) changed_turns: IndexSet<usize>,
    pub(super) removed_turn_ids: IndexSet<String>,
}

impl ThreadHistoryChangeAccumulator {
    #[expect(
        clippy::expect_used,
        reason = "The accumulator records live turn positions and rollback removes invalid positions before finishing"
    )]
    pub(super) fn finish(self, builder: &ThreadHistoryBuilder) -> ThreadHistoryChangeSet {
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

