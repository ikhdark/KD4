mod history;

use history::build_bounded_input_history;
use history::can_reuse_previous_summary;
use history::compaction_image_omission_marker;
use history::compaction_output_call_id;
use history::compaction_summary_item_with_artifact_pins;
use history::compaction_summary_items;
use history::is_compaction_model_generated_item;
use history::latest_summary_message;

pub use history::content_items_to_text;
pub(crate) use history::bind_omission_receipts_to_recovery;
pub(crate) use history::build_compacted_history;
pub(crate) use history::build_task_input_checkpoint_with_details;
pub(crate) use history::collect_user_messages;
pub(crate) use history::compaction_context_message;
pub(crate) use history::has_authoritative_task_state_context;
pub(crate) use history::insert_compaction_initial_context;
pub(crate) use history::is_compaction_summary_item;
pub(crate) use history::new_omission_receipt_ids;
pub(crate) use history::strip_compaction_startup_envelopes;
pub(crate) use history::task_compaction_items;

use std::sync::Arc;
use std::time::Instant;

use crate::Prompt;
use crate::client::ModelClientSession;
use crate::client_common::ResponseEvent;
use crate::context::world_state::WorldState;
use crate::context::world_state::WorldStateSnapshot;
use crate::hook_runtime::run_post_compact_hook_gate;
use crate::hook_runtime::run_pre_compact_hook_gate;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::ResponsesStreamRetryState;
use crate::responses_retry::handle_retryable_response_stream_error;
use crate::session::session::Session;
use crate::session::turn::get_last_assistant_message_from_turn;
use crate::session::turn_context::TurnContext;
use crate::tools::command_output_artifact::CanonicalOutputArtifact;
use crate::tools::command_output_artifact::create_canonical_output_artifact;
use codex_analytics::CodexCompactionEvent;
use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionStatus;
use codex_analytics::CompactionStrategy;
use codex_analytics::CompactionTrigger;
use codex_analytics::now_unix_seconds;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::CompactionMode;
use codex_protocol::protocol::CompactionOperation;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::user_input::UserInput;
use codex_tools::CanonicalToolResult;
use codex_utils_image::MAX_PROMPT_IMAGE_SOURCE_BYTES;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use codex_utils_string::TokenCountEstimate;
use futures::prelude::*;
use tokio_util::sync::CancellationToken;

use codex_model_provider_info::ModelProviderInfo;

pub use codex_prompts::COMPACTION_BASE_INSTRUCTIONS;
pub use codex_prompts::INCREMENTAL_SUMMARIZATION_PROMPT;
pub use codex_prompts::SUMMARIZATION_PROMPT;
pub use codex_prompts::SUMMARY_PREFIX;
const COMPACT_TASK_STATE_MAX_TOKENS: usize = 2_400;
const COMPACT_UNSTRUCTURED_UPDATE_MAX_TOKENS: usize = 1_000;
// Keep introductory prose subordinate to the structured task-state sections.
const COMPACT_PREAMBLE_MAX_TOKENS: usize = 300;
const GOAL_HEADING: &str = "## Goal";
const CURRENT_STATE_HEADING: &str = "## Current state";
const COMPLETED_WORK_HEADING: &str = "## Completed work";
const UNRESOLVED_WORK_HEADING: &str = "## Unresolved work";
const EVIDENCE_HEADING: &str = "## Evidence";
const NEXT_ACTION_HEADING: &str = "## Next action";
const COMPACTION_SECTIONS: [(&str, usize); 6] = [
    (GOAL_HEADING, 250),
    (CURRENT_STATE_HEADING, 350),
    (COMPLETED_WORK_HEADING, 250),
    (UNRESOLVED_WORK_HEADING, 350),
    (EVIDENCE_HEADING, 500),
    (NEXT_ACTION_HEADING, 250),
];

fn local_compaction_output_schema(
    openai: bool, custom_prompt: bool, rebase: bool,
) -> Option<serde_json::Value> {
    // The typed OpenAI Responses provider supports the same strict output
    // contract as ordinary final schemas. Unknown/custom providers, custom
    // prompts and incremental rebases retain their existing text protocol.
    if !openai || custom_prompt || rebase {
        return None;
    }
    let names = COMPACTION_SECTIONS.iter().map(|(heading, _)| heading.trim_start_matches("## ")).collect::<Vec<_>>();
    let properties = names.iter().map(|name| ((*name).to_string(), serde_json::json!({
        "type": "string", "description": "Complete nonempty section body, preserving constraints, unresolved work, evidence and uncertainty; use an explicit none statement where applicable.",
    }))).collect::<serde_json::Map<_, _>>();
    Some(serde_json::json!({
        "type": "object", "properties": properties,
        "required": names, "additionalProperties": false,
    }))
}

fn render_structured_compaction_summary(raw: &str) -> CodexResult<String> {
    // Compatibility with already complete text handoffs is not proof that a
    // provider enforced the requested schema. The ordinary semantic validator
    // still checks these below, as it did before the schema was requested.
    if !raw.trim_start().starts_with('{') && has_compaction_section(raw) {
        return Ok(raw.to_string());
    }
    let value = serde_json::from_str::<serde_json::Value>(raw)
        .map_err(|error| CodexErr::Fatal(format!("invalid structured compaction handoff: {error}")))?;
    let fields = value.as_object().filter(|fields| fields.len() == COMPACTION_SECTIONS.len())
        .ok_or_else(|| CodexErr::Fatal("structured compaction requires exactly the six checkpoint sections".into()))?;
    let mut sections = Vec::with_capacity(COMPACTION_SECTIONS.len());
    for (heading, _) in COMPACTION_SECTIONS {
        let name = heading.trim_start_matches("## ");
        let body = fields.get(name).and_then(serde_json::Value::as_str)
            .filter(|body| !body.trim().is_empty())
            .ok_or_else(|| CodexErr::Fatal(format!("structured compaction section {name} is missing or empty")))?;
        sections.push(format!("{heading}\n{}", body.trim()));
    }
    Ok(sections.join("\n\n"))
}

pub(crate) const MAX_RETAINED_USER_IMAGES: usize = 8;
pub(crate) const MAX_RETAINED_USER_IMAGE_BYTES: usize =
    MAX_PROMPT_IMAGE_SOURCE_BYTES / 3 * 4 + 4096;
pub(crate) const COMPACT_IMAGE_OMISSION_MARKER: &str =
    "[codex-local-compaction omitted user images: limits exceeded]";
/// Controls whether compaction replacement history must include initial context.
///
/// Pre-turn/manual compaction variants use `AtStart` so the next request retains the same
/// cacheable initial-context prefix instead of appending a fresh copy after the summary.
#[derive(Clone, Debug)]
pub(crate) enum InitialContextInjection {
    AtStart(Arc<WorldState>),
}

pub(crate) async fn build_compaction_initial_context(
    sess: &Session,
    turn_context: &TurnContext,
    initial_context_injection: &InitialContextInjection,
) -> (
    Vec<ResponseItem>,
    Option<WorldStateSnapshot>,
    Vec<codex_protocol::protocol::ContextFragmentDigest>,
) {
    // Return the rendered state with its items so history and its baseline stay identical.
    match initial_context_injection {
        InitialContextInjection::AtStart(world_state) => {
            let (mut items, delivered_snapshot, fragment_digests) = sess
                .build_initial_context_with_world_state_and_provenance(
                    turn_context,
                    world_state.as_ref(),
                )
                .await;
            if let Some(inventory) = sess
                .services
                .unified_exec_manager
                .compaction_process_inventory()
                .await
            {
                items.push(inventory);
            }
            (items, Some(delivered_snapshot), fragment_digests)
        }
    }
}

pub(crate) fn should_use_remote_compact_task(
    provider: &ModelProviderInfo,
    compact_prompt: Option<&str>,
) -> bool {
    provider.supports_remote_compaction() && compact_prompt.is_none()
}

fn incremental_summarization_prompt(compact_prompt: Option<&str>) -> Option<&'static str> {
    // Both entrypoints already supply the custom prompt as the initial input.
    // Only the default prompt needs additional incremental-summary guidance.
    compact_prompt
        .is_none()
        .then_some(INCREMENTAL_SUMMARIZATION_PROMPT)
}

fn select_compaction_input(
    input: &mut [UserInput],
    previous: Option<&str>,
    compact_prompt: Option<&str>,
    rebase_sections: &[usize],
) {
    if let Some(prompt) = incremental_summarization_prompt(compact_prompt)
        && let [UserInput::Text { text, .. }] = input
        && text == SUMMARIZATION_PROMPT
    {
        // Select one default contract without changing caller-authored input.
        *text = match previous {
            Some(previous) => format!(
                "{prompt}{}{}",
                compaction_rebase_prompt(rebase_sections),
                compaction_update_budget_prompt(previous, rebase_sections)
            ),
            // The prefix is installed by the harness, not generated by the model.
            None => {
                let remaining = COMPACT_TASK_STATE_MAX_TOKENS
                    .saturating_sub(approx_token_count(SUMMARY_PREFIX) + 1);
                format!(
                    "{SUMMARIZATION_PROMPT}\nFirst-request output budget: at most {remaining} tokens for this response, including headings. The harness adds the checkpoint prefix separately; the 2,400-token target includes that prefix. Preserve obligations and qualifications."
                )
            }
        };
    }
}

