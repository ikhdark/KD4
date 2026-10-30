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

/// Recovery is derived from the existing durable invocation/result stream, not
/// handler-return notifications. A crash after effects but before result append
/// therefore leaves an unresolved invocation visible on resume.
pub(super) fn append_unsettled_tool_recovery(
    history: &mut Vec<ResponseItem>,
    items: &[RolloutItem],
) {
    let page = unsettled_tool_recovery_page(items, 0);
    if page["unresolved_count"] == 0 {
        // A fork can contain only a replacement checkpoint. Absence of its
        // original invocations is not proof that the inherited warning settled.
        return;
    }
    let notice = ResponseItem::Message {
        id: Some(codex_protocol::ResponseItemId::new(
            "msg_sctx_unsettled_tools",
        )),
        role: "developer".into(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
        content: vec![ContentItem::InputText {
            text: format!(
                "<unsettled_tool_recovery>\n{page}\nHandler return is not durable result delivery. These retained invocations have no available durable result; they may or may not have executed. Inspect affected targets before retrying. Null environment means the resolved environment was not recorded. Arguments are deliberately redacted. If omitted_count is nonzero, use read_file with the recovery_path and omit environment_id; this reads retained history, never repeats an operation.\n</unsettled_tool_recovery>",
            ),
        }],
    };
    // Reopening unchanged durable source must not grow its warning. A fork
    // may omit invocations represented only by an inherited notice, however,
    // so a different snapshot cannot silently settle that older uncertainty.
    if history.iter().any(|item| {
        crate::stable_context::is_unsettled_tool_recovery_notice(item)
            && matches!((item, &notice),
                (ResponseItem::Message { content: old, .. }, ResponseItem::Message { content: new, .. })
                    if old == new)
    }) {
        return;
    }
    history.push(notice);
}

pub(super) fn unsettled_tool_recovery_page(
    items: &[RolloutItem],
    offset: usize,
) -> serde_json::Value {
    use sha2::Digest;
    use std::collections::BTreeMap;
    let mut pending = BTreeMap::new();
    let mut unmatched = std::collections::HashMap::new();
    let mut turn = "";
    let mut cwd = None;
    let bounded = |value: &str| value.chars().take(240).collect::<String>();
    let redacted_path = |value: &str| {
        if value.contains("://") {
            url::Url::parse(value)
                .ok()
                .map(|url| {
                    format!(
                        "{}://{}",
                        url.scheme(),
                        url.host_str().unwrap_or("[redacted]")
                    )
                })
                .unwrap_or_else(|| "[redacted URI]".into())
        } else {
            bounded(value)
        }
    };
    for (index, item) in items.iter().enumerate() {
        let response = match item {
            RolloutItem::TurnContext(context) => {
                turn = context.turn_id.as_deref().unwrap_or_default();
                cwd = Some(context.cwd.as_path());
                continue;
            }
            RolloutItem::ResponseItem(response) => response,
            _ => continue,
        };
        match response {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::CustomToolCall { call_id, .. } => {
                // Match borrowed invocation identities first. Settled calls
                // never pay argument decoding, hashing or redaction costs.
                let kind = matches!(response, ResponseItem::CustomToolCall { .. });
                pending.insert(index, (turn, call_id.as_str(), cwd, response));
                // Legacy raw IDs can recur within one turn. Only a unique
                // unmatched preceding invocation can own a later result.
                unmatched
                    .entry((turn, kind, call_id.as_str()))
                    .and_modify(|prior| *prior = None)
                    .or_insert(Some(index));
            }
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            }
            | ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                // Only the registry's complete delivery-loss placeholder is a
                // recovery signal; quoted source/log text is still a result.
                let unavailable = items.get(index + 1).is_some_and(|next|
                    matches!(next, RolloutItem::ResponseItem(item)
                        if crate::tools::registry::is_model_delivery_unavailable_receipt(item, call_id)))
                    // Conservative compatibility with legacy host placeholders.
                    || output.body.to_text().is_some_and(|text|
                    text == "Tool execution completed, but its full result could not be preserved for model delivery.");
                let kind = matches!(response, ResponseItem::CustomToolCallOutput { .. });
                if let Some(Some(source)) = unmatched.remove(&(turn, kind, call_id.as_str()))
                    && !unavailable
                {
                    pending.remove(&source);
                }
            }
            _ => {}
        }
    }
    let count = pending.len();
    let mut pending = pending.into_iter().collect::<Vec<_>>();
    pending.sort_unstable_by_key(|(index, _)| std::cmp::Reverse(*index));
    let operations = pending.into_iter().skip(offset).take(8).map(|(index, (turn, call_id, cwd, response))| {
        let (name, namespace, arguments) = match response {
            ResponseItem::FunctionCall { name, namespace, arguments, .. }
            | ResponseItem::CustomToolCall { name, namespace, input: arguments, .. } => (name, namespace, arguments),
            _ => unreachable!("only invocations enter pending"),
        };
        let args = (arguments.len() <= 16 * 1024)
            .then(|| serde_json::from_str::<serde_json::Value>(arguments).ok()).flatten()
            .unwrap_or(serde_json::Value::Null);
        let field = |keys: &[&str]| keys.iter().find_map(|key| args.get(*key).and_then(serde_json::Value::as_str));
        let cwd = cwd.map(|path| path.to_string_lossy()).unwrap_or_default();
        serde_json::json!({
            "turn_id": bounded(turn), "call_id": bounded(call_id), "rollout_item_index": index,
            "tool": bounded(&namespace.as_ref().map(|namespace| format!("{namespace}.{name}")).unwrap_or_else(|| name.clone())),
            "environment": field(&["environment_id"]).map(bounded),
            "cwd": redacted_path(field(&["workdir", "cwd"]).unwrap_or(&cwd)),
            "target": field(&["path", "file_path", "target"]).map(redacted_path),
            "invocation_sha256": hex::encode(sha2::Sha256::digest(arguments.as_bytes())),
            "status": "result_not_durably_available_effects_unknown",
        })
    }).collect::<Vec<_>>();
    let next = offset.saturating_add(operations.len());
    serde_json::json!({"operations": operations, "unresolved_count": count,
        "offset": offset, "omitted_count": count.saturating_sub(next),
        "recovery_path": (next < count).then(|| format!("context:unsettled-tools/{next}"))})
}

