use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::TurnTiming;
use codex_protocol::protocol::TurnTimingModelRequest;
use codex_protocol::protocol::TurnTimingProviderTokenUsage;

/// Recover only recorded evidence. Missing timestamps and incomplete requests
/// remain unknown; this must never masquerade as a complete terminal profile.
pub(crate) fn lost_turn_recovery(items: &[RolloutItem]) -> (String, TurnTiming) {
    let mut checkpoint = None;
    let mut plan = None;
    let mut timing = TurnTiming {
        schema_version: crate::turn_timing::TIMING_SCHEMA_VERSION,
        profile_valid: false,
        classification_complete: false,
        ..TurnTiming::default()
    };
    let mut last_total_tokens = None;
    let mut timing_observed_at = None;
    for item in items {
        match item {
            RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. })
                if role == "developer" =>
            {
                for part in content {
                    if let ContentItem::InputText { text } = part
                        && text.starts_with("<completed_phase_checkpoint>")
                        && !crate::tool_history::is_pressure_retirement_checkpoint_text(text)
                    {
                        checkpoint = Some(text.as_str());
                    }
                }
            }
            RolloutItem::EventMsg(EventMsg::PlanUpdate(value)) => {
                plan = serde_json::to_string(value).ok();
            }
            RolloutItem::EventMsg(EventMsg::TurnStarted(value)) => {
                timing = TurnTiming {
                    schema_version: crate::turn_timing::TIMING_SCHEMA_VERSION,
                    started_at_unix_ms: value.started_at.and_then(|value| value.checked_mul(1_000)),
                    ..TurnTiming::default()
                };
                timing_observed_at = None;
            }
            RolloutItem::SamplingBoundary(value) => {
                if let Some(observation) = &value.timing_checkpoint {
                    let previous = std::mem::replace(&mut timing, observation.timing.clone());
                    if observation.incremental {
                        merge_checkpoint_entries(&mut timing, previous);
                    }
                    timing_observed_at = Some(observation.observed_at_unix_ms);
                }
                if let Some(request) = timing.model_requests.iter_mut().find(|request| {
                    request.sampling_request_id.as_deref()
                        == Some(value.sampling_request_id.as_str())
                }) {
                    if !request
                        .physical_attempt_ids
                        .contains(&value.physical_attempt_id)
                    {
                        request
                            .physical_attempt_ids
                            .push(value.physical_attempt_id.clone());
                    }
                } else {
                    timing.model_requests.push(TurnTimingModelRequest {
                        // A sampling identity is not a logical generation.
                        // Legacy boundaries do not carry that relationship.
                        sampling_request_id: Some(value.sampling_request_id.clone()),
                        physical_attempt_ids: vec![value.physical_attempt_id.clone()],
                        ..TurnTimingModelRequest::default()
                    });
                }
            }
            RolloutItem::EventMsg(EventMsg::TokenCount(value)) => {
                if let Some(info) = &value.info {
                    // Rate-limit-only updates repeat the last usage. Do not
                    // attribute those stale tokens to a newly started request.
                    if last_total_tokens != Some(info.total_token_usage.total_tokens)
                        && let Some(request) = timing.model_requests.last_mut()
                    {
                        let usage = &info.last_token_usage;
                        request.output_tokens = usage.output_tokens.max(0) as u64;
                        request.reasoning_output_tokens =
                            usage.reasoning_output_tokens.max(0) as u64;
                        request.token_usage = Some(TurnTimingProviderTokenUsage {
                            coverage: usage.coverage,
                            output_excluding_reported_reasoning_tokens: usage
                                .known_visible_output()
                                .and_then(|tokens| u64::try_from(tokens).ok()),
                            input_tokens: usage.input_tokens.max(0) as u64,
                            cached_input_tokens: usage.cached_input_tokens.max(0) as u64,
                            visible_output_tokens: request
                                .output_tokens
                                .saturating_sub(request.reasoning_output_tokens),
                            reasoning_tokens: request.reasoning_output_tokens,
                            total_tokens: usage.total_tokens.max(0) as u64,
                        });
                    }
                    last_total_tokens = Some(info.total_token_usage.total_tokens);
                }
            }
            _ => {}
        }
    }
    // Only checkpoint scalars establish logical-generation cardinality. Row
    // indices default to zero in old records, and boundaries can be retries.
    // Keep the known prefix count; do not manufacture one from either identity.
    let observed_attempts = timing
        .model_requests
        .iter()
        .filter(|request| request.dispatch_ms.is_some())
        .count() as u32;
    timing.counters.model_request_count =
        timing.counters.model_request_count.max(observed_attempts);
    timing.profile_valid = false;
    timing.classification_complete = false;
    timing.completed_at_unix_ms = None;
    let mut notice = match timing_observed_at {
        Some(observed_at) => format!(
            "Recovered partial timing includes recorded phase totals through Unix millisecond {observed_at}, plus retained sampling identities and token usage. Those totals are only the observed prefix, not the final duration. The tail after that checkpoint and process-loss time remain unknown; this is not a valid complete timing profile."
        ),
        None => String::from(
            "Recovered partial timing contains only recorded sampling identities and token usage. Duration, completion time, and unrecorded phases remain unknown; this is not a valid complete timing profile.",
        ),
    };
    notice.push_str(
        "\n\nIn flight at the last retained timing checkpoint (not a crash-time inventory):",
    );
    notice.push_str(" Logical-generation totals come only from retained checkpoints; without one they remain unknown (the zero counter is not proof of no generations). Reconstructed request counters are observed lower bounds, not complete lifetime counts.");
    let mut in_flight = 0;
    for call in &timing.tool_calls {
        let handler_running =
            call.handler_entry_at_ms.is_some() && call.handler_exit_at_ms.is_none();
        let process_running =
            call.process_spawned_at_ms.is_some() && call.process_exited_at_ms.is_none();
        if handler_running || process_running {
            in_flight += 1;
            notice.push('\n');
            notice.push_str(
                &serde_json::json!({
                    "call_id": call.call_id, "tool": call.tool_name,
                    "parent_call_id": call.parent_call_id, "cell_id": call.parent_cell_id,
                    "handler_in_flight": handler_running, "process_in_flight": process_running,
                })
                .to_string(),
            );
        }
    }
    if in_flight == 0 {
        notice.push_str(" none recorded; this is not proof that no work was running.");
    }
    notice.push_str("\nCheckpoints are written at model requests. Calls started after the last checkpoint, including the final step, may be absent; listed calls may have finished afterward. Reuse retained results before repeating work.");
    append_retained_work(&mut notice, checkpoint, plan.as_deref());
    (notice, timing)
}

