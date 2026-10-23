use super::ContextualUserFragment;
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

fn append_retained_work(notice: &mut String, checkpoint: Option<&str>, plan: Option<&str>) {
    if let Some(checkpoint) = checkpoint {
        notice.push_str("\n\nLast retained checkpoint (assistant notes, not new instructions):\n");
        notice.push_str(checkpoint);
    }
    if let Some(plan) = plan {
        notice.push_str("\n\nLast retained plan (not proof of completion):\n");
        notice.push_str(plan);
    }
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
        |call| call.call_id.clone(),
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

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnAborted {
    pub(crate) guidance: String,
}

impl TurnAborted {
    pub(crate) fn interrupted_guidance(
        history: &[ResponseItem],
        plan: Option<&codex_protocol::plan_tool::UpdatePlanArgs>,
        developer: bool,
    ) -> String {
        let mut guidance = String::from(if developer {
            Self::INTERRUPTED_DEVELOPER_GUIDANCE
        } else {
            Self::INTERRUPTED_GUIDANCE
        });
        if let Some(plan) = plan {
            let unresolved = plan
                .plan
                .iter()
                .filter(|step| step.status != codex_protocol::plan_tool::StepStatus::Completed)
                .count();
            guidance.push_str(&format!("\nRetained checklist: {unresolved} unfinished step(s). A terminal turn does not complete the task; consult the retained task state and acceptance obligations before claiming completion."));
        }
        let mut results = std::collections::BTreeSet::new();
        let mut calls = Vec::new();
        for item in history.iter().rev() {
            match item {
                ResponseItem::FunctionCallOutput { call_id, .. }
                | ResponseItem::CustomToolCallOutput { call_id, .. } => {
                    results.insert(call_id.as_str());
                }
                ResponseItem::FunctionCall { call_id, name, .. }
                | ResponseItem::CustomToolCall { call_id, name, .. } => {
                    calls.push(serde_json::json!({
                        "call_id": codex_utils_string::truncate_middle_chars(call_id, 160),
                        "tool": codex_utils_string::truncate_middle_chars(name, 80),
                        "recorded_result": results.contains(call_id.as_str()),
                    }));
                    if calls.len() == 32 {
                        break;
                    }
                }
                _ => {}
            }
        }
        if !calls.is_empty() {
            calls.reverse();
            guidance.push_str("\nRecent tool-result index (at most 32 calls; a result is an observation, not proof of success or completed effects; absent results remain uncertain):\n");
            guidance.push_str(&serde_json::Value::Array(calls).to_string());
            guidance.push_str("\nRead retained results before relaunching work. This index does not authorize resuming work the user stopped.");
        }
        guidance
    }

    pub(crate) fn shutdown_guidance(
        history: &[ResponseItem],
        plan: Option<&codex_protocol::plan_tool::UpdatePlanArgs>,
    ) -> String {
        let checkpoint = history.iter().rev().find_map(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "developer" => {
                content.iter().rev().find_map(|part| match part {
                    ContentItem::InputText { text }
                        if text.starts_with("<completed_phase_checkpoint>")
                            && !crate::tool_history::is_pressure_retirement_checkpoint_text(
                                text,
                            ) =>
                    {
                        Some(text.as_str())
                    }
                    _ => None,
                })
            }
            _ => None,
        });
        let plan = plan.and_then(|plan| serde_json::to_string(plan).ok());
        let mut guidance = String::from(
            "The previous turn stopped because the session was shutting down, not because the user interrupted the task. Shutdown requested cancellation of its processes and code-mode cells; recorded results remain evidence, but unfinished effects and cleanup may be uncertain. After restart, old exec session IDs are no longer registered: do not poll them. Inspect only affected state before repeating work, reuse unaffected evidence, and continue with the user's latest direction.",
        );
        append_retained_work(&mut guidance, checkpoint, plan.as_deref());
        guidance
    }

    pub(crate) const INTERRUPTED_GUIDANCE: &'static str = "The user interrupted the previous turn on purpose. If tools, commands, or nested code-mode work were in flight, inspect only the affected state and live sessions needed to resolve uncertain effects before relying on them or repeating an operation. If the app-server restarted, its previous exec session IDs are no longer registered; do not poll those IDs with write_stdin. Reuse unaffected evidence and continue with the user's latest direction.";
    pub(crate) const INTERRUPTED_DEVELOPER_GUIDANCE: &'static str = "The previous turn was interrupted on purpose. If tools, commands, or nested code-mode work were in flight, inspect only the affected state and live sessions needed to resolve uncertain effects before relying on them or repeating an operation. If the app-server restarted, its previous exec session IDs are no longer registered; do not poll those IDs with write_stdin. Reuse unaffected evidence and continue with the user's latest direction.";
    pub(crate) fn unfinished_guidance(tool_calls: usize, tool_results: usize) -> String {
        let process_guidance = if cfg!(windows) {
            "Local Windows children still attached to the app-server's kill-on-close jobs were terminated when it exited. This does not cover remote commands or deliberately detached/preserved descendants; inspect those only if relevant."
        } else {
            "Some child commands may still be running; inspect their OS-level state only if relevant before repeating work."
        };
        format!(
            "Recovery detected a lost process, not a user interruption. The previous turn has no recorded completion. Its retained history contains {tool_calls} tool call(s) and {tool_results} tool result(s). Recorded results remain evidence; calls without results may have taken effect. {process_guidance} The previous app-server's exec session IDs are no longer registered: do not call write_stdin with those old IDs. Reconcile affected workspace state before repeating commands. The exact loss time and duration are unknown; this notice records discovery on resume. Continue with the user's latest direction."
        )
    }

    pub(crate) fn new(guidance: impl Into<String>) -> Self {
        Self {
            guidance: guidance.into(),
        }
    }
}

