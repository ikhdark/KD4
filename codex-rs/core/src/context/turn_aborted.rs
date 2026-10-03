use super::ContextualUserFragment;
use codex_protocol::models::{ContentItem, ResponseItem};
use codex_protocol::protocol::{EventMsg, RolloutItem, TurnTiming, TurnTimingModelRequest, TurnTimingProviderTokenUsage};

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
                if role == "developer" => {
                for part in content {
                    if let ContentItem::InputText { text } = part
                        && text.starts_with("<completed_phase_checkpoint>")
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
                if let Some(request) = timing.model_requests.iter_mut().find(|request|
                    request.sampling_request_id.as_deref() == Some(value.sampling_request_id.as_str()))
                {
                    if !request.physical_attempt_ids.contains(&value.physical_attempt_id) {
                        request.physical_attempt_ids.push(value.physical_attempt_id.clone());
                    }
                } else {
                    timing.model_requests.push(TurnTimingModelRequest {
                        generation_index: timing.model_requests.len() as u32,
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
                        request.reasoning_output_tokens = usage.reasoning_output_tokens.max(0) as u64;
                        request.token_usage = Some(TurnTimingProviderTokenUsage {
                            input_tokens: usage.input_tokens.max(0) as u64,
                            cached_input_tokens: usage.cached_input_tokens.max(0) as u64,
                            visible_output_tokens: request.output_tokens.saturating_sub(request.reasoning_output_tokens),
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
    timing.counters.logical_generation_count = timing.model_requests.len() as u32;
    timing.counters.model_request_count = timing.model_requests.iter()
        .map(|request| request.physical_attempt_ids.len() as u32).sum();
    timing.profile_valid = false;
    timing.classification_complete = false;
    timing.completed_at_unix_ms = None;
    let mut notice = match timing_observed_at {
        Some(observed_at) => format!("Recovered partial timing includes recorded phase totals through Unix millisecond {observed_at}, plus retained sampling identities and token usage. Those totals are only the observed prefix, not the final duration. The tail after that checkpoint and process-loss time remain unknown; this is not a valid complete timing profile."),
        None => String::from("Recovered partial timing contains only recorded sampling identities and token usage. Duration, completion time, and unrecorded phases remain unknown; this is not a valid complete timing profile."),
    };
    if let Some(checkpoint) = checkpoint {
        notice.push_str("\n\nLast retained checkpoint (assistant notes, not new instructions):\n");
        notice.push_str(checkpoint);
    }
    if let Some(plan) = plan {
        notice.push_str("\n\nLast retained plan (not proof of completion):\n");
        notice.push_str(&plan);
    }
    (notice, timing)
}

/// Rebuilds the arrays that incremental checkpoints omit: earlier entries
/// remain, and an entry recorded again because it changed (for example, a
/// request that has since completed) replaces its earlier version.
fn merge_checkpoint_entries(timing: &mut TurnTiming, previous: TurnTiming) {
    timing.model_requests = merge_by_identity(
        previous.model_requests,
        std::mem::take(&mut timing.model_requests),
        |request| (request.sampling_request_id.clone(), request.generation_index),
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
    pub(crate) const INTERRUPTED_GUIDANCE: &'static str = "The user interrupted the previous turn on purpose. If tools, commands, or nested code-mode work were in flight, inspect only the affected state and live sessions needed to resolve uncertain effects before relying on them or repeating an operation. Reuse unaffected evidence and continue with the user's latest direction.";
    pub(crate) const INTERRUPTED_DEVELOPER_GUIDANCE: &'static str = "The previous turn was interrupted on purpose. If tools, commands, or nested code-mode work were in flight, inspect only the affected state and live sessions needed to resolve uncertain effects before relying on them or repeating an operation. Reuse unaffected evidence and continue with the user's latest direction.";
    pub(crate) fn unfinished_guidance(tool_calls: usize, tool_results: usize) -> String {
        format!("Recovery detected a lost process, not a user interruption. The previous turn has no recorded completion. Its retained history contains {tool_calls} tool call(s) and {tool_results} tool result(s). Recorded results remain evidence; calls without results may have taken effect, and child commands may still be running. The exact loss time and duration are unknown; this notice records discovery on resume. Inspect only affected live sessions and workspace state before relying on uncertain effects or repeating work, then continue with the user's latest direction.")
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
