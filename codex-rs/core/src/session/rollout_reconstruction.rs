use super::*;
use crate::context::world_state::WorldStateSnapshot;
use crate::context_manager::is_user_turn_boundary;
use codex_protocol::models::ContentItem;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::SessionContextWindow;
use std::collections::HashSet;
use uuid::Uuid;

const UNIFIED_EXEC_RESUME_INVALIDATION_START: &str = "<unified_exec_resume_invalidated>";
const UNIFIED_EXEC_SESSION_ID_PREFIX: &str = "Process running with session ID ";

pub(crate) fn is_unified_exec_resume_invalidation(item: &ResponseItem) -> bool {
    matches!(item, ResponseItem::Message { role, content, .. }
        if role == "developer" && matches!(content.as_slice(),
            [ContentItem::InputText { text }]
                if text.starts_with(UNIFIED_EXEC_RESUME_INVALIDATION_START)))
}

// Return value of `Session::reconstruct_history_from_rollout`, bundling the rebuilt history with
// the resume/fork hydration metadata derived from the same replay.
#[derive(Debug)]
pub(super) struct RolloutReconstruction {
    pub(super) history: Vec<ResponseItem>,
    pub(super) plan: Option<UpdatePlanArgs>,
    pub(super) previous_turn_settings: Option<PreviousTurnSettings>,
    pub(super) reference_context_item: Option<TurnContextItem>,
    pub(super) world_state_baseline: Option<WorldStateSnapshot>,
    pub(super) window_number: u64,
    pub(super) first_window_id: Option<Uuid>,
    pub(super) previous_window_id: Option<Uuid>,
    pub(super) window_id: Option<Uuid>,
}