/// Rebuilds the arrays that incremental checkpoints omit: earlier entries
/// remain, and an entry recorded again because it changed (for example, a
/// request that has since completed) replaces its earlier version.
fn merge_checkpoint_entries(timing: &mut TurnTiming, previous: TurnTiming) {
    timing.counters.logical_generation_count = timing
        .counters
        .logical_generation_count
        .max(previous.counters.logical_generation_count);
    timing.counters.model_request_count = timing
        .counters
        .model_request_count
        .max(previous.counters.model_request_count);
    timing.model_requests = merge_by_identity(
        previous.model_requests,
        std::mem::take(&mut timing.model_requests),
        |request| {
            (
                request.sampling_request_id.clone(),
                request.generation_index,
            )
        },
    );
    timing.tool_calls = merge_by_identity(
        previous.tool_calls,
        std::mem::take(&mut timing.tool_calls),
        // Detailed legacy checkpoints can reuse a wire ID in a later
        // generation. Keep those executions distinct while preserving the
        // wire-ID fallback for older rows without an execution identity.
        |call| (call.call_id.clone(), call.execution_id.clone()),
    );
    timing.deterministic_continuation_receipts = merge_by_identity(
        previous.deterministic_continuation_receipts,
        std::mem::take(&mut timing.deterministic_continuation_receipts),
        |receipt| {
            (
                receipt.class,
                receipt.resource_identity_hash.clone(),
                receipt.state_revision.clone(),
                receipt.host_action,
            )
        },
    );
    // Intervals are turn-relative and ordered by start. An update repeats the
    // earlier checkpoint's last interval, which may have grown since; replace
    // every retained interval from that start on. A full timeline written by an
    // older checkpoint begins with the turn's first interval, so it replaces all.
    if let Some(timeline) = timing.activity_timeline.as_mut()
        && let Some(earlier) = previous.activity_timeline
    {
        let mut intervals = earlier.intervals;
        if let Some(first) = timeline.intervals.first() {
            intervals
                .truncate(intervals.partition_point(|interval| interval.start_ns < first.start_ns));
        }
        intervals.append(&mut timeline.intervals);
        timeline.intervals = intervals;
        // A blocking interval is written again when it changes (an open wait
        // later ends), so upsert by ID. Edges are append-only and written
        // exactly once, so concatenation keeps legitimate repeats. Drop
        // counters are cumulative.
        if let Some(earlier_blocking) = earlier.blocking {
            match timeline.blocking.as_mut() {
                Some(evidence) => {
                    evidence.intervals = merge_by_identity(
                        earlier_blocking.intervals,
                        std::mem::take(&mut evidence.intervals),
                        |interval| interval.id,
                    );
                    let mut dependencies = earlier_blocking.dependencies;
                    dependencies.append(&mut evidence.dependencies);
                    evidence.dependencies = dependencies;
                    evidence.dropped_intervals = evidence
                        .dropped_intervals
                        .max(earlier_blocking.dropped_intervals);
                    evidence.dropped_dependencies = evidence
                        .dropped_dependencies
                        .max(earlier_blocking.dropped_dependencies);
                }
                None => timeline.blocking = Some(earlier_blocking),
            }
        }
    }
}

fn merge_by_identity<T, K: PartialEq>(
    mut merged: Vec<T>,
    updates: Vec<T>,
    identity: impl Fn(&T) -> K,
) -> Vec<T> {
    for update in updates {
        let key = identity(&update);
        match merged.iter().position(|entry| identity(entry) == key) {
            Some(index) => merged[index] = update,
            None => merged.push(update),
        }
    }
    merged
}


pub(crate) use codex_conversation::context::TurnAborted;
use codex_conversation::context::append_retained_work;