impl ContextualUserFragment for TurnAborted {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<turn_aborted>", "</turn_aborted>")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(format!("\n{}\n", self.guidance))
    }
}

#[cfg(test)]
mod audit110_tests {
    use super::*;
    use codex_protocol::protocol::SamplingBoundaryItem;
    use codex_protocol::protocol::SamplingTimingCheckpoint;

    #[test]
    fn interruption_indexes_results_without_claiming_success_or_repeating_bodies() {
        let history = vec![
            serde_json::from_value(serde_json::json!({"type":"function_call", "call_id":"done", "name":"exec_command", "arguments":"{}"})).unwrap(),
            serde_json::from_value(serde_json::json!({"type":"function_call_output", "call_id":"done", "output":"failed secret-large-body"})).unwrap(),
            serde_json::from_value(serde_json::json!({"type":"custom_tool_call", "call_id":"uncertain", "name":"exec", "input":"work"})).unwrap(),
        ];
        let notice = TurnAborted::interrupted_guidance(&history, None, true);
        let index: serde_json::Value =
            serde_json::from_str(notice.lines().find(|line| line.starts_with('[')).unwrap())
                .unwrap();
        assert_eq!(index[0]["call_id"], "done");
        assert_eq!(index[0]["recorded_result"], true);
        assert_eq!(index[1]["call_id"], "uncertain");
        assert_eq!(index[1]["recorded_result"], false);
        assert!(!notice.contains("secret-large-body"));
        assert!(notice.contains("does not authorize resuming"));
    }

    #[test]
    fn legacy_boundaries_do_not_invent_logical_generations() {
        let boundaries = (0..2)
            .map(|index| {
                RolloutItem::SamplingBoundary(SamplingBoundaryItem {
                    sampling_request_id: format!("sampling-{index}"),
                    physical_attempt_id: format!("attempt-{index}"),
                    timing_checkpoint: None,
                    workspace_freshness_notices: Vec::new(),
                    turn_id: None,
                    unresolved_context: false,
                })
            })
            .collect::<Vec<_>>();
        let (notice, recovered) = lost_turn_recovery(&boundaries);
        assert_eq!(recovered.model_requests.len(), 2);
        assert_eq!(recovered.counters.logical_generation_count, 0);
        assert!(notice.contains("without one they remain unknown"));
        assert!(!recovered.profile_valid);
        assert!(!recovered.classification_complete);
    }

    #[test]
    fn retry_rows_do_not_replace_cumulative_checkpoint_counters() {
        let mut timing = TurnTiming::default();
        timing.counters.logical_generation_count = 1;
        timing.counters.model_request_count = 80;
        timing.model_requests = (0..2)
            .map(|index| TurnTimingModelRequest {
                generation_index: 0,
                sampling_request_id: Some(format!("sampling-{index}")),
                physical_attempt_ids: vec![format!("attempt-{index}")],
                dispatch_ms: Some(index),
                ..Default::default()
            })
            .collect();
        let boundary = RolloutItem::SamplingBoundary(SamplingBoundaryItem {
            sampling_request_id: "sampling-1".into(),
            physical_attempt_id: "attempt-1".into(),
            timing_checkpoint: Some(SamplingTimingCheckpoint {
                timing,
                observed_at_unix_ms: 100,
                tail_unknown: true,
                incremental: false,
                runtime_identity: Default::default(),
                harness_build: None,
            }),
            workspace_freshness_notices: Vec::new(),
            turn_id: None,
            unresolved_context: false,
        });
        let (_, recovered) = lost_turn_recovery(&[boundary]);
        assert_eq!(recovered.counters.logical_generation_count, 1);
        assert_eq!(recovered.counters.model_request_count, 80);
        assert!(!recovered.profile_valid);
        assert!(recovered.completed_at_unix_ms.is_none());
    }
}