pub(super) fn append_unified_exec_resume_invalidation(history: &mut Vec<ResponseItem>) {
    let unified_exec_call_ids = history
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCall {
                name,
                namespace: None,
                call_id,
                ..
            } if matches!(name.as_str(), "exec_command" | "write_stdin") => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let code_mode_call_ids = history
        .iter()
        .filter_map(|item| match item {
            ResponseItem::CustomToolCall {
                name,
                namespace: None,
                call_id,
                ..
            } if name == "exec" => Some(call_id.as_str()),
            ResponseItem::FunctionCall {
                name,
                namespace: None,
                call_id,
                ..
            } if matches!(name.as_str(), "exec" | "wait") => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let has_old_sessions = history.iter().any(|item| match item {
        ResponseItem::FunctionCallOutput {
            call_id, output, ..
        } if unified_exec_call_ids.contains(call_id.as_str()) => {
            output.body.to_text().is_some_and(|output| {
                output.lines().any(|line| {
                    line.strip_prefix(UNIFIED_EXEC_SESSION_ID_PREFIX)
                        .is_some_and(|status| {
                            let id = status.split_once(';').map_or(status, |(id, _)| id);
                            id.trim().parse::<u32>().is_ok()
                        })
                })
            })
        }
        ResponseItem::CustomToolCallOutput {
            call_id, output, ..
        }
        | ResponseItem::FunctionCallOutput {
            call_id, output, ..
        } if code_mode_call_ids.contains(call_id.as_str()) => output
            .body
            .to_text()
            .is_some_and(|text| has_live_nested_command(&text)),
        _ => false,
    });
    // Replace earlier runtime notices so this notice scopes invalidation to the
    // current resume boundary, including when a numeric process ID is reused.
    let mut had_notice = false;
    history.retain(|item| {
        let is_notice = is_unified_exec_resume_invalidation(item);
        had_notice |= is_notice;
        !is_notice
    });
    if !has_old_sessions && !had_notice {
        return;
    }
    let text = format!(
        "{UNIFIED_EXEC_RESUME_INVALIDATION_START}\n\
Process session IDs in the pre-resume history are no longer live. Do not poll those old sessions. \
Newly returned session IDs are valid, even when a number is reused. \
This includes sessions in nested code-mode command receipts. Invalidation does not establish \
whether a command completed or was terminated; retain its recorded output and recovery references, \
and re-establish uncertain effects before repeating it. \
Start another process only when the current task still requires execution; \
do not rerun completed commands merely because this conversation resumed.\n\
</unified_exec_resume_invalidated>"
    );
    history.push(ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
}

fn has_live_nested_command(text: &str) -> bool {
    fn live(states: &serde_json::Value) -> bool {
        states.as_array().is_some_and(|states| {
            states.iter().any(|state| {
                matches!(state["tool"].as_str(), Some("exec_command" | "write_stdin"))
                    && state["process_exited"].as_bool() == Some(false)
                    && state
                        .get("session_id")
                        .filter(|id| !id.is_null())
                        .or_else(|| state.get("polled_session_id"))
                        .and_then(serde_json::Value::as_u64)
                        .is_some_and(|id| u32::try_from(id).is_ok())
            })
        })
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text)
        && [
            "/nested_commands",
            "/essential/nested_commands",
            "/result/essential/nested_commands",
        ]
        .iter()
        .any(|pointer| value.pointer(pointer).is_some_and(live))
    {
        return true;
    }
    let marker = "Nested command states (independent of script completion):\n";
    text.match_indices(marker).any(|(index, _)| {
        if index != 0 && !text[..index].ends_with('\n') {
            return false;
        }
        serde_json::Deserializer::from_str(&text[index + marker.len()..])
            .into_iter::<serde_json::Value>()
            .next()
            .is_some_and(|value| value.as_ref().is_ok_and(live))
    })
}

#[derive(Debug, Clone, Copy)]
struct ReconstructedWindow {
    number: u64,
    first_id: Option<Uuid>,
    previous_id: Option<Uuid>,
    id: Option<Uuid>,
}

#[derive(Debug, Default)]
enum TurnReferenceContextItem {
    /// No `TurnContextItem` has been seen for this replay span yet.
    ///
    /// This differs from `Cleared`: `NeverSet` means there is no evidence this turn ever
    /// established a baseline, while `Cleared` means a baseline existed and a later compaction
    /// invalidated it. Only the latter must emit an explicit clearing segment for resume/fork
    /// hydration.
    #[default]
    NeverSet,
    /// A previously established baseline was invalidated by later compaction.
    Cleared,
    /// The latest baseline established by this replay span.
    Latest(Box<TurnContextItem>),
}

#[derive(Debug, Default)]
struct ActiveReplaySegment<'a> {
    turn_id: Option<String>,
    plan: Option<UpdatePlanArgs>,
    counts_as_user_turn: bool,
    previous_turn_settings: Option<PreviousTurnSettings>,
    reference_context_item: TurnReferenceContextItem,
    world_state_replay: Vec<&'a RolloutItem>,
    history_effect_indexes: Vec<usize>,
    replacement_checkpoint: Option<ReplacementCheckpoint<'a>>,
    compaction_count: u64,
    has_legacy_compaction_without_window_number: bool,
    window: Option<ReconstructedWindow>,
}

#[derive(Debug)]
struct ReplacementCheckpoint<'a> {
    history: &'a [ResponseItem],
    suffix: &'a [RolloutItem],
}

struct FinalizedReplayState<'items, 'state> {
    plan: &'state mut Option<UpdatePlanArgs>,
    base_replacement_history: &'state mut Option<&'items [ResponseItem]>,
    rollout_suffix: &'state mut &'items [RolloutItem],
    discarded_history_effect_indexes: &'state mut HashSet<usize>,
    previous_turn_settings: &'state mut Option<PreviousTurnSettings>,
    reference_context_item: &'state mut TurnReferenceContextItem,
    world_state_replay: &'state mut Vec<&'items RolloutItem>,
    surviving_compaction_count: &'state mut u64,
    has_surviving_legacy_compaction_without_window_number: &'state mut bool,
    window: &'state mut Option<ReconstructedWindow>,
    pending_rollback_turns: &'state mut usize,
}

fn turn_ids_are_compatible(active_turn_id: Option<&str>, item_turn_id: Option<&str>) -> bool {
    active_turn_id
        .is_none_or(|turn_id| item_turn_id.is_none_or(|item_turn_id| item_turn_id == turn_id))
}

fn finalize_active_segment<'a>(
    active_segment: ActiveReplaySegment<'a>,
    state: FinalizedReplayState<'a, '_>,
) {
    let FinalizedReplayState {
        plan,
        base_replacement_history,
        rollout_suffix,
        discarded_history_effect_indexes,
        previous_turn_settings,
        reference_context_item,
        world_state_replay,
        surviving_compaction_count,
        has_surviving_legacy_compaction_without_window_number,
        window,
        pending_rollback_turns,
    } = state;
    // Thread rollback drops the newest surviving real user-message boundaries. In replay, that
    // means skipping the next finalized segments that contain a non-contextual
    // `EventMsg::UserMessage`.
    if *pending_rollback_turns > 0 {
        discarded_history_effect_indexes.extend(active_segment.history_effect_indexes);
        if active_segment.counts_as_user_turn {
            *pending_rollback_turns -= 1;
        }
        return;
    }

    world_state_replay.extend(active_segment.world_state_replay);
    if plan.is_none() {
        *plan = active_segment.plan;
    }
    *surviving_compaction_count =
        (*surviving_compaction_count).saturating_add(active_segment.compaction_count);
    *has_surviving_legacy_compaction_without_window_number |=
        active_segment.has_legacy_compaction_without_window_number;

    // A surviving replacement-history checkpoint is a complete history base. Once we
    // know the newest surviving one, older rollout items do not affect rebuilt history.
    if base_replacement_history.is_none()
        && let Some(checkpoint) = active_segment.replacement_checkpoint
    {
        *base_replacement_history = Some(checkpoint.history);
        *rollout_suffix = checkpoint.suffix;
    }

    if window.is_none() {
        *window = active_segment.window;
    }
    // `previous_turn_settings` come from the newest surviving user turn that established them.
    if previous_turn_settings.is_none() && active_segment.counts_as_user_turn {
        *previous_turn_settings = active_segment.previous_turn_settings;
    }

    // `reference_context_item` comes from the newest surviving user turn baseline, or
    // from a surviving compaction that explicitly cleared that baseline.
    if matches!(reference_context_item, TurnReferenceContextItem::NeverSet)
        && (active_segment.counts_as_user_turn
            || matches!(
                active_segment.reference_context_item,
                TurnReferenceContextItem::Cleared
            ))
    {
        *reference_context_item = active_segment.reference_context_item;
    }
}