impl Session {
    /// Cold, explicitly requested recovery from the existing durable stream.
    /// No artifact writes or retained raw invocation copies are added to resume.
    pub(crate) async fn read_unsettled_tool_recovery(
        &self,
        offset: usize,
        active_turn: &str,
    ) -> Result<String, String> {
        let path = self
            .current_rollout_path()
            .await
            .map_err(|error| error.to_string())?
            .ok_or("unsettled recovery requires a durable session rollout")?;
        let (mut items, _, parse_errors) =
            codex_rollout::RolloutRecorder::load_rollout_items(&path)
                .await
                .map_err(|error| error.to_string())?;
        // Live calls have an owner and are not lost results. Bound all pages
        // at this turn's first context, including the recovery call itself.
        if let Some(start) = items.iter().position(|item| {
            matches!(item,
            RolloutItem::TurnContext(context) if context.turn_id.as_deref() == Some(active_turn))
        }) {
            items.truncate(start);
        }
        tokio::task::spawn_blocking(move || {
            let mut page = unsettled_tool_recovery_page(&items, offset);
            page["rollout_parse_errors"] = serde_json::json!(parse_errors);
            page["history_complete"] = serde_json::json!(parse_errors == 0);
            page.to_string()
        })
        .await
        .map_err(|error| error.to_string())
    }
}

