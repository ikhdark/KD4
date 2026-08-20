use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::read_turn_timing_spec::READ_TURN_TIMING_TOOL_NAME;
use crate::tools::handlers::read_turn_timing_spec::create_read_turn_timing_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::TurnTiming;
use codex_protocol::protocol::TurnTimingProviderTokenUsage;
use codex_thread_store::ReadThreadParams;
use codex_tools::JsonToolOutput;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;

const SUMMARY_SLOW_MODEL_REQUEST_LIMIT: usize = 5;
const SUMMARY_ERROR_MESSAGE_LIMIT: usize = 500;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadTurnTimingArgs {
    thread_id: String,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    detail: ReadTurnTimingDetail,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ReadTurnTimingDetail {
    #[default]
    Summary,
    Full,
}

#[derive(Clone, Debug)]
struct TerminalTurnTiming {
    turn_id: String,
    outcome: &'static str,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    time_to_first_token_ms: Option<i64>,
    error_message: Option<String>,
    abort_reason: Option<Value>,
    timing: Option<TurnTiming>,
}

pub struct ReadTurnTimingHandler;

impl ToolExecutor<ToolInvocation> for ReadTurnTimingHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_TURN_TIMING_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_read_turn_timing_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(handle_read_turn_timing(invocation))
    }
}

impl CoreToolRuntime for ReadTurnTimingHandler {}

async fn handle_read_turn_timing(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "read_turn_timing received unsupported payload".to_string(),
        ));
    };
    let args: ReadTurnTimingArgs = serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to parse read_turn_timing arguments: {err}"
        ))
    })?;
    let thread_id = ThreadId::from_string(&args.thread_id)
        .map_err(|err| FunctionCallError::RespondToModel(format!("invalid thread_id: {err}")))?;
    let thread = invocation
        .session
        .services
        .thread_store
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived: true,
            include_history: true,
        })
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to read timing for thread {}: {err}",
                args.thread_id
            ))
        })?;
    let history = thread.history.ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "thread {} did not return persisted history",
            args.thread_id
        ))
    })?;
    let terminal =
        select_terminal_turn(&history.items, args.turn_id.as_deref()).ok_or_else(|| {
            let selection = args
                .turn_id
                .as_deref()
                .map(|turn_id| format!("turn {turn_id}"))
                .unwrap_or_else(|| "a terminal turn".to_string());
            FunctionCallError::RespondToModel(format!(
                "thread {} does not contain {selection}",
                args.thread_id
            ))
        })?;
    let output = match args.detail {
        ReadTurnTimingDetail::Summary => timing_summary(&args.thread_id, &terminal),
        ReadTurnTimingDetail::Full => full_timing(&args.thread_id, terminal),
    };
    Ok(boxed_tool_output(JsonToolOutput::new(output)))
}

fn select_terminal_turn(
    items: &[RolloutItem],
    requested_turn_id: Option<&str>,
) -> Option<TerminalTurnTiming> {
    let mut latest_started_turn_id = None;
    let mut selected = None;
    let mut terminalization_by_turn = HashMap::new();

    for item in items {
        let RolloutItem::EventMsg(event) = item else {
            continue;
        };
        match event {
            EventMsg::TurnStarted(event) => {
                latest_started_turn_id = Some(event.turn_id.clone());
            }
            EventMsg::TurnComplete(event) => {
                if requested_turn_id.is_none_or(|turn_id| turn_id == event.turn_id) {
                    selected = Some(TerminalTurnTiming {
                        turn_id: event.turn_id.clone(),
                        outcome: if event.error.is_some() {
                            "failed"
                        } else {
                            "completed"
                        },
                        completed_at: event.completed_at,
                        duration_ms: event.duration_ms,
                        time_to_first_token_ms: event.time_to_first_token_ms,
                        error_message: event.error.as_ref().map(|error| error.message.clone()),
                        abort_reason: None,
                        timing: event.timing.clone(),
                    });
                }
            }
            EventMsg::TurnAborted(event) => {
                let turn_id = event
                    .turn_id
                    .clone()
                    .or_else(|| latest_started_turn_id.clone());
                if let Some(turn_id) = turn_id
                    && requested_turn_id.is_none_or(|requested| requested == turn_id.as_str())
                {
                    selected = Some(TerminalTurnTiming {
                        turn_id,
                        outcome: "aborted",
                        completed_at: event.completed_at,
                        duration_ms: event.duration_ms,
                        time_to_first_token_ms: None,
                        error_message: None,
                        abort_reason: serde_json::to_value(&event.reason).ok(),
                        timing: event.timing.clone(),
                    });
                }
            }
            EventMsg::TurnTerminalizationComplete(event) => {
                terminalization_by_turn
                    .insert(event.turn_id.clone(), event.receipt.terminalization.clone());
            }
            _ => {}
        }
    }

    if let Some(selected) = selected.as_mut()
        && let Some(timing) = selected.timing.as_mut()
        && let Some(terminalization) = terminalization_by_turn.get(&selected.turn_id)
    {
        timing.terminalization = terminalization.clone();
    }
    selected
}