impl Session {
    pub(super) async fn reconstruct_history_from_rollout(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> RolloutReconstruction {
        Self::reconstruct_rollout(
            rollout_items,
            turn_context.model_info.truncation_policy.into(),
        )
    }

    /// Rebuild the active model history without truncating retained payloads. Fork admission
    /// must use the same rollback and compaction decisions as the child that will replay it.
    pub(crate) fn reconstruct_model_history_from_rollout(
        rollout_items: &[RolloutItem],
    ) -> Vec<ResponseItem> {
        Self::reconstruct_rollout(
            rollout_items,
            codex_utils_output_truncation::TruncationPolicy::Bytes(usize::MAX),
        )
        .history
    }

    fn reconstruct_rollout(
        rollout_items: &[RolloutItem],
        truncation_policy: codex_utils_output_truncation::TruncationPolicy,
    ) -> RolloutReconstruction {
        // Replay metadata should already match the shape of the future lazy reverse loader, even
        // while history materialization still uses an eager bridge. Scan newest-to-oldest,
        // stopping once a surviving replacement-history checkpoint and the required resume metadata
        // are both known; then replay only the buffered surviving tail forward to preserve exact
        // history semantics.
        let initial_window = rollout_items.iter().find_map(|item| match item {
            RolloutItem::SessionMeta(session_meta) => session_meta
                .meta
                .context_window
                .as_ref()
                .and_then(reconstructed_window_from_session_context_window),
            _ => None,
        });
        let mut base_replacement_history: Option<&[ResponseItem]> = None;
        let mut plan = None;
        let update_plan_call_ids =
            rollout_items
                .iter()
                .filter_map(|item| match item {
                    RolloutItem::ResponseItem(ResponseItem::FunctionCall {
                        name, call_id, ..
                    }) if name == "update_plan" => Some(call_id.as_str()),
                    _ => None,
                })
                .collect::<HashSet<_>>();
        let has_plan_updates = rollout_items
            .iter()
            .any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::PlanUpdate(_))))
            || !update_plan_call_ids.is_empty();
        let mut previous_turn_settings = None;
        let mut reference_context_item = TurnReferenceContextItem::NeverSet;
        let mut world_state_replay = Vec::new();
        let mut surviving_compaction_count = 0u64;
        let mut has_surviving_legacy_compaction_without_window_number = false;
        let mut window = None;
        let has_turn_started = rollout_items
            .iter()
            .any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))));
        let has_segmented_turn_boundaries = rollout_items.iter().any(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(
                    EventMsg::TurnStarted(_) | EventMsg::UserMessage(_) | EventMsg::TurnComplete(_)
                )
            )
        });
        // Rollback is "drop the newest N user turns". While scanning in reverse, that becomes
        // "skip the next N user-turn segments we finalize".
        let mut pending_rollback_turns = 0usize;
        // Reverse replay owns rollback decisions. Track model-history effects from discarded
        // segments so the forward materialization pass does not reapply those effects or the
        // rollback event after metadata replay has already consumed it.
        let mut discarded_history_effect_indexes = HashSet::new();
        // Borrowed suffix of rollout items newer than the newest surviving replacement-history
        // checkpoint. If no such checkpoint exists, this remains the full rollout.
        let mut rollout_suffix = rollout_items;
        // Reverse replay accumulates rollout items into the newest in-progress turn segment until
        // we hit its matching `TurnStarted`, at which point the segment can be finalized.
        let mut active_segment: Option<ActiveReplaySegment<'_>> = None;

        for (index, item) in rollout_items.iter().enumerate().rev() {
            match item {
                RolloutItem::EventMsg(EventMsg::PlanUpdate(update)) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    // The newest committed update survives compaction, but belongs
                    // to its turn so rollback can discard it with that turn.
                    if active_segment.plan.is_none() {
                        active_segment.plan = Some(update.clone());
                    }
                }
                RolloutItem::Compacted(compacted) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.history_effect_indexes.push(index);
                    active_segment.world_state_replay.push(item);
                    active_segment.compaction_count =
                        active_segment.compaction_count.saturating_add(1);
                    active_segment.has_legacy_compaction_without_window_number |=
                        compacted.window_number.is_none();
                    if active_segment.window.is_none()
                        && let Some(window_number) = compacted.window_number
                    {
                        active_segment.window = Some(ReconstructedWindow {
                            number: window_number,
                            first_id: compacted.first_window_id.as_deref().and_then(parse_uuid_v7),
                            previous_id: compacted
                                .previous_window_id
                                .as_deref()
                                .and_then(parse_uuid_v7),
                            id: compacted.window_id.as_deref().and_then(parse_uuid_v7),
                        });
                    }
                    // Looking backward, compaction clears any older baseline unless a newer
                    // `TurnContextItem` in this same segment has already re-established it.
                    if matches!(
                        active_segment.reference_context_item,
                        TurnReferenceContextItem::NeverSet
                    ) {
                        active_segment.reference_context_item = TurnReferenceContextItem::Cleared;
                    }
                    if active_segment.replacement_checkpoint.is_none()
                        && let Some(replacement_history) = &compacted.replacement_history
                    {
                        active_segment.replacement_checkpoint = Some(ReplacementCheckpoint {
                            history: replacement_history,
                            suffix: &rollout_items[index + 1..],
                        });
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    if has_segmented_turn_boundaries {
                        discarded_history_effect_indexes.insert(index);
                        pending_rollback_turns = pending_rollback_turns.saturating_add(
                            usize::try_from(rollback.num_turns).unwrap_or(usize::MAX),
                        );
                    }
                }
                RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    // Reverse replay often sees `TurnComplete` before any turn-scoped metadata.
                    // Capture the turn id early so later `TurnContext` / abort items can match it.
                    if active_segment.turn_id.is_none() {
                        active_segment.turn_id = Some(event.turn_id.clone());
                    }
                }
                RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                    if let Some(active_segment) = active_segment.as_mut() {
                        if active_segment.turn_id.is_none()
                            && let Some(turn_id) = &event.turn_id
                        {
                            active_segment.turn_id = Some(turn_id.clone());
                        }
                    } else if let Some(turn_id) = &event.turn_id {
                        active_segment = Some(ActiveReplaySegment {
                            turn_id: Some(turn_id.clone()),
                            ..Default::default()
                        });
                    }
                }
                RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.counts_as_user_turn = true;
                }
                RolloutItem::TurnContext(ctx) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    // `TurnContextItem` can attach metadata to an existing segment, but only a
                    // real `UserMessage` event should make the segment count as a user turn.
                    if active_segment.turn_id.is_none() {
                        active_segment.turn_id = ctx.turn_id.clone();
                    }
                    if turn_ids_are_compatible(
                        active_segment.turn_id.as_deref(),
                        ctx.turn_id.as_deref(),
                    ) {
                        active_segment.previous_turn_settings = Some(PreviousTurnSettings {
                            model: ctx.model.clone(),
                            comp_hash: ctx.comp_hash.clone(),
                        });
                        if matches!(
                            active_segment.reference_context_item,
                            TurnReferenceContextItem::NeverSet
                        ) {
                            active_segment.reference_context_item =
                                if ctx.context_provenance.is_some() {
                                    TurnReferenceContextItem::Latest(Box::new(ctx.clone()))
                                } else {
                                    // Legacy context state can describe host settings without proving
                                    // the complete model-visible baseline was transmitted.
                                    TurnReferenceContextItem::Cleared
                                };
                        }
                    }
                }
                RolloutItem::SamplingBoundary(boundary) => {
                    if boundary.unresolved_context {
                        let active_segment =
                            active_segment.get_or_insert_with(ActiveReplaySegment::default);
                        if matches!(
                            active_segment.reference_context_item,
                            TurnReferenceContextItem::NeverSet
                        ) {
                            active_segment.reference_context_item =
                                TurnReferenceContextItem::Cleared;
                        }
                    }
                }
                RolloutItem::WorldState(_) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.world_state_replay.push(item);
                }
                RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                    // `TurnStarted` is the oldest boundary of the active reverse segment.
                    if active_segment.as_ref().is_some_and(|active_segment| {
                        turn_ids_are_compatible(
                            active_segment.turn_id.as_deref(),
                            Some(event.turn_id.as_str()),
                        )
                    }) && let Some(active_segment) = active_segment.take()
                    {
                        finalize_active_segment(
                            active_segment,
                            FinalizedReplayState {
                                plan: &mut plan,
                                base_replacement_history: &mut base_replacement_history,
                                rollout_suffix: &mut rollout_suffix,
                                discarded_history_effect_indexes:
                                    &mut discarded_history_effect_indexes,
                                previous_turn_settings: &mut previous_turn_settings,
                                reference_context_item: &mut reference_context_item,
                                world_state_replay: &mut world_state_replay,
                                surviving_compaction_count: &mut surviving_compaction_count,
                                has_surviving_legacy_compaction_without_window_number:
                                    &mut has_surviving_legacy_compaction_without_window_number,
                                window: &mut window,
                                pending_rollback_turns: &mut pending_rollback_turns,
                            },
                        );
                    }
                }
                RolloutItem::ResponseItem(response_item) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.history_effect_indexes.push(index);
                    active_segment.counts_as_user_turn |= is_user_turn_boundary(response_item);
                    // Events and legacy direct outputs share chronological and
                    // rollback ordering; neither representation is always newer.
                    // Eventless histories apply rollback during forward history
                    // replay, so their plans must use the final-history fallback.
                    if has_segmented_turn_boundaries
                        && active_segment.plan.is_none()
                        && let ResponseItem::FunctionCallOutput {
                            call_id, output, ..
                        } = response_item
                        && update_plan_call_ids.contains(call_id.as_str())
                    {
                        active_segment.plan = crate::plan_store::plan_from_tool_output(output);
                    }
                }
                RolloutItem::InterAgentCommunication(_) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.history_effect_indexes.push(index);
                    active_segment.counts_as_user_turn = true;
                }
                RolloutItem::EventMsg(_)
                | RolloutItem::SessionMeta(_)
                | RolloutItem::ToolManifest(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. } => {}
            }

            // Older rollouts used UserMessage as the start event. Finalize at
            // that same boundary so rollback discards one turn and its metadata.
            if !has_turn_started
                && matches!(item, RolloutItem::EventMsg(EventMsg::UserMessage(_)))
                && let Some(active_segment) = active_segment.take()
            {
                finalize_active_segment(
                    active_segment,
                    FinalizedReplayState {
                        plan: &mut plan,
                        base_replacement_history: &mut base_replacement_history,
                        rollout_suffix: &mut rollout_suffix,
                        discarded_history_effect_indexes: &mut discarded_history_effect_indexes,
                        previous_turn_settings: &mut previous_turn_settings,
                        reference_context_item: &mut reference_context_item,
                        world_state_replay: &mut world_state_replay,
                        surviving_compaction_count: &mut surviving_compaction_count,
                        has_surviving_legacy_compaction_without_window_number:
                            &mut has_surviving_legacy_compaction_without_window_number,
                        window: &mut window,
                        pending_rollback_turns: &mut pending_rollback_turns,
                    },
                );
            }

            if base_replacement_history.is_some()
                && previous_turn_settings.is_some()
                && !matches!(reference_context_item, TurnReferenceContextItem::NeverSet)
                && window.is_some()
                && (plan.is_some() || !has_plan_updates)
            {
                // At this point we have the eager resume metadata, the replacement-history base,
                // and an explicit surviving window identity, so older rollout items cannot affect
                // this result.
                break;
            }
        }

        if let Some(active_segment) = active_segment.take() {
            finalize_active_segment(
                active_segment,
                FinalizedReplayState {
                    plan: &mut plan,
                    base_replacement_history: &mut base_replacement_history,
                    rollout_suffix: &mut rollout_suffix,
                    discarded_history_effect_indexes: &mut discarded_history_effect_indexes,
                    previous_turn_settings: &mut previous_turn_settings,
                    reference_context_item: &mut reference_context_item,
                    world_state_replay: &mut world_state_replay,
                    surviving_compaction_count: &mut surviving_compaction_count,
                    has_surviving_legacy_compaction_without_window_number:
                        &mut has_surviving_legacy_compaction_without_window_number,
                    window: &mut window,
                    pending_rollback_turns: &mut pending_rollback_turns,
                },
            );
        }

        let initial_window = if has_surviving_legacy_compaction_without_window_number {
            None
        } else {
            initial_window
        };

        let mut history = ContextManager::new();
        let mut saw_legacy_compaction_without_replacement_history = false;
        if let Some(base_replacement_history) = base_replacement_history {
            history.replace(base_replacement_history.to_vec());
        }
        // Materialize exact history semantics from the replay-derived suffix. The eventual lazy
        // design should keep this same replay shape, but drive it from a resumable reverse source
        // instead of an eagerly loaded `&[RolloutItem]`.
        let rollout_suffix_start = rollout_items.len().saturating_sub(rollout_suffix.len());
        for (offset, item) in rollout_suffix.iter().enumerate() {
            let index = rollout_suffix_start + offset;
            if discarded_history_effect_indexes.contains(&index) {
                continue;
            }
            match item {
                RolloutItem::ResponseItem(response_item) => {
                    history.record_items(std::iter::once(response_item), truncation_policy);
                }
                RolloutItem::InterAgentCommunication(communication) => {
                    let response_item = communication.to_model_input_item();
                    history.record_items(std::iter::once(&response_item), truncation_policy);
                }
                RolloutItem::InterAgentCommunicationMetadata { .. } => {}
                RolloutItem::Compacted(compacted) => {
                    if let Some(replacement_history) = &compacted.replacement_history {
                        // This should actually never happen, because the reverse loop above (to build rollout_suffix)
                        // should stop before any compaction that has Some replacement_history
                        history.replace(replacement_history.clone());
                    } else {
                        saw_legacy_compaction_without_replacement_history = true;
                        // Legacy rollouts without `replacement_history` should rebuild the
                        // historical TurnContext at the correct insertion point from persisted
                        // `TurnContextItem`s. These are rare enough that we currently just clear
                        // `reference_context_item`, reinject canonical context at the end of the
                        // resumed conversation, and accept the temporary out-of-distribution
                        // prompt shape.
                        // TODO(ccunningham): if we drop support for None replacement_history compaction items,
                        // we can get rid of this second loop entirely and just build `history` directly in the first loop.
                        let user_messages = compact::collect_user_messages(history.raw_items());
                        let rebuilt = compact::build_compacted_history(
                            Vec::new(),
                            &user_messages,
                            &compacted.message,
                        );
                        history.replace(rebuilt);
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    history.drop_last_n_user_turns(rollback.num_turns);
                }
                RolloutItem::EventMsg(_)
                | RolloutItem::TurnContext(_)
                | RolloutItem::WorldState(_)
                | RolloutItem::SamplingBoundary(_)
                | RolloutItem::ToolManifest(_)
                | RolloutItem::SessionMeta(_) => {}
            }
        }

        let reference_context_item = match reference_context_item {
            TurnReferenceContextItem::NeverSet | TurnReferenceContextItem::Cleared => None,
            TurnReferenceContextItem::Latest(turn_reference_context_item) => {
                Some(*turn_reference_context_item)
            }
        };
        let reference_context_item = if saw_legacy_compaction_without_replacement_history {
            None
        } else {
            reference_context_item
        };

        // Segments and their contents were collected newest-first; replay the surviving records
        // chronologically so compaction resets and merge patches have their original meaning.
        world_state_replay.reverse();
        let mut world_state_baseline: Option<WorldStateSnapshot> = None;
        for item in world_state_replay {
            match item {
                RolloutItem::Compacted(_) => world_state_baseline = None,
                RolloutItem::WorldState(world_state) if world_state.full => {
                    world_state_baseline = match serde_json::from_value(world_state.state.clone()) {
                        Ok(snapshot) => Some(snapshot),
                        Err(err) => {
                            tracing::warn!(%err, "failed to restore world-state snapshot");
                            None
                        }
                    };
                }
                RolloutItem::WorldState(world_state) => {
                    let Some(baseline) = world_state_baseline.as_mut() else {
                        tracing::warn!("ignored world-state patch without a full snapshot");
                        continue;
                    };
                    if let Err(err) = baseline.apply_merge_patch(&world_state.state) {
                        tracing::warn!(%err, "failed to apply world-state patch");
                        world_state_baseline = None;
                    }
                }
                RolloutItem::SessionMeta(_)
                | RolloutItem::ToolManifest(_)
                | RolloutItem::SamplingBoundary(_)
                | RolloutItem::ResponseItem(_)
                | RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. }
                | RolloutItem::TurnContext(_)
                | RolloutItem::EventMsg(_) => {
                    unreachable!("only world-state replay items are collected")
                }
            }
        }

        let window = window.or(initial_window).unwrap_or(ReconstructedWindow {
            number: surviving_compaction_count,
            first_id: None,
            previous_id: None,
            id: None,
        });
        RolloutReconstruction {
            history: history.into_raw_items(),
            plan,
            previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            window_number: window.number,
            first_window_id: window.first_id,
            previous_window_id: window.previous_id,
            window_id: window.id,
        }
    }
}

fn parse_uuid_v7(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value)
        .ok()
        .filter(|uuid| uuid.get_version_num() == 7)
}

fn reconstructed_window_from_session_context_window(
    context_window: &SessionContextWindow,
) -> Option<ReconstructedWindow> {
    let id = parse_uuid_v7(&context_window.window_id)?;
    Some(ReconstructedWindow {
        number: 0,
        first_id: Some(id),
        previous_id: None,
        id: Some(id),
    })
}