/// Reserve handles from the entire reloaded rollout, including compacted and
/// rolled-back spans. Those handles may still be present in retained receipts.
pub(super) fn historical_process_ids(items: &[RolloutItem]) -> HashSet<u32> {
    fn text_ids(text: &str, ids: &mut HashSet<u32>) {
        for prefix in [
            UNIFIED_EXEC_SESSION_ID_PREFIX,
            "Running command session_id: ",
            "\"session_id\"",
            "\"polled_session_id\"",
            "\"process_id\"",
        ] {
            for (offset, _) in text.match_indices(prefix) {
                let mut suffix = text[offset + prefix.len()..].trim_start();
                if prefix.ends_with('"') {
                    let Some(value) = suffix.strip_prefix(':') else {
                        continue;
                    };
                    suffix = value.trim_start().trim_start_matches('"');
                }
                let digits = suffix.bytes().take_while(u8::is_ascii_digit).count();
                if let Ok(id) = suffix[..digits].parse() {
                    ids.insert(id);
                }
            }
        }
    }
    fn response_ids(item: &ResponseItem, ids: &mut HashSet<u32>) {
        if let Some(inventory) = crate::unified_exec::process_inventory::recorded_inventory(item) {
            for entry in inventory["entries"].as_array().into_iter().flatten() {
                if let Some(id) = entry["session_id"]
                    .as_u64()
                    .and_then(|id| u32::try_from(id).ok())
                {
                    ids.insert(id);
                }
            }
        }
        match item {
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(text) = output.body.to_text() {
                    text_ids(&text, ids);
                }
            }
            ResponseItem::FunctionCall { arguments, .. } => text_ids(arguments, ids),
            ResponseItem::CustomToolCall { input, .. } => text_ids(input, ids),
            _ => {}
        }
    }
    let mut ids = HashSet::new();
    for item in items {
        let id = match item {
            RolloutItem::EventMsg(EventMsg::ExecCommandBegin(event)) => event.process_id.as_deref(),
            RolloutItem::EventMsg(EventMsg::ExecCommandEnd(event)) => event.process_id.as_deref(),
            RolloutItem::EventMsg(EventMsg::TerminalInteraction(event)) => {
                Some(event.process_id.as_str())
            }
            RolloutItem::ResponseItem(item) => {
                response_ids(item, &mut ids);
                None
            }
            RolloutItem::Compacted(item) => {
                for item in item.replacement_history.iter().flatten() {
                    response_ids(item, &mut ids);
                }
                None
            }
            _ => None,
        };
        if let Some(id) = id.and_then(|id| id.parse().ok()) {
            ids.insert(id);
        }
    }
    ids
}

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
    pub(super) plan_lineage: Option<crate::plan_store::PlanLineage>,

    pub(super) previous_turn_settings: Option<PreviousTurnSettings>,
    pub(super) reference_context_item: Option<TurnContextItem>,
    pub(super) world_state_baseline: Option<WorldStateSnapshot>,
    pub(super) additional_context: crate::state::AdditionalContextStore,
    pub(super) repository_sources: Option<serde_json::Value>,
    pub(super) window_number: u64,
    pub(super) first_window_id: Option<Uuid>,
    pub(super) previous_window_id: Option<Uuid>,
    pub(super) window_id: Option<Uuid>,
}