fn full_timing(thread_id: &str, terminal: TerminalTurnTiming) -> Value {
    json!({
        "threadId": thread_id,
        "turnId": terminal.turn_id,
        "outcome": terminal.outcome,
        "completedAt": terminal.completed_at,
        "durationMs": terminal.duration_ms,
        "timeToFirstTokenMs": terminal.time_to_first_token_ms,
        "errorMessage": terminal.error_message,
        "abortReason": terminal.abort_reason,
        "timingAvailable": terminal.timing.is_some(),
        "timing": terminal.timing,
    })
}

fn timing_summary(thread_id: &str, terminal: &TerminalTurnTiming) -> Value {
    let timing = terminal.timing.as_ref();
    json!({
        "threadId": thread_id,
        "turnId": terminal.turn_id,
        "outcome": terminal.outcome,
        "completedAt": terminal.completed_at,
        "durationMs": terminal.duration_ms,
        "timeToFirstTokenMs": terminal.time_to_first_token_ms,
        "errorMessage": terminal.error_message.as_deref().map(truncated_error_message),
        "abortReason": terminal.abort_reason,
        "timingAvailable": timing.is_some(),
        "timing": timing.map(compact_timing),
    })
}

fn compact_timing(timing: &TurnTiming) -> Value {
    let exclusive = &timing.exclusive;
    let unions = &timing.unions;
    let local = &timing.local;
    let counters = &timing.counters;
    let terminal = &timing.terminalization;
    let mut slowest_model_requests = timing.model_requests.iter().collect::<Vec<_>>();
    slowest_model_requests.sort_by_key(|request| std::cmp::Reverse(request.model_stream_wait_ns));
    slowest_model_requests.truncate(SUMMARY_SLOW_MODEL_REQUEST_LIMIT);

    json!({
        "schemaVersion": timing.schema_version,
        "profileValid": timing.profile_valid,
        "classificationComplete": timing.classification_complete,
        "startedAtUnixMs": timing.started_at_unix_ms,
        "completedAtUnixMs": timing.completed_at_unix_ms,
        "inclusiveDurationMs": timing.inclusive_duration_ms,
        "machineDurationMs": timing.machine_duration_ms,
        "exclusiveMs": {
            "modelOnly": ns_to_ms(exclusive.model_only_ns),
            "toolOnly": ns_to_ms(exclusive.tool_only_ns),
            "modelPlusTool": ns_to_ms(exclusive.model_plus_tool_ns),
            "interactiveOnlyWait": ns_to_ms(exclusive.interactive_only_wait_ns),
            "interactivePlusMachine": ns_to_ms(exclusive.interactive_plus_machine_ns),
            "retryOnly": ns_to_ms(exclusive.retry_only_ns),
            "orchestration": ns_to_ms(exclusive.orchestration_ns),
            "standaloneWork": ns_to_ms(exclusive.standalone_work_ns),
            "finalization": ns_to_ms(exclusive.finalization_ns),
            "unclassified": ns_to_ms(exclusive.unclassified_ns),
        },
        "unionsMs": {
            "modelActive": ns_to_ms(unions.model_active_union_ns),
            "modelRequestWait": ns_to_ms(unions.model_request_wait_union_ns),
            "modelStreamWait": ns_to_ms(unions.model_stream_wait_union_ns),
            "modelStreamProcessing": ns_to_ms(unions.model_stream_processing_union_ns),
            "toolActive": ns_to_ms(unions.tool_active_union_ns),
            "interactiveWait": ns_to_ms(unions.interactive_wait_union_ns),
        },
        "localMs": {
            "preparation": ns_to_ms(local.preparation_union_ns),
            "planning": ns_to_ms(local.planning_union_ns),
            "planningExclusive": ns_to_ms(local.planning_exclusive_union_ns),
            "compaction": ns_to_ms(local.compaction_union_ns),
            "persistence": ns_to_ms(local.persistence_union_ns),
            "serialization": ns_to_ms(local.serialization_union_ns),
            "routerBuild": ns_to_ms(local.router_build_union_ns),
            "startupPrewarmWait": ns_to_ms(local.startup_prewarm_wait_union_ns),
            "executorReadinessWait": ns_to_ms(local.executor_readiness_wait_union_ns),
        },
        "milestonesMs": timing.milestones,
        "counters": {
            "logicalGenerations": counters.logical_generation_count,
            "modelRequests": counters.model_request_count,
            "modelRetries": counters.model_retry_count,
            "modelFallbacks": counters.model_fallback_count,
            "toolCalls": counters.tool_call_count,
            "approvalWaits": counters.approval_wait_count,
            "permissionWaits": counters.permission_wait_count,
            "userInputWaits": counters.user_input_wait_count,
            "internallyDrainedWaits": counters.internally_drained_wait_count,
            "exactRepeatedWaits": counters.exact_repeated_wait_count,
            "ownerDrainedContinuations": counters.owner_drained_continuation_count,
            "toolOutputTruncations": counters.tool_output_truncation_count,
            "toolOutputArtifactRereads": counters.tool_output_artifact_reread_count,
        },
        "providerTokens": aggregate_provider_tokens(timing),
        "observationalNonprogressTokens": timing.observational_nonprogress_tokens,
        "observationalNonprogressLatency": timing.observational_nonprogress_latency,
        "terminalizationMs": {
            "finalMutationToSeal": ns_to_ms(terminal.final_mutation_to_seal_ns),
            "validationProcess": ns_to_ms(terminal.validation_process_ns),
            "validationAggregate": ns_to_ms(terminal.validation_aggregate_ns),
            "completionGate": ns_to_ms(terminal.completion_gate_ns),
            "reviewPreflight": ns_to_ms(terminal.review_preflight_ns),
            "review": ns_to_ms(terminal.review_ns),
            "durableCommit": ns_to_ms(terminal.durable_commit_ns),
            "deliveryAttempt": ns_to_ms(terminal.delivery_attempt_ns),
            "interactionRelease": ns_to_ms(terminal.interaction_release_ns),
            "postCleanup": ns_to_ms(terminal.post_cleanup_ns),
            "unclassified": ns_to_ms(terminal.unclassified_ns),
        },
        "slowestModelRequests": slowest_model_requests.into_iter().map(|request| json!({
            "generationIndex": request.generation_index,
            "generationReason": request.generation_reason,
            "generationPurpose": request.generation_purpose,
            "disposition": request.disposition,
            "attemptKind": request.attempt_kind,
            "isContinuation": request.is_continuation,
            "modelStreamWaitMs": ns_to_ms(request.model_stream_wait_ns),
            "decisionLatencyMs": request.decision_latency_ns.map(ns_to_ms),
            "toolCalls": request.tool_call_count,
            "toolActiveMs": ns_to_ms(request.tool_active_union_ns),
            "tokenUsage": request.token_usage,
            "dispatchMs": request.dispatch_ms,
            "firstActionableOutputMs": request.first_actionable_output_ms,
            "completedMs": request.completed_ms,
        })).collect::<Vec<_>>(),
    })
}