fn compaction_rebase_sections(previous: &str) -> Vec<usize> {
    // Several individually small sections can exhaust the aggregate budget.
    // Reserve room for an ordinary update in the first request, not a retry.
    if approx_token_count(previous).saturating_add(COMPACT_UNSTRUCTURED_UPDATE_MAX_TOKENS)
        > COMPACT_TASK_STATE_MAX_TOKENS
    {
        return (0..COMPACTION_SECTIONS.len()).collect();
    }
    let mut bodies = vec![String::new(); COMPACTION_SECTIONS.len()];
    let mut current = None;
    for (line, section) in checkpoint_lines(previous) {
        if let Some(index) = section {
            current = Some(index);
        } else if let Some(index) = current {
            bodies[index].push_str(line);
            bodies[index].push('\n');
        }
    }
    COMPACTION_SECTIONS
        .iter()
        .enumerate()
        .filter_map(|(index, (heading, budget))| {
            (*heading == NEXT_ACTION_HEADING || approx_token_count(&bodies[index]) >= *budget)
                .then_some(index)
        })
        .collect()
}

fn compaction_rebase_prompt(sections: &[usize]) -> String {
    if sections.is_empty() {
        return String::new();
    }
    format!(
        "\nRebase these sections instead of appending deltas: {}. Return each as a complete refreshed state, retaining unresolved obligations, constraints, live handles, and indispensable evidence from ALL earlier updates. Keep each previous Goal and Unresolved work line verbatim in its section. Only after the existing update_plan requirement owner records completion or authorized supersession may you account for it on one Completed work line: `Resolved: <exact line> Evidence: <receipt>` or `Superseded: <exact line> User instruction: <accepted instruction>`. Preserve existing requirement IDs as `[ID] <original requirement text>`; do not invent a second ledger. Summary prose never performs a transition. Replace the previous bodies. Keep the combined checkpoint within 2,400 tokens; do not omit obligations to fit.",
        sections
            .iter()
            .map(|&index| COMPACTION_SECTIONS[index].0)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn retained_compaction_sections(previous: &str, rebased: &[usize]) -> String {
    let mut current = None;
    checkpoint_lines(previous)
        .filter_map(|(line, section)| {
            if let Some(index) = section {
                current = Some(index);
            }
            (!current.is_some_and(|index| rebased.contains(&index))).then_some(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn compaction_update_budget_prompt(previous: &str, rebased: &[usize]) -> String {
    let retained = retained_compaction_sections(previous, rebased);
    let retained_tokens = approx_token_count(retained.trim());
    let remaining = COMPACT_TASK_STATE_MAX_TOKENS
        .saturating_sub(retained_tokens.saturating_add(approx_token_count("\n\n")));
    format!(
        "\nFirst-request output budget: at most {remaining} tokens for this response, including headings and refreshed sections. The runtime retains approximately {retained_tokens} tokens of the earlier checkpoint. The 2,400-token target applies to the combined checkpoint, NOT to this update alone. Preserve obligations and qualifications; do not fill the allowance with unchanged prose."
    )
}

// Accounting prose alone cannot retire scope. Reuse the existing requirement
// owner instead of deriving another obligation ledger from summary text.
fn compaction_anchor_accounted_for(
    completed: &str,
    anchor: &str,
    lineage: Option<&crate::plan_store::PlanLineage>,
) -> bool {
    completed.lines().any(|line| {
        let line = line.trim().trim_start_matches("- ");
        [
            ("Resolved:", "Evidence:"),
            ("Superseded:", "User instruction:"),
        ]
        .iter()
        .any(|(prefix, marker)| {
            line.strip_prefix(prefix).is_some_and(|body| {
                body.split_once(marker).is_some_and(|(claim, evidence)| {
                    claim.trim() == anchor && !evidence.trim().is_empty()
                        && lineage.is_some_and(|lineage| {
                            lineage.compaction_anchor_closed(anchor, *prefix == "Superseded:")
                        })
                })
            })
        })
    })
}

/// `bound_overflow` is set only for the corrective attempt, after a complete
/// rebase was already requested. Conserved anchors can still exceed the fixed
/// budget (accounting for a resolved anchor costs more than keeping it), and
/// failing again would fail every later compaction on the same anchors. Bound
/// that checkpoint as a first checkpoint is bounded instead: marked incomplete,
/// with both exact handoffs kept by generated_summary_recovery_canonical.
fn rebased_compaction_summary(
    previous: &str,
    suffix: &str,
    rebased: &[usize],
    bound_overflow: bool,
    lineage: Option<&crate::plan_store::PlanLineage>,
) -> CodexResult<String> {
    validate_generated_compaction_summary(Some(previous), suffix)?;
    let populated = compaction_section_bodies(suffix);
    if rebased.iter().any(|&index| !populated[index]) {
        return Err(CodexErr::Fatal(
            "compaction omitted a requested refreshed section".into(),
        ));
    }
    // Text quoted in Next action is not conservation of a Goal/Unresolved
    // anchor. Retain missing anchors without dropping the new state or making
    // a second generation. Next-action-only updates do not need this scan.
    // Completed-work accounting remains a model claim, never authorization.
    let mut repairs = [Vec::new(), Vec::new()];
    if rebased.contains(&0) || rebased.contains(&3) {
        let mut protected = [String::new(), String::new()];
        let mut completed = String::new();
        let mut current = None;
        for (line, section) in checkpoint_lines(suffix) {
            if let Some(index) = section {
                current = Some(index);
            } else {
                let target = match current {
                    Some(0) => Some(&mut protected[0]),
                    Some(3) => Some(&mut protected[1]),
                    Some(2) => Some(&mut completed),
                    _ => None,
                };
                if let Some(target) = target {
                    target.push_str(line);
                    target.push('\n');
                }
            }
        }
        current = None;
        for (line, section) in checkpoint_lines(previous) {
            if let Some(index) = section {
                current = Some(index);
            } else if let Some(index @ (0 | 3)) = current
                && rebased.contains(&index)
                && !line.trim().is_empty()
                // A runtime marker on a bounded checkpoint is not an obligation.
                && line.trim() != INCOMPLETE_CHECKPOINT_EXCERPT
                && !protected[usize::from(index == 3)]
                    .lines()
                    .any(|retained| retained.trim() == line.trim())
                && !compaction_anchor_accounted_for(&completed, line.trim(), lineage)
            {
                repairs[usize::from(index == 3)].push(line);
            }
        }
    }
    let retained = retained_compaction_sections(previous, rebased);
    let mut combined = format!("{}\n\n{}", retained.trim(), suffix.trim());
    for (index, repair) in [0, 3].into_iter().zip(repairs) {
        if !repair.is_empty() {
            combined.push_str("\n\n");
            combined.push_str(COMPACTION_SECTIONS[index].0);
            combined.push('\n');
            combined.push_str(&repair.join("\n"));
        }
    }
    // Never evict an old delta merely because unrelated updates used the
    // remaining budget. The existing corrective attempt can request a rebase.
    if !bound_overflow && approx_token_count(&combined) > COMPACT_TASK_STATE_MAX_TOKENS {
        return Err(CodexErr::Fatal("incremental checkpoint needs a complete rebase to fit; no prior obligations were discarded".into()));
    }
    // The fresh update was validated above, markers included. The retained
    // remainder and anchor repairs come from the previous checkpoint and may
    // carry its runtime incomplete-excerpt markers; check structure only.
    validate_compaction_sections_populated(&combined)?;
    Ok(truncate_compaction_summary(
        &combined,
        COMPACT_TASK_STATE_MAX_TOKENS,
    ))
}

pub(crate) struct InlineAutoCompactReuse<'a> {
    pub(crate) client_session: &'a mut ModelClientSession,
    pub(crate) prefetched_workspace_identity:
        Option<&'a Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "Preserve explicit compaction context, publication options, and cancellation"
)]
pub(crate) async fn run_inline_auto_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    reuse: InlineAutoCompactReuse<'_>,
    initial_context_injection: InitialContextInjection,
    reason: CompactionReason,
    phase: CompactionPhase,
    emit_error_event: bool,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let InlineAutoCompactReuse {
        client_session,
        prefetched_workspace_identity,
    } = reuse;
    let prompt = turn_context
        .config
        .compact_prompt
        .as_deref()
        .unwrap_or(SUMMARIZATION_PROMPT)
        .to_string();
    let input = vec![UserInput::Text {
        text: prompt,
        // Compaction prompt is synthesized; no UI element ranges to preserve.
        text_elements: Vec::new(),
    }];

    run_compact_task_inner(
        sess,
        turn_context,
        Some(client_session),
        prefetched_workspace_identity,
        input,
        initial_context_injection,
        CompactionTrigger::Auto,
        reason,
        phase,
        emit_error_event,
        cancellation_token,
    )
    .await?;
    Ok(())
}

pub(crate) async fn run_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_context.sub_id.clone(),
        trace_id: turn_context.trace_id.clone(),
        started_at: turn_context.turn_timing_state.started_at_unix_secs().await,
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(&turn_context, start_event).await;
    let step_context = sess.capture_step_context(Arc::clone(&turn_context)).await?;
    let world_state = Arc::new(sess.build_world_state_for_step(step_context.as_ref()).await);
    run_compact_task_with_initial_context(
        sess,
        turn_context,
        input,
        InitialContextInjection::AtStart(world_state),
        cancellation_token,
    )
    .await
}

/// Continue a standalone compaction turn, including a failed remote attempt,
/// without emitting another TurnStarted or recapturing its initial context.
pub(crate) async fn run_compact_task_with_initial_context(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    initial_context_injection: InitialContextInjection,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    run_compact_task_inner(
        sess,
        turn_context,
        // A standalone compaction turn has no in-flight sampling session to inherit.
        /*client_session*/
        None,
        /*prefetched_workspace_identity*/ None,
        input,
        initial_context_injection,
        CompactionTrigger::Manual,
        CompactionReason::UserRequested,
        CompactionPhase::StandaloneTurn,
        /*emit_error_event*/ true,
        cancellation_token,
    )
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_compact_task_inner(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    client_session: Option<&mut ModelClientSession>,
    prefetched_workspace_identity: Option<&Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
    input: Vec<UserInput>,
    initial_context_injection: InitialContextInjection,
    trigger: CompactionTrigger,
    reason: CompactionReason,
    phase: CompactionPhase,
    emit_error_event: bool,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let compaction_metadata =
        CompactionTurnMetadata::new(trigger, reason, CompactionImplementation::Responses, phase);
    let attempt = CompactionAnalyticsAttempt::begin(
        sess.as_ref(),
        turn_context.as_ref(),
        trigger,
        reason,
        CompactionImplementation::Responses,
        phase,
    )
    .await;
    if run_pre_compact_hook_gate(&sess, &turn_context, trigger).await {
        let error = CodexErr::TurnAborted;
        attempt
            .track(
                sess.as_ref(),
                CompactionStatus::Interrupted,
                Some(&error),
                CompactionAnalyticsDetails::default(),
            )
            .await;
        return Err(error);
    }
    let mut analytics_details = CompactionAnalyticsDetails::default();
    let mut compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(&turn_context, &compaction_item)
        .await;
    // Keep the large compaction state machine off every enclosing turn and
    // thread-spawn future's stack, including forked child startup.
    let result = Box::pin(run_compact_task_inner_impl(
        Arc::clone(&sess),
        Arc::clone(&turn_context),
        client_session,
        prefetched_workspace_identity,
        input,
        initial_context_injection,
        compaction_metadata,
        &mut analytics_details,
        &mut compaction_item,
        emit_error_event,
        cancellation_token,
    ))
    .await;
    // The inner operation can fail before or after sampling/persistence. Close
    // the item at its owner boundary, including interruption and every `?` exit.
    sess.emit_turn_item_completed_with_legacy_events(
        &turn_context,
        compaction_item,
        result.is_ok(),
    )
    .await;
    if result.is_ok() {
        let warning = EventMsg::Warning(WarningEvent {
            message: "Heads up: Long threads and multiple compactions can cause the model to be less accurate. Start a new thread when possible to keep threads small and targeted.".to_string(),
        });
        sess.send_event(&turn_context, warning).await;
    }
    let status = compaction_status_from_result(&result);
    let codex_error = result.as_ref().err();
    if let Ok(summary) = &result
        && run_post_compact_hook_gate(&sess, &turn_context, trigger, Some(summary)).await
    {
        attempt
            .track(sess.as_ref(), status, codex_error, analytics_details)
            .await;
        return Err(CodexErr::TurnAborted);
    }
    attempt
        .track(sess.as_ref(), status, codex_error, analytics_details)
        .await;
    result.map(|_| ())
}

#[allow(clippy::too_many_arguments)]
async fn run_compact_task_inner_impl(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    client_session: Option<&mut ModelClientSession>,
    prefetched_workspace_identity: Option<&Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
    mut input: Vec<UserInput>,
    initial_context_injection: InitialContextInjection,
    compaction_metadata: CompactionTurnMetadata,
    analytics_details: &mut CompactionAnalyticsDetails,
    compaction_item: &mut TurnItem,
    emit_error_event: bool,
    cancellation_token: &CancellationToken,
) -> CodexResult<String> {
    // Read before history is replaced so the completed item can report the
    // before/after pair rather than only the post-compaction size.
    let tokens_before = sess.get_total_token_usage().await;
    let mut history = sess.clone_history().await;
    let task_items = task_compaction_items(history.raw_items());
    let (
        mut retained_history,
        retained_image_count,
        omitted_image_count,
        omitted_user_text,
        omitted_text,
    ) = build_bounded_input_history(&task_items, false);
    bound_compaction_tool_outputs(sess.as_ref(), &mut retained_history).await?;
    let omission_receipt_ids = new_omission_receipt_ids(&retained_history, history.raw_items());
    analytics_details.retained_image_count = Some(retained_image_count);
    let summary_items = compaction_summary_items(history.raw_items());
    // A structurally valid summary cannot certify conservation of arbitrary
    // free-text uncertainty. Reuse the existing recovery artifact, retaining
    // the consumed source as well as bounded task-input omissions. This also
    // covers custom prompts; no classifier or extra model request is needed.
    let previous_summary = latest_summary_message(history.raw_items()).map(str::to_string);
    let reuse_previous_summary = previous_summary.is_some()
        && can_reuse_previous_summary(history.raw_items(), omitted_user_text);
    let mut compaction_mode = if reuse_previous_summary {
        CompactionMode::Reuse
    } else if turn_context.config.compact_prompt.is_some() {
        CompactionMode::Custom
    } else if previous_summary.is_some() {
        CompactionMode::Incremental
    } else {
        CompactionMode::Full
    };
    // The unread tail is preserved in replacement history, not interpreted by the
    // summarizer. Remove its calls as well so normalization cannot synthesize outputs.
    // Collect recovery pins first, including references that occur only in that tail.
    let artifact_pin_payload = sess
        .compaction_artifact_pins(&history.tool_history_state(), history.raw_items())
        .await?;
    let source_recovery = local_compaction_source_recovery(
        task_items,
        &summary_items,
        omitted_text || omitted_image_count > 0,
    );
    let mut generated_summary_recovery = None;
    // Read-only owner snapshot; summary generation never mutates requirement
    // status, and an unobserved later transition cannot authorize retirement.
    let plan_lineage = sess.services.plan_store.snapshot_with_lineage().await
        .map(|(_, lineage)| lineage);
    // Persist source recovery while the existing summary request runs. Both
    // must finish before publishing replacement history; cancellation cannot
    // detach a writer or publish a checkpoint with unavailable source bytes.
    let recovery_cancellation = cancellation_token.child_token();
    let (text_recovery_result, summary_text_result) = tokio::join!(
        async {
            let result = match source_recovery {
                Some(source) => match source.await {
                    Ok(canonical) => persist_compaction_recovery(sess.as_ref(), canonical)
                        .await
                        .map(Some),
                    Err(error) => Err(CodexErr::Fatal(format!(
                        "Compaction source retention failed; original history was retained: {error}"
                    ))),
                },
                None => Ok(None),
            };
            if result.is_err() {
                recovery_cancellation.cancel();
            }
            result
        },
        async {
            let cancellation_token = &recovery_cancellation;
            if reuse_previous_summary {
                let bounded = validated_compaction_summary(
                    previous_summary.as_deref(),
                    "",
                    false,
                    turn_context.config.compact_prompt.is_some(),
                )?;
                generated_summary_recovery =
                    generated_summary_recovery_canonical(previous_summary.as_deref(), "", &bounded);
                Ok(bounded)
            } else {
                let rebase_enabled = turn_context.config.compact_prompt.is_none()
                    && previous_summary
                        .as_deref()
                        .is_some_and(has_compaction_section);
                let mut rebase_sections = if rebase_enabled {
                    compaction_rebase_sections(previous_summary.as_deref().unwrap_or_default())
                } else {
                    Vec::new()
                };
                if !rebase_sections.is_empty() {
                    compaction_mode = CompactionMode::Rebase;
                }
                history.replace(summary_items);
                select_compaction_input(
                    &mut input,
                    previous_summary.as_deref(),
                    turn_context.config.compact_prompt.as_deref(),
                    &rebase_sections,
                );
                // The request keeps the turn's base instructions so it can reuse
                // the session's cached prompt prefix; the summarizer contract
                // travels after the compaction prompt in this trailing input.
                input.push(UserInput::Text {
                    text: COMPACTION_BASE_INSTRUCTIONS.trim().to_string(),
                    text_elements: Vec::new(),
                });
                let initial_input_for_turn: ResponseInputItem = ResponseInputItem::from(input);
                history.record_items(
                    &[initial_input_for_turn.into()],
                    turn_context.model_info.truncation_policy.into(),
                );

                let workspace_identity = prefetched_workspace_identity.cloned().flatten();
                // History stays available for the pinned input below, used only
                // when this exact input cannot fit the model window.
                let turn_input = local_compaction_prompt_items(
                    &history.clone().for_local_compaction_prompt(
                        &turn_context.model_info.input_modalities,
                        workspace_identity.as_ref(),
                        &sess.services.git_workspace,
                    ),
                );
                // Advertise the turn's tool schemas, also part of the cached
                // prefix; the prompt below disables calls to them.
                let step_context = sess.capture_step_context(Arc::clone(&turn_context)).await?;
                let tool_router = crate::session::turn::built_tools(
                    sess.as_ref(),
                    &step_context,
                    &[],
                    cancellation_token,
                )
                .await?;
                let max_retries = turn_context.provider.info().stream_max_retries();
                // Reuse the turn's live session when compaction runs inside a turn so the warm transport,
                // sticky routing, and websocket incremental-request state carry over instead of forcing a
                // second handshake while the turn's own connection sits idle. A standalone compaction turn
                // has no such session and opens its own, which then publishes normally on drop.
                //
                // The caller reaches this point only after its response stream has closed: `run_turn`
                // compacts from the post-sampling branch, and pre-sampling compaction runs before any
                // stream is opened. Retries inside this compact turn keep sharing the one session.
                let mut owned_client_session = None;
                let client_session = match client_session {
                    Some(client_session) => client_session,
                    None => owned_client_session.insert(sess.services.model_client.new_session()),
                };
                let window_id = sess.current_window_id().await;
                let responses_metadata = turn_context.turn_metadata_state.to_responses_metadata(
                    sess.installation_id.clone(),
                    window_id,
                    CodexResponsesRequestKind::Compaction(compaction_metadata),
                );

                // Same instructions, schemas and parallel-call setting as the
                // turn's sampling requests; only tool calls are forbidden, as
                // for a terminal continuation.
                let mut prompt = Prompt {
                    input: turn_input.into(),
                    tools: tool_router.model_visible_schemas_for_turn(turn_context.as_ref()),
                    parallel_tool_calls: turn_context.model_info.supports_parallel_tool_calls,
                    tool_calls_disabled: true,
                    base_instructions: crate::session::turn::validation_mode_base_instructions(&turn_context)
                        .as_ref()
                        .clone(),
                    output_schema: local_compaction_output_schema(
                        turn_context.provider.info().is_openai(),
                        turn_context.config.compact_prompt.is_some(), rebase_enabled,
                    ),
                    output_schema_strict: true,
                    ..Default::default()
                };
                let source_input_len = prompt.input.len();
                let mut pin_source = Some(history);
                if prompt.output_schema.is_some() {
                    let mut input = prompt.input.to_vec();
                    input.push(ResponseItem::from(ResponseInputItem::from(vec![UserInput::Text {
                        text: "Return the checkpoint as the required JSON object. Each named field is the complete body of its checkpoint section, without the outer heading. Preserve the full handoff content and uncertainty; JSON shape does not establish semantic completeness.".into(),
                        text_elements: Vec::new(),
                    }])));
                    prompt.input = input.into();
                }
                turn_context.turn_timing_state.begin_compaction_generation();
                let mut retry_state = ResponsesStreamRetryState::new(max_retries);
                let mut retried_invalid_summary = false;
                loop {
                    retry_state.check_retry_budget(cancellation_token)?;
                    // Size the complete request, including format and repair
                    // instructions, before starting another provider attempt.
                    if local_compaction_input_exceeds_window(
                        &prompt, turn_context.model_context_window(),
                    ) && let Some(history) = pin_source.take() {
                        let mut pinned_input = local_compaction_prompt_items(
                            &history.for_local_compaction_prompt_with_checkpoint_pins(
                                &turn_context.model_info.input_modalities,
                                workspace_identity.as_ref(),
                                &sess.services.git_workspace,
                            ),
                        );
                        pinned_input.extend(prompt.input.iter().skip(source_input_len).cloned());
                        prompt.input = pinned_input.into();
                    }
                    if local_compaction_input_exceeds_window(&prompt, turn_context.model_context_window()) {
                        sess.set_total_tokens_full(turn_context.as_ref()).await;
                        return Err(CodexErr::ContextWindowExceeded);
                    }
                    retry_state.begin_attempt(&turn_context, ResponsesStreamRequest::LocalCompaction);
                    let attempt_result = drain_to_completed(
                        &sess,
                        turn_context.as_ref(),
                        &mut *client_session,
                        &responses_metadata,
                        &prompt,
                        cancellation_token,
                    )
                    .await;

                    match attempt_result {
                        Ok(output) => {
                            if let Some(usage) = output.token_usage.as_ref() {
                                analytics_details.record_compaction_receipt(usage);
                            }
                            sess.record_compaction_token_usage(
                                turn_context.as_ref(),
                                output.token_usage.as_ref(),
                            )
                            .await?;
                            let summary_raw =
                                get_last_assistant_message_from_turn(&output.items)
                                    .unwrap_or_default();
                            let normalized = if prompt.output_schema.is_some() {
                                render_structured_compaction_summary(&summary_raw)
                            } else {
                                Ok(summary_raw.clone())
                            };
                            let validated = normalized.and_then(|summary_suffix| if rebase_enabled {
                                rebased_compaction_summary(
                                    previous_summary.as_deref().unwrap_or_default(),
                                    &summary_suffix,
                                    &rebase_sections,
                                    /*bound_overflow*/ retried_invalid_summary,
                                    plan_lineage.as_ref(),
                                )
                            } else {
                                validated_compaction_summary(
                                    previous_summary.as_deref(),
                                    &summary_suffix,
                                    true,
                                    turn_context.config.compact_prompt.is_some(),
                                )
                            });
                            match validated {
                                Ok(summary_text) => {
                                    generated_summary_recovery =
                                        generated_summary_recovery_canonical(
                                            previous_summary.as_deref(),
                                            &summary_raw,
                                            &summary_text,
                                        );
                                    // Even valid summarizer output remains tentative until
                                    // replacement history commits. Its summary is published
                                    // only in that checkpoint, never as ordinary conversation.
                                    break Ok(summary_text);
                                }
                                Err(error) if !retried_invalid_summary => {
                                    // Keep rejected output out of durable and live history. Give the
                                    // model one corrective attempt, without replaying an unbounded
                                    // malformed answer into the already-full compaction request.
                                    retried_invalid_summary = true;
                                    if rebase_enabled {
                                        rebase_sections = (0..COMPACTION_SECTIONS.len()).collect();
                                        compaction_mode = CompactionMode::Rebase;
                                    }
                                    let correction = ResponseItem::from(ResponseInputItem::from(
                                        vec![UserInput::Text {
                                            text: format!(
                                                "The previous compaction handoff was rejected: {error}. Regenerate the handoff with every required checkpoint section and a non-empty body for each section.{}",
                                                compaction_rebase_prompt(&rebase_sections)
                                            ),
                                            text_elements: Vec::new(),
                                        }],
                                    ));
                                    let mut corrected_input = prompt.input.to_vec();
                                    corrected_input.push(correction);
                                    prompt.input = corrected_input.into();
                                    // A completed answer followed by a corrected prompt is a
                                    // new semantic generation, not transport recovery. Keep
                                    // retry_state intact: its budget spans this whole operation.
                                    turn_context
                                        .turn_timing_state
                                        .begin_compaction_repair_generation();
                                }
                                Err(error) => break Err(error),
                            }
                        }
                        Err(err @ (CodexErr::Interrupted | CodexErr::TurnAborted)) => {
                            return Err(err);
                        }
                        Err(e @ CodexErr::ContextWindowExceeded) => {
                            sess.set_total_tokens_full(turn_context.as_ref()).await;
                            return Err(e);
                        }
                        Err(e) if !e.is_retryable() => {
                            return Err(e);
                        }
                        Err(e) => {
                            handle_retryable_response_stream_error(
                                &mut retry_state,
                                max_retries,
                                e,
                                &mut *client_session,
                                sess.as_ref(),
                                turn_context.as_ref(),
                                ResponsesStreamRequest::LocalCompaction,
                                cancellation_token,
                            )
                            .await?;
                            turn_context.turn_timing_state.record_model_retry();
                        }
                    }
                }
            }
        }
    );
    let text_recovery_sidecar = match text_recovery_result {
        Ok(sidecar) => sidecar,
        Err(error) => {
            sess.track_turn_codex_error(turn_context.as_ref(), &error);
            if emit_error_event {
                sess.send_event(&turn_context, EventMsg::Error(error.to_error_event(None)))
                    .await;
            }
            return Err(error);
        }
    };
    let summary_text = match summary_text_result {
        Ok(summary_text) => summary_text,
        Err(error @ (CodexErr::Interrupted | CodexErr::TurnAborted)) => return Err(error),
        Err(error) => {
            sess.track_turn_codex_error(turn_context.as_ref(), &error);
            if emit_error_event {
                let event = EventMsg::Error(error.to_error_event(/*message_prefix*/ None));
                sess.send_event(&turn_context, event).await;
            }
            return Err(error);
        }
    };
    // A generated summary does not expire original constraints. Keep the bounded
    // exact task input and unconsumed tail in source order alongside the handoff.
    let mut summary_for_history = summary_text.clone();
    if omitted_image_count > 0 {
        summary_for_history.push_str("\n\n");
        summary_for_history.push_str(&compaction_image_omission_marker(omitted_image_count));
    }
    let mut summary_item =
        compaction_summary_item_with_artifact_pins(summary_for_history, artifact_pin_payload);
    if let Some(text) = &text_recovery_sidecar {
        bind_omission_receipts_to_recovery(&mut retained_history, text, &omission_receipt_ids);
    }
    if let Some(text) = text_recovery_sidecar
        && let ResponseItem::Message { content, .. } = &mut summary_item
    {
        content.push(ContentItem::InputText { text });
    }
    if let Some(canonical) = generated_summary_recovery {
        let text = persist_compaction_recovery(sess.as_ref(), canonical).await?;
        if let ResponseItem::Message { content, .. } = &mut summary_item {
            content.push(ContentItem::InputText { text });
        }
    }
    // Keep the derived handoff before exact task input. Inserting at the start
    // cannot split a pending call/output pair or duplicate the user's request.
    summary_item.set_turn_id_if_missing(&turn_context.sub_id);
    retained_history.insert(0, summary_item);
    let mut new_history = retained_history;
    let (initial_context, world_state_baseline, fragment_digests) =
        build_compaction_initial_context(
            sess.as_ref(),
            turn_context.as_ref(),
            &initial_context_injection,
        )
        .await;
    if !has_authoritative_task_state_context(&initial_context)
        && let Some(plan) = retained_plan_context(sess.as_ref()).await?
    {
        new_history.push(plan);
    }
    if !initial_context.is_empty() {
        new_history = insert_compaction_initial_context(
            new_history,
            initial_context,
            &initial_context_injection,
        );
    }
    let reference_context_item = match &initial_context_injection {
        InitialContextInjection::AtStart(_) => {
            Some(turn_context.to_turn_context_item_async().await)
        }
    };
    let compacted_item = CompactedItem {
        message: summary_text.clone(),
        // Persist the exact new eviction shape. Older records with `None` still use the legacy
        // reconstruction path that retained raw user messages.
        replacement_history: Some(new_history.clone()),
        provenance: None,
        // The ordered history commit assigns and publishes the next window.
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    };
    sess.replace_compacted_history(
        &turn_context,
        new_history,
        reference_context_item,
        world_state_baseline,
        fragment_digests,
        compacted_item,
        CompactionOperation {
            mode: compaction_mode,
            request: serde_json::to_value(compaction_metadata).map_err(|error| {
                CodexErr::Fatal(format!("failed to encode compaction operation: {error}"))
            })?,
        },
    )
    .await?;
    sess.recompute_token_usage(&turn_context).await;

    // Report what the replacement actually dropped. These values are already
    // computed above for analytics; carrying them on the completed item is what
    // lets a client say more than "context compacted".
    if let TurnItem::ContextCompaction(item) = compaction_item {
        item.tokens_before = analytics_details
            .active_context_tokens_before
            .or(Some(tokens_before));
        item.tokens_after = Some(sess.get_total_token_usage().await);
        item.omitted_image_count = Some(omitted_image_count);
        item.retained_image_count = Some(retained_image_count);
        item.omitted_text = Some(omitted_text);
    }
    Ok(summary_text)
}


/// Selected System skills are retained exactly in the checkpoint, not
/// activated as instructions to the local summarizer, and startup envelopes
/// stay out so the summary does not absorb them. Current stable context keeps
/// its source order, as sampling sends it: regrouping it ahead of history
/// rewrote the request from its first item and forfeited the session's cached
/// prompt prefix.
fn local_compaction_prompt_items(turn_input: &[ResponseItem]) -> Vec<ResponseItem> {
    strip_compaction_startup_envelopes(
        turn_input
            .iter()
            .filter(|item| {
                !matches!(item, ResponseItem::Message { role, .. } if role == "system")
                    || !crate::stable_context::is_trusted_selected_skill_item(item)
            })
            .cloned()
            .collect::<Vec<_>>(),
    )
}

/// The local summarizer reads acquired evidence exactly whenever that fits.
/// Sampling and the compaction trigger measured retired outputs as their
/// checkpoint pins; re-expanded past the window, the request could only fail,
/// again on every later turn, so the caller sends the pins instead. Exact
/// outputs stay recoverable through those pins and the source archive.
fn local_compaction_input_exceeds_window(prompt: &Prompt, context_window: Option<i64>) -> bool {
    context_window.is_some_and(|window| prompt.budget_tokens_for_input(&prompt.input) >= window)
}

fn bounded_task_state_summary(previous_summary: Option<&str>, summary_suffix: &str) -> String {
    let summary_suffix = summary_suffix.trim();
    match (previous_summary, summary_suffix.is_empty()) {
        (Some(previous_summary), true) => {
            truncate_compaction_summary(previous_summary, COMPACT_TASK_STATE_MAX_TOKENS)
        }
        (Some(previous_summary), false) if has_compaction_section(summary_suffix) => {
            truncate_compaction_summary(
                &format!("{previous_summary}\n\n{summary_suffix}"),
                COMPACT_TASK_STATE_MAX_TOKENS,
            )
        }
        (Some(previous_summary), false) => retain_unstructured_incremental_update(
            previous_summary,
            summary_suffix,
            COMPACT_TASK_STATE_MAX_TOKENS,
        ),
        (None, _) => truncate_compaction_summary(
            &format!("{SUMMARY_PREFIX}\n{summary_suffix}"),
            COMPACT_TASK_STATE_MAX_TOKENS,
        ),
    }
}

fn has_compaction_section(summary: &str) -> bool {
    checkpoint_lines(summary).any(|(_, section)| section.is_some())
}

/// Only outer headings are checkpoint structure. Fenced/indented excerpts are
/// evidence, even when they contain literal checkpoint headings.
fn checkpoint_lines(summary: &str) -> impl Iterator<Item = (&str, Option<usize>)> {
    checkpoint_lines_with_structure(summary).map(|(line, section, _)| (line, section))
}

fn checkpoint_lines_with_structure(
    summary: &str,
) -> impl Iterator<Item = (&str, Option<usize>, bool)> {
    summary.lines().scan(None::<(u8, usize)>, |fence, line| {
        let left = line.trim_start_matches(' ');
        let outer = line.len() - left.len() <= 3 && !left.starts_with('\t');
        let marker = left.as_bytes().first().copied();
        if outer && matches!(marker, Some(b'`' | b'~')) {
            let marker = marker.unwrap_or_default();
            let count = left.bytes().take_while(|byte| *byte == marker).count();
            match *fence {
                Some((open, length))
                    if marker == open && count >= length && left[count..].trim().is_empty() =>
                {
                    *fence = None
                }
                None if count >= 3 => *fence = Some((marker, count)),
                _ => {}
            }
            if count >= 3 || fence.is_some() {
                return Some((line, None, false));
            }
        }
        let section = if outer && fence.is_none() {
            COMPACTION_SECTIONS
                .iter()
                .position(|(heading, _)| left.trim_end() == *heading)
        } else {
            None
        };
        Some((line, section, outer && fence.is_none()))
    })
}

fn validate_generated_compaction_summary(
    previous_summary: Option<&str>,
    summary_suffix: &str,
) -> CodexResult<()> {
    let summary_suffix = summary_suffix.trim();
    if summary_suffix.is_empty() {
        return Err(CodexErr::Fatal(
            "compaction completed without a checkpoint handoff".to_string(),
        ));
    }

    if (summary_suffix.contains(INCOMPLETE_CHECKPOINT_EXCERPT)
        || summary_suffix.contains("[truncated]"))
        && checkpoint_lines_with_structure(summary_suffix).any(|(line, _, outer)| {
            outer && matches!(line.trim(), INCOMPLETE_CHECKPOINT_EXCERPT | "[truncated]")
        })
    {
        return Err(CodexErr::Fatal(
            "compaction returned an incomplete excerpt, not a standalone checkpoint".into(),
        ));
    }

    if !has_compaction_section(summary_suffix) {
        return Err(CodexErr::Fatal(
            "default compaction requires structured checkpoint sections".into(),
        ));
    }

    if previous_summary.is_some() && !has_nonempty_compaction_section(summary_suffix) {
        return Err(CodexErr::Fatal(
            "incremental compaction handoff did not contain a recognized non-empty checkpoint section"
                .to_string(),
        ));
    }

    let complete_summary = match previous_summary {
        Some(previous_summary) => format!("{previous_summary}\n\n{summary_suffix}"),
        None => summary_suffix.to_string(),
    };
    validate_compaction_sections_populated(&complete_summary)
}

/// Section completeness only. Runtime incomplete-excerpt markers inherited
/// from an earlier bounded checkpoint are not model output: their exact
/// originals stay recoverable, so they must not make a fresh update invalid.
fn validate_compaction_sections_populated(complete_summary: &str) -> CodexResult<()> {
    let populated_sections = compaction_section_bodies(complete_summary);
    let missing = COMPACTION_SECTIONS
        .iter()
        .zip(populated_sections)
        .filter_map(|((heading, _), populated)| (!populated).then_some(*heading))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(CodexErr::Fatal(format!(
            "compaction handoff is incomplete; missing non-empty sections: {}",
            missing.join(", ")
        )))
    }
}

fn generated_summary_recovery_canonical(
    previous_summary: Option<&str>,
    summary_suffix: &str,
    bounded: &str,
) -> Option<CanonicalToolResult> {
    let items = previous_summary
        .into_iter()
        .chain(std::iter::once(summary_suffix))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>();
    (!items.iter().all(|text| bounded.contains(text.trim()))).then(|| {
        CanonicalToolResult::json(serde_json::json!({
            "version": 1,
            "kind": "generated_compaction_summary_recovery",
            "instruction": "Exact generated handoffs before local trimming, in chronological order. Marked incomplete sections are not standalone conclusions: recover omitted requirements, qualifications and actions under /items before relying on them; newer position alone does not establish supersession.",
            "items": items,
        }))
    })
}

fn validated_compaction_summary(
    previous_summary: Option<&str>,
    summary_suffix: &str,
    validate_suffix: bool,
    allow_custom_freeform: bool,
) -> CodexResult<String> {
    if validate_suffix && summary_suffix.trim().is_empty() {
        return Err(CodexErr::Fatal(
            "compaction completed without a checkpoint handoff".into(),
        ));
    }
    if validate_suffix && !allow_custom_freeform {
        validate_generated_compaction_summary(previous_summary, summary_suffix)?;
    }
    let summary_text = bounded_task_state_summary(previous_summary, summary_suffix);
    // Locally bounded excerpts are explicitly incomplete, with exact originals
    // attached by generated_summary_recovery_canonical. Do not certify them as
    // complete generated checkpoints merely because their headings survived.
    if !allow_custom_freeform
        && has_compaction_section(&summary_text)
        && !summary_text.contains(INCOMPLETE_CHECKPOINT_EXCERPT)
    {
        validate_generated_compaction_summary(None, &summary_text)?;
    }
    Ok(summary_text)
}

fn has_nonempty_compaction_section(summary: &str) -> bool {
    compaction_section_bodies(summary)
        .into_iter()
        .any(|populated| populated)
}

fn compaction_section_bodies(summary: &str) -> [bool; COMPACTION_SECTIONS.len()] {
    let mut populated = [false; COMPACTION_SECTIONS.len()];
    let mut current = None;
    for (line, section) in checkpoint_lines(summary) {
        let trimmed = line.trim();
        if let Some(index) = section {
            current = Some(index);
        } else if !trimmed.is_empty()
            && let Some(index) = current
        {
            // Incremental checkpoints may repeat a heading. An earlier empty
            // occurrence must not hide a later body for the same section.
            populated[index] = true;
        }
    }
    populated
}

fn retain_unstructured_incremental_update(
    previous_summary: &str,
    summary_suffix: &str,
    max_tokens: usize,
) -> String {
    let update = bounded_checkpoint_excerpt(
        &format!("\n\n{summary_suffix}"),
        COMPACT_UNSTRUCTURED_UPDATE_MAX_TOKENS.min(max_tokens),
    );
    let previous_budget = max_tokens.saturating_sub(approx_token_count(&update));
    bounded_checkpoint_excerpt(
        &format!(
            "{}{}",
            truncate_compaction_summary(previous_summary, previous_budget),
            update
        ),
        max_tokens,
    )
}

const INCOMPLETE_CHECKPOINT_EXCERPT: &str = "[Incomplete excerpt; recover /items.]";

fn bounded_checkpoint_excerpt(text: &str, max_tokens: usize) -> String {
    if approx_token_count(text) <= max_tokens {
        return text.to_string();
    }
    let budget = max_tokens.saturating_sub(approx_token_count(INCOMPLETE_CHECKPOINT_EXCERPT) + 1);
    let units = vec![(text.to_string(), approx_token_count(text))];
    format!(
        "{INCOMPLETE_CHECKPOINT_EXCERPT}\n{}",
        retain_newest_section_updates(&units, budget)
    )
}

fn truncate_compaction_summary(summary: &str, max_tokens: usize) -> String {
    if approx_token_count(summary) <= max_tokens {
        return summary.to_string();
    }
    // Origin is harness-owned, not introductory prose competing with sections.
    // Keep the current ID-plus-prefix recognition contract on every later pass.
    if let Some(body) = summary.strip_prefix(SUMMARY_PREFIX) {
        let body = truncate_compaction_summary(
            body.trim_start(),
            max_tokens.saturating_sub(approx_token_count(SUMMARY_PREFIX) + 1),
        );
        return format!("{SUMMARY_PREFIX}\n{body}");
    }
    if !has_compaction_section(summary) {
        return bounded_checkpoint_excerpt(summary, max_tokens);
    }

    let mut preamble = Vec::new();
    let mut sections = COMPACTION_SECTIONS
        .iter()
        .map(|(heading, budget)| (*heading, *budget, Vec::<Vec<&str>>::new()))
        .collect::<Vec<_>>();
    let mut current = None;
    for (line, section) in checkpoint_lines(summary) {
        if let Some(index) = section {
            current = Some(index);
            sections[index].2.push(Vec::new());
            continue;
        }
        match current {
            Some(index) => {
                if let Some(body) = sections[index].2.last_mut() {
                    body.push(line);
                }
            }
            None => preamble.push(line),
        }
    }

    let sections = sections
        .into_iter()
        .filter(|(_, _, updates)| !updates.is_empty())
        .map(|(heading, budget, updates)| {
            let updates = updates
                .into_iter()
                .map(|lines| {
                    let body = lines.join("\n");
                    let tokens = approx_token_count(body.trim());
                    (body, tokens)
                })
                .collect::<Vec<_>>();
            (heading, budget, updates)
        })
        .collect::<Vec<_>>();
    let full_preamble = preamble.join("\n");
    let full_bodies = sections
        .iter()
        .map(|(heading, _, updates)| {
            if *heading == GOAL_HEADING {
                retain_latest_goal(updates, usize::MAX)
            } else {
                updates
                    .iter()
                    .map(|(body, _)| body.trim())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            }
        })
        .collect::<Vec<_>>();
    let full = render_structured_compaction(&full_preamble, &sections, &full_bodies);
    if approx_token_count(&full) <= max_tokens {
        return full;
    }
    let section_units = sections.iter().map(|(heading, _, updates)| {
        if *heading == GOAL_HEADING { Vec::new() } else { section_update_units(updates) }
    }).collect::<Vec<_>>();
    let mut bodies = vec![INCOMPLETE_CHECKPOINT_EXCERPT.to_string(); sections.len()];
    let mut bounded_preamble = String::new();
    let headings = sections
        .iter()
        .map(|(heading, _, _)| TokenCountEstimate::new(heading))
        .collect::<Vec<_>>();
    let mut body_costs = bodies
        .iter()
        .map(|body| TokenCountEstimate::new(body))
        .collect::<Vec<_>>();
    let summary_cost = |preamble: Option<TokenCountEstimate>, costs: &[TokenCountEstimate]| {
        let mut parts = headings
            .iter()
            .zip(costs)
            .map(|(heading, body)| heading.then(*body, 1));
        let first = preamble.unwrap_or_else(|| parts.next().unwrap_or_default());
        parts
            .fold(first, |total, part| total.then(part, 2))
            .tokens()
    };
    if summary_cost(None, &body_costs) > max_tokens {
        // A syntactically complete handoff has an irreducible minimum. Callers use a
        // substantially larger bound, but preserve the typed structure if a future caller
        // supplies an impossible limit instead of cutting a heading or body mid-field.
        return render_structured_compaction(&bounded_preamble, &sections, &bodies);
    }

    // Protect goal/next-action allocations, then spend spare capacity on
    // unresolved obligations before completed work or introductory prose.
    let mut allocation_order = (0..sections.len()).collect::<Vec<_>>();
    allocation_order.sort_by_key(|&index| match sections[index].0 {
        GOAL_HEADING => 0,
        NEXT_ACTION_HEADING => 1,
        UNRESOLVED_WORK_HEADING => 2,
        EVIDENCE_HEADING => 3,
        CURRENT_STATE_HEADING => 4,
        _ => 5,
    });
    for borrow_spare in [false, true] {
        for &index in &allocation_order {
            let (heading, configured_budget, updates) = &sections[index];
            if bodies[index] == full_bodies[index] {
                continue;
            }
            let mut low = 0usize;
            let mut high = if borrow_spare || *heading == UNRESOLVED_WORK_HEADING {
                max_tokens
            } else {
                (*configured_budget).min(max_tokens)
            };
            let mut selected_cost = body_costs[index];
            while low < high {
                let candidate_budget = low.saturating_add(high).saturating_add(1) / 2;
                let candidate = if *heading == GOAL_HEADING {
                    retain_latest_goal(updates, candidate_budget)
                } else {
                    retain_section_units(&section_units[index], candidate_budget)
                };
                let candidate = if candidate != full_bodies[index] {
                    format!("{INCOMPLETE_CHECKPOINT_EXCERPT}\n{candidate}")
                } else {
                    candidate
                };
                body_costs[index] = TokenCountEstimate::new(&candidate);
                if summary_cost(None, &body_costs) <= max_tokens {
                    low = candidate_budget;
                    bodies[index] = candidate;
                    selected_cost = body_costs[index];
                } else {
                    high = candidate_budget.saturating_sub(1);
                }
            }
            body_costs[index] = selected_cost;
        }
    }

    // Introductory prose receives only capacity left after obligations.
    let mut low = 0usize;
    let mut high = COMPACT_PREAMBLE_MAX_TOKENS.min(max_tokens);
    while low < high {
        let candidate_budget = low.saturating_add(high).saturating_add(1) / 2;
        let candidate = bounded_checkpoint_excerpt(&full_preamble, candidate_budget);
        let cost = (!candidate.trim().is_empty()).then(|| TokenCountEstimate::new(&candidate));
        if summary_cost(cost, &body_costs) <= max_tokens {
            low = candidate_budget;
            bounded_preamble = candidate;
        } else {
            high = candidate_budget.saturating_sub(1);
        }
    }
    render_structured_compaction(&bounded_preamble, &sections, &bodies)
}

type CompactionSection<'a> = (&'a str, usize, Vec<(String, usize)>);

fn render_structured_compaction(
    preamble: &str,
    sections: &[CompactionSection<'_>],
    bodies: &[String],
) -> String {
    let mut rendered = String::new();
    if !preamble.trim().is_empty() {
        rendered.push_str(preamble);
    }
    for ((heading, _, _), body) in sections.iter().zip(bodies) {
        if !rendered.is_empty() {
            rendered.push_str("\n\n");
        }
        rendered.push_str(heading);
        rendered.push('\n');
        rendered.push_str(body);
    }
    rendered
}

fn retain_newest_section_updates(updates: &[(String, usize)], max_tokens: usize) -> String {
    retain_section_units(&section_update_units(updates), max_tokens)
}

fn section_update_units(updates: &[(String, usize)]) -> Vec<(&str, TokenCountEstimate)> {
    let mut prepared = Vec::new();
    for (update, _) in updates.iter().rev() {
        let structured = checkpoint_lines_with_structure(update).any(|(line, _, outer)| {
            !outer || line.trim_start().starts_with('>') || line.trim_start().starts_with("Source:")
        });
        let units = if structured { vec![update.as_str()] } else { update.rsplit("\n\n").collect() };
        prepared.extend(units.into_iter().map(str::trim).filter(|unit| !unit.is_empty())
            .map(|unit| (unit, TokenCountEstimate::new(unit))));
    }
    prepared
}

fn retain_section_units(units: &[(&str, TokenCountEstimate)], max_tokens: usize) -> String {
    let mut retained = Vec::new();
    let mut retained_cost: Option<TokenCountEstimate> = None;
    // Paragraphs keep multiline obligations and their qualifications together.
    // Never sample characters from a negation or fabricate a complete sentence.
    for &(unit, cost) in units {
        let cost = retained_cost.map_or(cost, |previous| previous.then(cost, 2));
        if cost.tokens() <= max_tokens {
            retained.push(unit);
            retained_cost = Some(cost);
        }
    }
    retained.reverse();
    retained.join("\n\n")
}

/// Goal updates can contain still-binding constraints. Preserve fitting updates
/// in source order; exact omissions are retained in the recovery artifact.
fn retain_latest_goal(updates: &[(String, usize)], max_tokens: usize) -> String {
    retain_newest_section_updates(updates, max_tokens)
}

pub(crate) struct CompactionAnalyticsAttempt {
    _timing_guard: crate::turn_timing::TurnTimingGuard,
    thread_id: String,
    turn_id: String,
    trigger: CompactionTrigger,
    reason: CompactionReason,
    implementation: CompactionImplementation,
    phase: CompactionPhase,
    active_context_tokens_before: i64,
    started_at: u64,
    start_instant: Instant,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct CompactionAnalyticsDetails {
    pub(crate) active_context_tokens_before: Option<i64>,
    pub(crate) retained_image_count: Option<usize>,
    pub(crate) compaction_summary_tokens: Option<i64>,
    pub(crate) cached_input_tokens: Option<i64>,
}

impl CompactionAnalyticsDetails {
    /// Applies a compaction receipt. Only components the provider reported
    /// replace the current values; an unreported class keeps the local
    /// measurement (or stays unknown) instead of becoming a numeric zero.
    pub(crate) fn record_compaction_receipt(&mut self, token_usage: &TokenUsage) {
        if let Some(input_tokens) = token_usage.known_input() {
            self.active_context_tokens_before = Some(input_tokens);
        }
        if let Some(output_tokens) = token_usage.known_output() {
            self.compaction_summary_tokens = Some(output_tokens);
        }
        if let Some(cached_input_tokens) = token_usage.known_cached_input() {
            self.cached_input_tokens = Some(cached_input_tokens);
        }
    }
}

impl CompactionAnalyticsAttempt {
    pub(crate) async fn begin(
        sess: &Session,
        turn_context: &TurnContext,
        trigger: CompactionTrigger,
        reason: CompactionReason,
        implementation: CompactionImplementation,
        phase: CompactionPhase,
    ) -> Self {
        let timing_guard = turn_context
            .turn_timing_state
            .begin_local_phase(crate::turn_timing::TurnLocalPhase::Compaction);
        let active_context_tokens_before = sess.get_total_token_usage().await;
        Self {
            _timing_guard: timing_guard,
            thread_id: sess.thread_id.to_string(),
            turn_id: turn_context.sub_id.clone(),
            trigger,
            reason,
            implementation,
            phase,
            active_context_tokens_before,
            started_at: now_unix_seconds(),
            start_instant: Instant::now(),
        }
    }

    pub(crate) async fn track(
        self,
        sess: &Session,
        status: CompactionStatus,
        codex_error: Option<&CodexErr>,
        details: CompactionAnalyticsDetails,
    ) {
        let CompactionAnalyticsDetails {
            active_context_tokens_before,
            retained_image_count,
            compaction_summary_tokens,
            cached_input_tokens,
        } = details;
        let active_context_tokens_before =
            active_context_tokens_before.unwrap_or(self.active_context_tokens_before);
        let active_context_tokens_after = sess.get_total_token_usage().await;
        sess.services
            .analytics_events_client
            .track_compaction(CodexCompactionEvent {
                thread_id: self.thread_id,
                turn_id: self.turn_id,
                trigger: self.trigger,
                reason: self.reason,
                implementation: self.implementation,
                phase: self.phase,
                strategy: CompactionStrategy::Memento,
                status,
                codex_error_kind: codex_error.map(Into::into),
                codex_error_http_status_code: codex_error
                    .and_then(CodexErr::http_status_code_value),
                active_context_tokens_before,
                active_context_tokens_after,
                retained_image_count,
                compaction_summary_tokens,
                cached_input_tokens,
                started_at: self.started_at,
                completed_at: now_unix_seconds(),
                duration_ms: Some(
                    u64::try_from(self.start_instant.elapsed().as_millis()).unwrap_or(u64::MAX),
                ),
            });
    }
}

pub(crate) fn compaction_status_from_result<T>(result: &CodexResult<T>) -> CompactionStatus {
    match result {
        Ok(_) => CompactionStatus::Completed,
        Err(CodexErr::Interrupted | CodexErr::TurnAborted) => CompactionStatus::Interrupted,
        Err(_) => CompactionStatus::Failed,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CompactedUserMessage {
    source_item_id: Option<String>,
    content: Vec<UserInput>,
    internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
}

pub(crate) struct TaskInputCheckpoint {
    pub(crate) items: Vec<ResponseItem>,
    pub(crate) retained_images: usize,
    pub(crate) omitted_images: usize,
    pub(crate) omitted_text: bool,
}

impl TaskInputCheckpoint {
    pub(crate) fn needs_source_recovery(&self) -> bool {
        self.omitted_text || self.omitted_images > 0
    }
}

pub(crate) async fn persist_task_compaction_text_recovery(
    sess: &Session,
    source_items: &[ResponseItem],
    omitted_text: bool,
) -> CodexResult<Option<String>> {
    if !omitted_text {
        return Ok(None);
    }
    let canonical = compaction_text_recovery_for_items(task_compaction_items(source_items));
    persist_compaction_recovery(sess, canonical).await.map(Some)
}

pub(crate) async fn retained_plan_context(sess: &Session) -> CodexResult<Option<ResponseItem>> {
    use crate::context::ContextualUserFragment;
    use crate::context::InternalContextSource;
    use crate::context::InternalModelContextFragment;

    let Some((plan, lineage)) = sess.services.plan_store.snapshot_with_lineage().await else {
        return Ok(None);
    };
    let retained = serde_json::json!({
        "revision": crate::plan_store::plan_revision_with_lineage(Some(&plan), &lineage),
        "current_plan": plan,
        "step_ids": plan.plan.iter().map(|step| lineage.step_id(&step.step)).collect::<Vec<_>>(),
        "lineage": lineage.checklist_metadata(),
        "remaining_requirements": lineage.remaining_requirements(&plan),
        "pinned_requirements": lineage.pinned_requirements(),
        "obligations": lineage.obligation_summary(&plan),
    });
    let serialized = serde_json::to_string(&retained)
        .map_err(|error| CodexErr::Fatal(format!("could not preserve compaction plan: {error}")))?;
    let bounded = truncate_text_to_token_ceiling(&serialized, COMPACT_TASK_STATE_MAX_TOKENS);
    let mut body = format!(
        "Stored checklist restored after compaction, not new instructions or proof of completion. \
Later update_plan results replace it. Original remaining requirements and pinned IDs, sources and states are retained separately from display rows. Summary rewrites never complete requirements; pins require explicit resolve or accepted user-authorized supersession.\n{bounded}"
    );
    if bounded != serialized {
        let recovery = persist_compaction_recovery(
            sess,
            CanonicalToolResult::json(serde_json::json!({"items": [retained]})),
        )
        .await?;
        body.push_str(
            "\nChecklist excerpt is incomplete. Recover the stored checklist at /items/0.\n",
        );
        body.push_str(&recovery);
    }
    let mut item = compaction_context_message(
        InternalModelContextFragment::new(
            InternalContextSource::from_static("compaction_plan"),
            body,
        )
        .render(),
    );
    crate::stable_context::mark_trusted_stable_context_item(&mut item);
    Ok(Some(item))
}

/// Unread outputs must survive compaction, but need not remain inline at an
/// unbounded size. Unlike consumed-output retirement, this never asserts that
/// the model observed the result or that uncertain effects were resolved.
/// Archive the exact item before changing its body, keeping its call identity
/// and success metadata. Both local and remote replacement paths use this.
pub(crate) async fn bound_compaction_tool_outputs(
    sess: &Session,
    items: &mut [ResponseItem],
) -> CodexResult<()> {
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputContentItem;

    const INLINE_OUTPUT_TOKENS: usize = 8_000;
    let mut outputs = items.iter().enumerate().filter_map(|(index, item)| {
        let output = match item {
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => output,
            _ => return None,
        };
        let tokens = match &output.body {
            FunctionCallOutputBody::Text(text) => approx_token_count(text),
            FunctionCallOutputBody::ContentItems(parts) => {
                parts.iter().filter_map(|part| match part {
                    FunctionCallOutputContentItem::InputText { text } => Some(approx_token_count(text)),
                    _ => None,
                }).fold(0usize, usize::saturating_add)
            }
        };
        Some((tokens, index))
    }).collect::<Vec<_>>();
    let mut total = outputs.iter().fold(0usize, |sum, (tokens, _)| sum.saturating_add(*tokens));
    outputs.sort_unstable_by(|left, right| right.cmp(left));
    for (tokens, index) in outputs {
        if total <= INLINE_OUTPUT_TOKENS {
            break;
        }
        // Tiny results are not worth replacing with a larger envelope.
        if tokens <= 1_024 {
            continue;
        }
        let recovery = persist_compaction_recovery(sess, CanonicalToolResult::json(
            serde_json::json!({"items": [&items[index]]}),
        )).await?;
        let pin = serde_json::json!({
            "kind": "compaction_unread_tool_output",
            "unresolved": true,
            "effects_state": "unknown",
            "retry_safe": false,
            "instruction": "Unread tool result, NOT a summary or proof of success. Recover the exact original response item at /items/0 before relying on its outcome or retrying any work. Cancellation is not rollback; preserve live-operation and effect uncertainty from the original result. Do not rerun its producer.",
            "recovery": serde_json::from_str::<serde_json::Value>(&recovery)
                .map_err(|error| CodexErr::Fatal(format!("invalid compaction recovery reference: {error}")))?,
        }).to_string();
        total = total.saturating_sub(tokens).saturating_add(approx_token_count(&pin));
        match &mut items[index] {
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                match &mut output.body {
                    FunctionCallOutputBody::Text(text) => *text = pin,
                    FunctionCallOutputBody::ContentItems(parts) => {
                        // Archive all parts, but replace only text. Images and
                        // opaque content retain their original order and bytes.
                        let mut pin = Some(pin);
                        parts.retain_mut(|part| match part {
                            FunctionCallOutputContentItem::InputText { text } => {
                                if let Some(pin) = pin.take() {
                                    *text = pin;
                                    true
                                } else {
                                    false
                                }
                            }
                            _ => true,
                        });
                    }
                }
            }
            _ => unreachable!("selected a textual tool output"),
        }
    }
    Ok(())
}

async fn persist_compaction_recovery(
    sess: &Session,
    canonical: CanonicalToolResult,
) -> CodexResult<String> {
    let codex_home = sess.codex_home().await;
    let artifact = create_canonical_output_artifact(
        codex_home.as_path(),
        &sess.thread_id().to_string(),
        &canonical,
    )
    .await;
    if !artifact.complete {
        return Err(CodexErr::Fatal(
            "Compaction could not preserve exact unresolved text; original history was retained."
                .to_string(),
        ));
    }
    let sidecar = compaction_text_recovery_sidecar(&canonical, &artifact).ok_or_else(|| {
        CodexErr::Fatal(
            "Compaction recovery artifact is unavailable; original history was retained."
                .to_string(),
        )
    })?;
    let artifact_id = artifact.artifact_id().ok_or_else(|| {
        CodexErr::Fatal("Compaction recovery artifact has no identity".to_string())
    })?;
    sess.try_register_tool_artifact_origin(
        artifact_id.clone(),
        format!("compaction-recovery:{artifact_id}"),
        canonical.exact_bytes,
        canonical.sha256.clone(),
    )
    .await?;
    // The sidecar may become the only surviving reference. Persist its owner
    // before replacement history can retire the exact source text.
    sess.flush_tool_history_persistence().await?;
    Ok(sidecar)
}

fn local_compaction_source_recovery(
    task_items: Vec<ResponseItem>,
    summarized_items: &[ResponseItem],
    omitted_text: bool,
) -> Option<tokio::task::JoinHandle<CanonicalToolResult>> {
    let has_source = summarized_items.iter().any(|item| {
        (is_compaction_model_generated_item(item)
            && !matches!(item, ResponseItem::Reasoning { .. }))
            || compaction_output_call_id(item).is_some()
    });
    (omitted_text || has_source).then(|| {
        let summarized_items = summarized_items.to_vec();
        // Serialization is measurable for large consumed sources. Keep it off
        // the async executor and overlap it with the existing model request.
        // Only pure serialization runs here: all durable writes remain awaited
        // by compaction before it may publish replacement history.
        tokio::task::spawn_blocking(move || {
            compaction_text_recovery_with_source(task_items, &summarized_items)
        })
    })
}

fn compaction_text_recovery_for_items(items: Vec<ResponseItem>) -> CanonicalToolResult {
    compaction_text_recovery_with_source(items, &[])
}

fn compaction_text_recovery_with_source(
    items: Vec<ResponseItem>,
    summarized_items: &[ResponseItem],
) -> CanonicalToolResult {
    let exact_items = items
        .into_iter()
        .filter(|item| {
            matches!(
                item,
                ResponseItem::Message { .. } | ResponseItem::AgentMessage { .. }
            )
        })
        .collect::<Vec<_>>();
    let mut value = serde_json::json!({
        "version": 1,
        "kind": "local_compaction_text_recovery",
        "instruction": "Recover exact unresolved text with read_tool_output and a json_pointer under /items; do not ask the user to repeat it.",
        "items": exact_items,
    });
    if !summarized_items.is_empty() {
        // Exclude private reasoning. Public assistant conclusions and tool
        // observations remain exact, including qualifications omitted by the
        // generated handoff. Existing /items selectors remain compatible.
        value["summarized_items"] = serde_json::json!(
            summarized_items
                .iter()
                .filter(|item| !matches!(item, ResponseItem::Reasoning { .. }))
                .collect::<Vec<_>>()
        );
        value["summary_is_lossless"] = false.into();
        value["instruction"] = "Source archive, not an open-work checklist. Summary omission is not resolution. Recover missing decision-relevant text with read_tool_output under /summarized_items or /items; do not repeat producers or recover irrelevant history.".into();
    }
    CanonicalToolResult::json(value)
}

fn compaction_text_recovery_sidecar(
    canonical: &CanonicalToolResult,
    artifact: &CanonicalOutputArtifact,
) -> Option<String> {
    let artifact_id = artifact.artifact_id()?;
    let mut value = serde_json::json!({
        "version": 1,
        "kind": "local_compaction_text_recovery",
        "artifact_id": artifact_id,
        "canonical_sha256": canonical.sha256,
        "canonical_bytes": canonical.exact_bytes,
        "complete": artifact.complete,
        "unavailable_ranges": artifact.unavailable_ranges,
        "instruction": "Use read_tool_output with this artifact_id and json_pointer selectors under /items to recover exact omitted unresolved text; do not rerun work or ask the user to repeat it."
    });
    if canonical
        .value
        .as_ref()
        .is_some_and(|value| value.get("summarized_items").is_some())
    {
        value["summary_is_lossless"] = false.into();
        value["source_selector"] =
            serde_json::json!({"kind":"json_pointer", "pointer":"/summarized_items"});
        value["instruction"] = "Generated summary completeness is unverified; omission is not resolution. Exact consumed assistant/tool text survives under /summarized_items; task input under /items. Recover only missing decision-relevant details with read_tool_output, not producers or irrelevant history.".into();
    }
    serde_json::to_string(&value).ok()
}


struct LocalCompactionOutput {
    items: Vec<ResponseItem>,
    token_usage: Option<TokenUsage>,
}

async fn drain_to_completed(
    sess: &Session,
    turn_context: &TurnContext,
    client_session: &mut ModelClientSession,
    responses_metadata: &CodexResponsesMetadata,
    prompt: &Prompt,
    cancellation_token: &CancellationToken,
) -> CodexResult<LocalCompactionOutput> {
    client_session.set_turn_timing(Arc::clone(&turn_context.turn_timing_state));
    client_session.set_startup_timing(Arc::clone(&sess.startup_timing));
    let model_request_timing_guard = turn_context.turn_timing_state.begin_model_request_wait();
    let stream_result = tokio::select! {
        _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
        result = async {
            let service_tier = crate::session::turn::service_tier_for_sampling(sess, turn_context).await;
            client_session.stream(
            prompt,
            &turn_context.model_info,
            &turn_context.session_telemetry,
            turn_context.request_reasoning_effort(),
            codex_protocol::config_types::ReasoningSummary::None,
            service_tier,
            responses_metadata,
            ).await
        } => result,
    };
    drop(model_request_timing_guard);
    sess.bind_rate_limit_account(client_session.dispatched_account().cloned())
        .await;
    if let Err(CodexErr::UsageLimitReached(error)) = &stream_result
        && let Some(snapshot) = error.rate_limits.as_deref()
    {
        sess.update_rate_limits(turn_context, snapshot.clone()).await;
    }
    let mut stream = stream_result?;
    let mut items = Vec::new();
    let mut model_stream_processing_handoff: Option<crate::turn_timing::TurnTimingGuard> = None;
    loop {
        let model_stream_wait_timing_guard =
            turn_context.turn_timing_state.begin_model_stream_wait();
        // Hand off between the wait and processing guards without a gap, so each
        // streamed event does not record a separate orchestration interval.
        drop(model_stream_processing_handoff.take());
        let maybe_event = tokio::select! {
            _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
            event = stream.next() => event,
        };
        let Some(event) = maybe_event else {
            return Err(CodexErr::Stream(
                "stream closed before response.completed".into(),
                None,
            ));
        };
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                drop(model_stream_wait_timing_guard);
                if let Some(token_usage) = error.terminal_usage() {
                    turn_context
                        .turn_timing_state
                        .record_generation_token_usage(token_usage.as_ref());
                    if let Some(usage) = token_usage.as_ref() {
                        sess.record_compaction_token_usage(turn_context, Some(usage))
                            .await?;
                    }
                }
                let error = error.into_response_error();
                if let CodexErr::UsageLimitReached(limit) = &error
                    && let Some(snapshot) = limit.rate_limits.as_deref()
                {
                    sess.update_rate_limits(turn_context, snapshot.clone()).await;
                }
                return Err(error);
            }
        };
        model_stream_processing_handoff = turn_context
            .turn_timing_state
            .begin_model_stream_processing(&event);
        drop(model_stream_wait_timing_guard);
        if let ResponseEvent::Completed { token_usage, .. } = &event {
            // Give the compaction request its completion time and usage in the
            // turn timing record, as remote compaction does.
            turn_context
                .turn_timing_state
                .record_response_event_milestones(&event);
            turn_context
                .turn_timing_state
                .record_generation_token_usage(token_usage.as_ref());
        }
        match event {
            ResponseEvent::OutputItemDone(item) => {
                items.push(item);
            }
            ResponseEvent::RateLimits(snapshot) => {
                sess.update_rate_limits(turn_context, snapshot).await;
            }
            ResponseEvent::Completed { token_usage, .. } => {
                return Ok(LocalCompactionOutput { items, token_usage });
            }
            _ => continue,
        }
    }
}