pub(super) fn append_unified_exec_resume_invalidation_with_rollout(
    history: &mut Vec<ResponseItem>,
    rollout: &[RolloutItem],
) {
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
    let mut handles = ResumeHandles::new();
    // Inventory is an observation of the previous owner, never live authority
    // in this owner. Preserve its identities in the existing invalidation notice.
    for (position, item) in history.iter().enumerate() {
        if let Some(inventory) = crate::unified_exec::process_inventory::recorded_inventory(item) {
            for entry in inventory["entries"].as_array().into_iter().flatten() {
                if let Some(id) = entry["session_id"]
                    .as_u64()
                    .and_then(|id| u32::try_from(id).ok())
                {
                    let artifact = entry["raw_output_artifact_id"]
                        .as_str()
                        .filter(|id| id.len() <= 160)
                        .map(str::to_string);
                    record_resume_handle(&mut handles, id, artifact, (false, position));
                }
            }
        }
    }
    // Canonical lifecycle items are host-produced and persisted independently
    // of script text. Never infer nested handles/artifacts from printed JSON.
    for (position, item) in rollout.iter().enumerate() {
        let item = match item {
            RolloutItem::EventMsg(EventMsg::ItemStarted(event)) => &event.item,
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => &event.item,
            _ => continue,
        };
        let codex_protocol::items::TurnItem::CommandExecution(command) = item else {
            continue;
        };
        let Some(id) = command
            .process_id
            .as_deref()
            .and_then(|id| id.parse::<u32>().ok())
        else {
            continue;
        };
        let artifact = command
            .output_metadata
            .as_ref()
            .and_then(|metadata| metadata.raw_output_artifact_id.clone());
        record_resume_handle(&mut handles, id, artifact, (true, position));
    }
    let has_old_sessions = history
        .iter()
        .enumerate()
        .fold(false, |found, (position, item)| {
            let observed_at = (false, position);
            let current = match item {
                ResponseItem::FunctionCallOutput {
                    call_id, output, ..
                } if unified_exec_call_ids.contains(call_id.as_str()) => {
                    output.body.to_text().is_some_and(|output| {
                        collect_resume_handles(&output, false, observed_at, &mut handles)
                    })
                }
                ResponseItem::CustomToolCallOutput {
                    call_id, output, ..
                }
                | ResponseItem::FunctionCallOutput {
                    call_id, output, ..
                } if code_mode_call_ids.contains(call_id.as_str()) => {
                    output.body.to_text().is_some_and(|text| {
                        collect_resume_handles(&text, true, observed_at, &mut handles)
                    })
                }
                _ => false,
            };
            found || current
        });
    // Replace earlier runtime notices so this notice scopes invalidation to the
    // current resume boundary. Historical process IDs are excluded at admission.
    let mut had_notice = false;
    history.retain(|item| {
        let is_notice = is_unified_exec_resume_invalidation(item);
        had_notice |= is_notice;
        !is_notice && !crate::unified_exec::process_inventory::is_process_inventory(item)
    });
    if !has_old_sessions && !had_notice && handles.is_empty() {
        return;
    }
    // Process IDs are random, so ID order is not recency. Keep the most
    // recently observed handles and list them chronologically.
    let omitted = handles.len().saturating_sub(32);
    let mut recorded_handles = handles.into_iter().collect::<Vec<_>>();
    recorded_handles.sort_by_key(|(_, (_, observed_at))| std::cmp::Reverse(*observed_at));
    recorded_handles.truncate(32);
    recorded_handles.reverse();
    let recorded_handles = recorded_handles.into_iter().map(|(session_id, (artifact_id, _))| {
        serde_json::json!({"session_id": session_id, "raw_output_artifact_id": artifact_id})
    }).collect::<Vec<_>>();
    let evidence = serde_json::json!({"recorded_process_handles": recorded_handles,
        "omitted_handle_count": omitted});
    let text = format!(
        "{UNIFIED_EXEC_RESUME_INVALIDATION_START}\n\
Process session IDs recorded before this thread was resumed or forked are not live in this process owner. \
Do not poll those old sessions. \
Newly returned session IDs are valid and do not reuse recorded process IDs. \
This includes sessions in nested code-mode command receipts. Live cell execution from before \
that boundary cannot be resumed. Only durably retained terminal receipts may be recoverable through wait; \
do not restart work to recreate evidence. Invalidation does not establish \
whether a command completed or was terminated; retain its recorded output and recovery references, \
and re-establish uncertain effects before repeating it. \
Start another process only when the current task still requires execution; \
do not rerun completed commands merely because this thread was resumed or forked.\n\
Recorded process handles and saved output references (historical evidence, not a crash-time inventory):\n\
{evidence}\n\
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

/// Saved-output reference and latest observation of each recorded process
/// handle. Lifecycle records `(true, rollout position)` order after receipts
/// seen only in history `(false, history position)`; only rollouts predating
/// lifecycle process IDs lack those records.
type ResumeHandles = std::collections::BTreeMap<u32, (Option<String>, (bool, usize))>;

fn record_resume_handle(
    handles: &mut ResumeHandles,
    id: u32,
    artifact: Option<String>,
    observed_at: (bool, usize),
) {
    let retained = handles.entry(id).or_insert((None, observed_at));
    retained.1 = retained.1.max(observed_at);
    if artifact.is_some() {
        retained.0 = artifact;
    }
}

fn collect_command_receipt_handle(
    value: &serde_json::Value,
    observed_at: (bool, usize),
    handles: &mut ResumeHandles,
) -> bool {
    // Raw command receipts omit these fields. Explicitly different owners are
    // not process handles, even when their JSON happens to contain session_id.
    if value
        .get("tool")
        .is_some_and(|tool| !matches!(tool.as_str(), Some("exec_command" | "write_stdin")))
        || value
            .get("namespace")
            .is_some_and(|namespace| !namespace.is_null())
    {
        return false;
    }
    let valid_id = |id: &serde_json::Value| id.as_u64().and_then(|id| u32::try_from(id).ok());
    // A returned handle is live even after process exit while pipes drain.
    // A historical polled handle alone is live only without terminal exit.
    let id = value.get("session_id").and_then(valid_id).or_else(|| {
        (value["process_exited"].as_bool() == Some(false))
            .then(|| value.get("polled_session_id").and_then(valid_id))
            .flatten()
    });
    let Some(id) = id else {
        return false;
    };
    let artifact = value["raw_output_artifact_id"]
        .as_str()
        .filter(|id| id.len() <= 160)
        .map(str::to_string);
    record_resume_handle(handles, id, artifact, observed_at);
    true
}

fn collect_resume_handles(
    text: &str,
    nested: bool,
    observed_at: (bool, usize),
    handles: &mut ResumeHandles,
) -> bool {
    if nested {
        // Legacy code-mode text has no authenticated subrecord boundaries.
        // Invalidate conservatively without asserting printed IDs/artifacts.
        return true;
    }
    // Only the direct native envelope owns these fields; stdout is nested data.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        return collect_command_receipt_handle(&value, observed_at, handles);
    }
    // Compatibility with the native legacy status header, never stdout lines.
    let Some(status) = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix(UNIFIED_EXEC_SESSION_ID_PREFIX))
    else {
        return false;
    };
    let id = status.split_once(';').map_or(status, |(id, _)| id);
    if let Ok(id) = id.trim().parse::<u32>() {
        record_resume_handle(handles, id, None, observed_at);
        true
    } else {
        false
    }
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
    // Outer None: no accepted boundary. Some(None): legacy/unknown provenance.

    plan: Option<UpdatePlanArgs>,
    plan_lineage: Option<crate::plan_store::PlanLineage>,
    plan_revision: Option<String>,
    // A donor for the already-selected newer projection is held until this
    // segment survives rollback. It need not be this segment's newest plan.
    plan_lineage_donor: Option<crate::plan_store::PlanLineage>,
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

    plan_lineage: &'state mut Option<crate::plan_store::PlanLineage>,
    plan_revision: &'state mut Option<String>,
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

        plan_lineage,
        plan_revision,
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
    // A full snapshot after the latest compaction still establishes context when
    // a fork filters out the agent/mail input that originally started this turn.
    // It must not create an extra user-turn boundary for rollback.
    let has_context_baseline = active_segment.counts_as_user_turn
        || active_segment
            .world_state_replay
            .iter()
            .take_while(|item| !matches!(item, RolloutItem::Compacted(_)))
            .any(|item| matches!(item, RolloutItem::WorldState(state) if state.full));
    world_state_replay.extend(active_segment.world_state_replay);
    if plan.is_none() {
        *plan = active_segment.plan;
        *plan_lineage = active_segment.plan_lineage;
        *plan_revision = active_segment.plan_revision;
    } else if plan_lineage.is_none() {
        *plan_lineage = active_segment.plan_lineage_donor;
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
    // Restore settings from the newest surviving context baseline.
    if previous_turn_settings.is_none() && has_context_baseline {
        *previous_turn_settings = active_segment.previous_turn_settings;
    }

    // `reference_context_item` comes from the newest surviving context baseline, or
    // from a surviving compaction that explicitly cleared that baseline.
    if matches!(reference_context_item, TurnReferenceContextItem::NeverSet)
        && (has_context_baseline
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
        // A loader diagnostic describes gaps in the entire source, including missing
        // rollback markers. It must not acquire the lifecycle of whichever turn was
        // last in that source. A fresh load supersedes a checkpoint's older notice.
        let reconstruction_notice = rollout_items.iter().rev().find_map(|item| match item {
            RolloutItem::ResponseItem(item)
                if codex_rollout::is_rollout_reconstruction_notice(item) =>
            {
                Some(item)
            }
            RolloutItem::Compacted(compacted) => {
                compacted.replacement_history.as_ref().and_then(|items| {
                    items
                        .iter()
                        .rev()
                        .find(|item| codex_rollout::is_rollout_reconstruction_notice(item))
                })
            }
            _ => None,
        });
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


        let mut plan_lineage = None;
        let mut plan_revision = None;
        let update_plan_call_ids = rollout_items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::ResponseItem(ResponseItem::FunctionCall {
                    name,
                    namespace: None,
                    call_id,
                    ..
                }) if name == "update_plan" => Some(call_id.as_str()),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let has_plan_updates = rollout_items.iter().any(|item| {
            matches!(item, RolloutItem::EventMsg(EventMsg::PlanUpdate(_)))
                || matches!(item, RolloutItem::ResponseItem(response)
                    if crate::plan_store::plan_snapshot_from_item(response).is_some())
        }) || !update_plan_call_ids.is_empty();
        let mut previous_turn_settings = None;
        let mut reference_context_item = TurnReferenceContextItem::NeverSet;
        let mut world_state_replay = Vec::new();
        let mut surviving_compaction_count = 0u64;
        let mut has_surviving_legacy_compaction_without_window_number = false;
        let mut window = None;
        // A resumed legacy rollout can acquire modern turns. Only user events
        // outside a started turn are legacy boundaries; steering inside a
        // modern turn must still roll back with that turn, not independently.
        // Modern starts own membership. Terminal publication can lag a successor;
        // those terminal facts must not seed an unrelated reverse segment.
        let first_started_turn = rollout_items
            .iter()
            .position(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))));
        let mut started_turn = None;
        let mut legacy_user_boundaries = HashSet::new();

        let mut in_review = false;
        let mut saw_review_entry = false;
        let mut review_marker_gap = false;
        let mut review_plan_events = HashSet::new();
        // Rollback markers recorded before typed inter-agent delivery counted
        // assistant-encoded mail as an instruction turn; replay them that way.
        let mut legacy_inter_agent_mail =
            codex_context_fragments::LegacyInterAgentMailReader::default();
        let mut legacy_inter_agent_mail_indexes = HashSet::new();
        for (index, item) in rollout_items.iter().enumerate() {
            if legacy_inter_agent_mail.observe(item).is_some() {
                legacy_inter_agent_mail_indexes.insert(index);
            }

            // Review delegates historically forwarded ownerless checklist
            // events. Only retained review markers establish foreign ownership;
            // ordinary legacy PlanUpdate events remain supported.
            match item {
                RolloutItem::EventMsg(EventMsg::EnteredReviewMode(_))
                | RolloutItem::EventMsg(EventMsg::ItemCompleted(
                    codex_protocol::protocol::ItemCompletedEvent {
                        item: codex_protocol::items::TurnItem::EnteredReviewMode(_),
                        ..
                    },
                )) => {
                    // Legacy and canonical events can both encode this entry.
                    in_review = true;
                    saw_review_entry = true;
                }
                RolloutItem::EventMsg(EventMsg::ExitedReviewMode(_))
                | RolloutItem::EventMsg(EventMsg::ItemCompleted(
                    codex_protocol::protocol::ItemCompletedEvent {
                        item: codex_protocol::items::TurnItem::ExitedReviewMode(_),
                        ..
                    },
                )) => {
                    review_marker_gap |= !saw_review_entry;
                    in_review = false;
                }
                RolloutItem::EventMsg(EventMsg::TurnStarted(_)) => {
                    review_marker_gap |= in_review;
                    in_review = false;
                    saw_review_entry = false;
                }
                RolloutItem::EventMsg(EventMsg::PlanUpdate(_)) if in_review => {
                    review_plan_events.insert(index);
                }
                _ => {}
            }
            match item {
                RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                    started_turn = Some(event.turn_id.as_str());
                }
                RolloutItem::EventMsg(
                    EventMsg::TurnComplete(event) | EventMsg::ImportedTurnClosed(event),
                ) if started_turn == Some(event.turn_id.as_str()) => {
                    started_turn = None;
                }
                RolloutItem::EventMsg(EventMsg::TurnAborted(event))
                    if event
                        .turn_id
                        .as_deref()
                        .is_none_or(|id| started_turn == Some(id)) =>
                {
                    started_turn = None;
                }
                RolloutItem::EventMsg(EventMsg::UserMessage(_)) if started_turn.is_none() => {
                    legacy_user_boundaries.insert(index);
                }
                _ => {}
            }
        }
        review_marker_gap |= in_review;
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
                    if review_plan_events.contains(&index) {
                        continue;
                    }
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
                RolloutItem::EventMsg(
                    EventMsg::TurnComplete(event) | EventMsg::ImportedTurnClosed(event),
                ) => {
                    if first_started_turn.is_some_and(|start| index >= start) {
                        continue;
                    }
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    // Reverse replay often sees `TurnComplete` before any turn-scoped metadata.
                    // Capture the turn id early so later `TurnContext` / abort items can match it.
                    if active_segment.turn_id.is_none() {
                        active_segment.turn_id = Some(event.turn_id.clone());
                    }
                }
                RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                    if first_started_turn.is_some_and(|start| index >= start) {
                        continue;
                    }
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
                RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                    if matches!(&event.item, codex_protocol::items::TurnItem::UserMessage(message)
                        if crate::event_mapping::is_user_input_item_id(&message.id)) =>
                {
                    let segment = active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    segment.counts_as_user_turn = true;
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

                                plan_lineage: &mut plan_lineage,
                                plan_revision: &mut plan_revision,
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
                    active_segment.counts_as_user_turn |= is_user_turn_boundary(response_item)
                        || legacy_inter_agent_mail_indexes.contains(&index);

                    if let Some(snapshot) =
                        crate::plan_store::plan_snapshot_from_item(response_item)
                    {
                        if plan_lineage.is_none()
                            && active_segment.plan_lineage_donor.is_none()
                            && let Some(selected) = &plan
                            && let Some(revision) = plan_revision.as_deref()
                        {
                            active_segment.plan_lineage_donor =
                                snapshot.full_lineage_for(selected, Some(revision));
                        }
                        if active_segment.plan_lineage.is_none() {
                            let selected = active_segment
                                .plan
                                .as_ref()
                                .unwrap_or(&snapshot.current_plan);
                            if let Some(lineage) = snapshot
                                .full_lineage_for(selected, active_segment.plan_revision.as_deref())
                            {
                                active_segment.plan = Some(snapshot.current_plan);
                                active_segment.plan_lineage = Some(lineage);
                                active_segment.plan_revision =
                                    (!snapshot.revision.is_empty()).then_some(snapshot.revision);
                            }
                        }
                    }
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
                        && let Some(response) =
                            crate::plan_store::plan_response_from_tool_output(output)
                    {
                        active_segment.plan = Some(response.current_plan);
                        active_segment.plan_revision =
                            (!response.revision.is_empty()).then_some(response.revision);
                        // Active projections must not eclipse the full
                        // durable snapshot earlier in this same turn.
                        active_segment.plan_lineage =
                            response.lineage_complete.then_some(response.lineage);
                    }
                }
                RolloutItem::AdditionalContextState { .. } => {
                    active_segment
                        .get_or_insert_with(ActiveReplaySegment::default)
                        .history_effect_indexes
                        .push(index);
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
            if legacy_user_boundaries.contains(&index)
                && let Some(active_segment) = active_segment.take()
            {
                finalize_active_segment(
                    active_segment,
                    FinalizedReplayState {
                        plan: &mut plan,

                        plan_lineage: &mut plan_lineage,
                        plan_revision: &mut plan_revision,
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
                && ((plan.is_some() && (plan_revision.is_none() || plan_lineage.is_some()))
                    || !has_plan_updates)
            {
                break;
            }
        }

        if let Some(active_segment) = active_segment.take() {
            finalize_active_segment(
                active_segment,
                FinalizedReplayState {
                    plan: &mut plan,

                    plan_lineage: &mut plan_lineage,
                    plan_revision: &mut plan_revision,
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
        let mut additional_context_state = None;
        let mut repository_sources = None;
        for (offset, item) in rollout_suffix.iter().enumerate() {
            let index = rollout_suffix_start + offset;
            if discarded_history_effect_indexes.contains(&index) {
                continue;
            }
            match item {
                RolloutItem::AdditionalContextState {
                    version,
                    state,
                    repository_sources: sources,
                    ..
                } => {
                    additional_context_state = (*version == 1).then_some(state);
                    if *version == 1
                        && let Some(sources) = sources
                    {
                        repository_sources = Some(sources.clone());
                    }
                }
                RolloutItem::ResponseItem(response_item) => {
                    // Durable plan state is replay metadata, not another model
                    // message or a second tool result.
                    if crate::plan_store::plan_snapshot_from_item(response_item).is_none()
                        && !super::user_decision::AcceptedUserDecision::is_snapshot(response_item)
                    {
                        history.record_items(std::iter::once(response_item), truncation_policy);
                    }
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
        // These are already rollback-filtered and newest-first. A full
        // snapshot or compaction resets the baseline; older patches/snapshots
        // cannot contribute and need not be decoded and merged only to discard.
        if let Some(reset) = world_state_replay.iter().position(|item| {
            matches!(item, RolloutItem::Compacted(_))
                || matches!(item, RolloutItem::WorldState(state) if state.full)
        }) {
            world_state_replay.truncate(reset + 1);
        }
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
                | RolloutItem::AdditionalContextState { .. }
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
        let mut history = history.into_raw_items();
        history.retain(|item| !codex_rollout::is_rollout_reconstruction_notice(item));
        if let Some(notice) = reconstruction_notice {
            history.push(notice.clone());
        }
        if (plan_revision.is_some() && plan_lineage.is_none()) || review_marker_gap {
            history.push(ResponseItem::Message {
                id: Some(codex_protocol::ResponseItemId::new("msg_plan_reconstruction_gap")),
                role: "developer".into(),
                content: vec![codex_protocol::models::ContentItem::InputText {
                    text: "<plan_reconstruction_gap>Original plan scope could not be fully reconstructed: a modern projection has no matching surviving full lineage, or review ownership markers are incomplete. The restored checklist is not a complete original-requirement ledger. Reconcile affected scope with the user before relying on checklist completion.</plan_reconstruction_gap>".into(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            });
        }
        let additional_context = additional_context_state
            .and_then(|state| crate::state::AdditionalContextStore::restore(state, &history))
            .unwrap_or_else(|| crate::state::AdditionalContextStore::from_legacy_history(&history));
        RolloutReconstruction {
            history,
            plan,
            plan_lineage,

            previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            additional_context,
            repository_sources,
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