fn aggregate_provider_tokens(timing: &TurnTiming) -> TurnTimingProviderTokenUsage {
    timing
        .model_requests
        .iter()
        .filter_map(|request| request.token_usage.as_ref())
        .fold(
            TurnTimingProviderTokenUsage::default(),
            |mut total, usage| {
                total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
                total.cached_input_tokens = total
                    .cached_input_tokens
                    .saturating_add(usage.cached_input_tokens);
                total.visible_output_tokens = total
                    .visible_output_tokens
                    .saturating_add(usage.visible_output_tokens);
                total.reasoning_tokens = total
                    .reasoning_tokens
                    .saturating_add(usage.reasoning_tokens);
                total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
                total
            },
        )
}

fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

fn truncated_error_message(message: &str) -> String {
    let mut chars = message.chars();
    let truncated = chars
        .by_ref()
        .take(SUMMARY_ERROR_MESSAGE_LIMIT)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::protocol::TerminalizationDeliveryState;
    use codex_protocol::protocol::TerminalizationRecoveryState;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_protocol::protocol::TurnStartedEvent;
    use codex_protocol::protocol::TurnTerminalizationCompleteEvent;
    use codex_protocol::protocol::TurnTerminalizationReceipt;
    use codex_protocol::protocol::TurnTimingModelRequest;
    use codex_protocol::protocol::TurnTimingTerminalization;

    fn completed(turn_id: &str, timing: TurnTiming) -> RolloutItem {
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: turn_id.to_string(),
            last_agent_message: None,
            surfaced_result: None,
            error: None,
            completion: None,
            completed_at: Some(7),
            duration_ms: Some(timing.inclusive_duration_ms as i64),
            time_to_first_token_ms: Some(3),
            timing: Some(timing),
        }))
    }

    #[test]
    fn latest_terminal_turn_uses_authoritative_late_terminalization() {
        let first = TurnTiming {
            inclusive_duration_ms: 10,
            ..TurnTiming::default()
        };
        let second = TurnTiming {
            inclusive_duration_ms: 20,
            ..TurnTiming::default()
        };
        let authoritative_terminalization = TurnTimingTerminalization {
            delivery_attempt_ns: 42_000_000,
            ..TurnTimingTerminalization::default()
        };
        let items = vec![
            completed("first", first),
            RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "second".to_string(),
                trace_id: None,
                started_at: Some(1),
                model_context_window: None,
                collaboration_mode_kind: ModeKind::Default,
            })),
            completed("second", second),
            RolloutItem::EventMsg(EventMsg::TurnTerminalizationComplete(
                TurnTerminalizationCompleteEvent {
                    turn_id: "second".to_string(),
                    receipt: TurnTerminalizationReceipt {
                        terminal_identity: "terminal".to_string(),
                        terminalization: authoritative_terminalization.clone(),
                        delivery_state: TerminalizationDeliveryState::Delivered,
                        active_turn_detached: true,
                        terminal_interaction_released: true,
                        recovery_state: TerminalizationRecoveryState::None,
                        deadline_exhausted_phase: None,
                    },
                },
            )),
        ];

        let selected = select_terminal_turn(&items, None).expect("latest terminal turn");
        assert_eq!(selected.turn_id, "second");
        assert_eq!(
            selected.timing.unwrap().terminalization,
            authoritative_terminalization
        );
    }

    #[test]
    fn summary_is_bounded_even_with_many_model_requests() {
        let model_requests = (0..100)
            .map(|generation_index| TurnTimingModelRequest {
                generation_index,
                model_stream_wait_ns: u64::MAX - u64::from(generation_index),
                token_usage: Some(TurnTimingProviderTokenUsage {
                    input_tokens: 1,
                    cached_input_tokens: 1,
                    visible_output_tokens: 1,
                    reasoning_tokens: 1,
                    total_tokens: 4,
                }),
                ..TurnTimingModelRequest::default()
            })
            .collect();
        let terminal = TerminalTurnTiming {
            turn_id: "turn".to_string(),
            outcome: "failed",
            completed_at: Some(i64::MAX),
            duration_ms: Some(i64::MAX),
            time_to_first_token_ms: Some(i64::MAX),
            error_message: Some("x".repeat(10_000)),
            abort_reason: None,
            timing: Some(TurnTiming {
                model_requests,
                ..TurnTiming::default()
            }),
        };

        let summary = timing_summary("thread", &terminal);
        let encoded = serde_json::to_string(&summary).expect("serialize summary");
        assert!(
            encoded.len() / 4 < 2_000,
            "summary was {} estimated tokens",
            encoded.len() / 4
        );
        assert_eq!(
            summary
                .pointer("/timing/slowestModelRequests")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(SUMMARY_SLOW_MODEL_REQUEST_LIMIT)
        );
        assert_eq!(
            summary.pointer("/timing/providerTokens/totalTokens"),
            Some(&json!(400))
        );
    }

    #[test]
    fn full_detail_preserves_the_complete_timing_profile() {
        let timing = TurnTiming {
            schema_version: 9,
            inclusive_duration_ns: 123,
            model_requests: vec![TurnTimingModelRequest {
                generation_index: 4,
                ..TurnTimingModelRequest::default()
            }],
            ..TurnTiming::default()
        };
        let terminal = TerminalTurnTiming {
            turn_id: "turn".to_string(),
            outcome: "completed",
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
            error_message: None,
            abort_reason: None,
            timing: Some(timing.clone()),
        };

        assert_eq!(
            full_timing("thread", terminal).get("timing"),
            Some(&serde_json::to_value(timing).expect("serialize timing"))
        );
    }
}
