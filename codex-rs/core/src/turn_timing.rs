use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_analytics::TurnProfile;
use codex_otel::TURN_TTFM_DURATION_METRIC;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnTiming;
use codex_protocol::protocol::TurnTimingAttemptKind;
use codex_protocol::protocol::TurnTimingAttemptKindCounts;
use codex_protocol::protocol::TurnTimingCounters;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_protocol::protocol::TurnTimingDiagnosticLatencyAggregate;
use codex_protocol::protocol::TurnTimingDiagnosticTokenAggregate;
use codex_protocol::protocol::TurnTimingExclusive;
use codex_protocol::protocol::TurnTimingGenerationDisposition;
use codex_protocol::protocol::TurnTimingGenerationDispositionCounts;
use codex_protocol::protocol::TurnTimingGenerationPurpose;
use codex_protocol::protocol::TurnTimingGenerationPurposeAggregate;
use codex_protocol::protocol::TurnTimingGenerationPurposeCounts;
use codex_protocol::protocol::TurnTimingGenerationReason;
use codex_protocol::protocol::TurnTimingGenerationReasonCounts;
use codex_protocol::protocol::TurnTimingLocal;
use codex_protocol::protocol::TurnTimingMilestones;
use codex_protocol::protocol::TurnTimingModelRequest;
use codex_protocol::protocol::TurnTimingPreFirstModelOutput;
use codex_protocol::protocol::TurnTimingProgressKind;
use codex_protocol::protocol::TurnTimingProviderTokenUsage;
use codex_protocol::protocol::TurnTimingRequestTokenCategories;
use codex_protocol::protocol::TurnTimingTerminalization;
use codex_protocol::protocol::TurnTimingUnions;

use crate::ResponseEvent;
use crate::session::turn_context::TurnContext;
use crate::stream_events_utils::raw_assistant_output_text_from_item;

const NANOS_PER_MILLISECOND: u128 = 1_000_000;
const TIMING_SCHEMA_VERSION: u16 = 20;
const MAX_DETERMINISTIC_CONTINUATION_RECEIPTS: usize = 64;

/// Control-only calls can advance orchestration without beginning the user's
/// requested work. Keep them observable in dispatch milestones and counters,
/// but do not let them satisfy the first-useful-action contract.
pub(crate) fn tool_counts_as_useful_first_action(tool_name: &str) -> bool {
    !matches!(
        tool_name,
        "update_plan"
            | "request_user_input"
            | "request_permissions"
            | "wait"
            | "wait_agent"
            | "wait_for_environment"
            | "write_stdin"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContinuationCause {
    Compaction,
    ToolResult,
    ServerEndTurnFalse,
    PendingInput,
    StopHook,
    CompletionReviewRepair,
    InvalidImageRecovery,
}

pub(crate) async fn record_turn_ttft_metric(turn_context: &TurnContext, event: &ResponseEvent) {
    let Some(duration) = turn_context
        .turn_timing_state
        .record_response_event_milestones(event)
    else {
        return;
    };
    turn_context.session_telemetry.record_turn_ttft(duration);
}

pub(crate) async fn record_turn_ttfm_metric(turn_context: &TurnContext, item: &TurnItem) {
    let Some(duration) = turn_context
        .turn_timing_state
        .record_ttfm_for_turn_item(item)
    else {
        return;
    };
    turn_context
        .session_telemetry
        .record_duration(TURN_TTFM_DURATION_METRIC, duration, &[]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TimeSample {
    pub(crate) monotonic_ns: u128,
    pub(crate) wall_unix_ms: i64,
}

#[derive(Clone, Copy, Debug)]
struct ClockSample {
    time: TimeSample,
}

trait TurnClock: Send + Sync {
    fn sample(&self) -> ClockSample;
}

#[derive(Debug)]
struct SystemTurnClock {
    origin: Instant,
}

impl Default for SystemTurnClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl TurnClock for SystemTurnClock {
    fn sample(&self) -> ClockSample {
        let now = Instant::now();
        ClockSample {
            time: TimeSample {
                monotonic_ns: now.saturating_duration_since(self.origin).as_nanos(),
                wall_unix_ms: now_unix_timestamp_ms(),
            },
        }
    }
}

pub(crate) struct TurnTimingState {
    clock: Arc<dyn TurnClock>,
    state: StdMutex<TurnTimingStateInner>,
}

impl Default for TurnTimingState {
    fn default() -> Self {
        Self::new(Arc::new(SystemTurnClock::default()))
    }
}

impl std::fmt::Debug for TurnTimingState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnTimingState")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TurnTimingSnapshot {
    pub(crate) started_at_unix_ms: Option<i64>,
    pub(crate) completed_at_unix_ms: Option<i64>,
    pub(crate) completed_at_unix_secs: Option<i64>,
    pub(crate) duration_ms: Option<i64>,
    pub(crate) time_to_first_token_ms: Option<i64>,
    pub(crate) legacy_profile: TurnProfile,
    pub(crate) profile: TurnTimingProfile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelAttemptGenerationMetadata {
    pub(crate) generation_index: u32,
    pub(crate) generation_purpose: Option<TurnTimingGenerationPurpose>,
    pub(crate) disposition: TurnTimingGenerationDisposition,
    pub(crate) relevant_state_fingerprint: Option<String>,
}

impl ModelAttemptGenerationMetadata {
    pub(crate) fn purpose_label(&self) -> Option<&'static str> {
        self.generation_purpose.map(|purpose| match purpose {
            TurnTimingGenerationPurpose::InitialReasoning => "initial",
            TurnTimingGenerationPurpose::ImplementationDecision => "implementation",
            TurnTimingGenerationPurpose::Wait => "wait",
            TurnTimingGenerationPurpose::FailureDiagnosis => "failure_diagnosis",
            TurnTimingGenerationPurpose::ValidationInterpretation => "validation_interpretation",
            TurnTimingGenerationPurpose::Repair => "repair",
            TurnTimingGenerationPurpose::Coordination => "agent_coordination",
            TurnTimingGenerationPurpose::ArtifactContinuation => "deterministic_tool_continuation",
            TurnTimingGenerationPurpose::CompactionRecovery => "compaction_recovery",
            TurnTimingGenerationPurpose::TerminalCompletionReasoning => "terminal",
        })
    }

    pub(crate) fn disposition_label(&self) -> &'static str {
        match self.disposition {
            TurnTimingGenerationDisposition::Unknown => "unknown",
            TurnTimingGenerationDisposition::DecisionBearing => "decision_bearing",
            TurnTimingGenerationDisposition::Deterministic => "deterministic",
        }
    }
}

impl TurnTimingSnapshot {
    pub(crate) fn inclusive_duration(&self) -> Option<Duration> {
        self.profile
            .started
            .then(|| duration_from_nanos(self.profile.inclusive_duration_ns))
    }

    pub(crate) fn protocol_timing(&self) -> TurnTiming {
        let profile = &self.profile;
        let mut saturation_count = profile.counters.saturation_count;

        let inclusive_duration_ns = public_ns(profile.inclusive_duration_ns, &mut saturation_count);
        let inclusive_duration_ms = public_ms(profile.inclusive_duration_ns, &mut saturation_count);
        let machine_duration_ns = public_ns(profile.machine_duration_ns, &mut saturation_count);
        let machine_duration_ms = public_ms(profile.machine_duration_ns, &mut saturation_count);
        let exclusive = TurnTimingExclusive {
            model_only_ns: public_ns(profile.exclusive.model_only_ns, &mut saturation_count),
            tool_only_ns: public_ns(profile.exclusive.tool_only_ns, &mut saturation_count),
            model_plus_tool_ns: public_ns(
                profile.exclusive.model_tool_overlap_ns,
                &mut saturation_count,
            ),
            interactive_only_wait_ns: public_ns(
                profile.exclusive.interactive_only_wait_ns,
                &mut saturation_count,
            ),
            interactive_plus_machine_ns: public_ns(
                profile.exclusive.interactive_machine_overlap_ns,
                &mut saturation_count,
            ),
            retry_only_ns: public_ns(profile.exclusive.retry_only_ns, &mut saturation_count),
            orchestration_ns: public_ns(profile.exclusive.orchestration_ns, &mut saturation_count),
            standalone_work_ns: public_ns(
                profile.exclusive.standalone_work_ns,
                &mut saturation_count,
            ),
            finalization_ns: public_ns(profile.exclusive.finalization_ns, &mut saturation_count),
            unclassified_ns: public_ns(profile.exclusive.unclassified_ns, &mut saturation_count),
        };
        let unions = TurnTimingUnions {
            model_active_union_ns: public_ns(profile.unions.model_active_ns, &mut saturation_count),
            model_active_union_ms: public_ms(profile.unions.model_active_ns, &mut saturation_count),
            model_request_wait_union_ns: public_ns(
                profile.unions.model_request_wait_ns,
                &mut saturation_count,
            ),
            model_stream_wait_union_ns: public_ns(
                profile.unions.model_stream_wait_ns,
                &mut saturation_count,
            ),
            model_stream_processing_union_ns: public_ns(
                profile.unions.model_stream_processing_ns,
                &mut saturation_count,
            ),
            tool_active_union_ns: public_ns(profile.unions.tool_active_ns, &mut saturation_count),
            tool_active_union_ms: public_ms(profile.unions.tool_active_ns, &mut saturation_count),
            interactive_wait_union_ns: public_ns(
                profile.unions.interactive_wait_ns,
                &mut saturation_count,
            ),
        };
        let local = TurnTimingLocal {
            preparation_union_ns: public_ns(profile.local.preparation_ns, &mut saturation_count),
            planning_union_ns: public_ns(profile.local.planning_ns, &mut saturation_count),
            planning_exclusive_union_ns: public_ns(
                profile
                    .local
                    .planning_ns
                    .saturating_sub(profile.local.planning_compaction_overlap_ns),
                &mut saturation_count,
            ),
            planning_compaction_overlap_union_ns: public_ns(
                profile.local.planning_compaction_overlap_ns,
                &mut saturation_count,
            ),
            compaction_union_ns: public_ns(profile.local.compaction_ns, &mut saturation_count),
            persistence_union_ns: public_ns(profile.local.persistence_ns, &mut saturation_count),
            serialization_union_ns: public_ns(
                profile.local.serialization_ns,
                &mut saturation_count,
            ),
            router_build_union_ns: public_ns(profile.local.router_build_ns, &mut saturation_count),
            startup_prewarm_wait_union_ns: public_ns(
                profile.local.startup_prewarm_wait_ns,
                &mut saturation_count,
            ),
            executor_readiness_wait_union_ns: public_ns(
                profile.local.executor_readiness_wait_ns,
                &mut saturation_count,
            ),
        };
        let milestones = TurnTimingMilestones {
            user_input_recorded_ms: profile
                .milestones
                .user_input_recorded_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_tool_accepted_ms: profile
                .milestones
                .first_tool_accepted_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_tool_gate_admitted_ms: profile
                .milestones
                .first_tool_gate_admitted_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_tool_handler_entry_ms: profile
                .milestones
                .first_tool_handler_entry_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_useful_tool_accepted_ms: profile
                .milestones
                .first_useful_tool_accepted_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_useful_tool_gate_admitted_ms: profile
                .milestones
                .first_useful_tool_gate_admitted_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_useful_action_ms: profile
                .milestones
                .first_useful_action_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_successful_useful_action_ms: profile
                .milestones
                .first_successful_useful_action_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_model_output_ms: profile
                .milestones
                .first_model_output_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_actionable_output_ms: profile
                .milestones
                .first_actionable_output_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_visible_output_ms: profile
                .milestones
                .first_visible_output_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_agent_message_ms: profile
                .milestones
                .first_agent_message_ns
                .map(|value| public_ms(value, &mut saturation_count)),
        };
        let pre_first_model_output =
            profile
                .pre_first_model_output
                .as_ref()
                .map(|timing| TurnTimingPreFirstModelOutput {
                    captured_at_ns: public_ns(timing.captured_at_ns, &mut saturation_count),
                    first_request_dispatch_ready_ns: public_ns(
                        timing.first_request_dispatch_ready_ns,
                        &mut saturation_count,
                    ),
                    client_critical_path_ns: public_ns(
                        timing.client_critical_path_ns,
                        &mut saturation_count,
                    ),
                    attributed_client_union_ns: public_ns(
                        timing.attributed_client_union_ns,
                        &mut saturation_count,
                    ),
                    unattributed_pre_output_ns: public_ns(
                        timing.unattributed_pre_output_ns,
                        &mut saturation_count,
                    ),
                    history_snapshot_ns: public_ns(
                        timing.history_snapshot_ns,
                        &mut saturation_count,
                    ),
                    normalization_ns: public_ns(timing.normalization_ns, &mut saturation_count),
                    prompt_construction_ns: public_ns(
                        timing.prompt_construction_ns,
                        &mut saturation_count,
                    ),
                    request_transformation_ns: public_ns(
                        timing.request_transformation_ns,
                        &mut saturation_count,
                    ),
                    serialization_ns: public_ns(timing.serialization_ns, &mut saturation_count),
                    transport_readiness_ns: public_ns(
                        timing.transport_readiness_ns,
                        &mut saturation_count,
                    ),
                });
        let model_requests = profile
            .model_requests
            .iter()
            .map(|request| {
                let request_token_categories =
                    request.request_token_categories.as_ref().map(|categories| {
                        let mut categories = categories.clone();
                        if let Some(usage) = request.token_usage.as_ref() {
                            categories.provider_input_tokens = Some(usage.input_tokens);
                            categories.provider_reconciliation_residual = Some(signed_difference(
                                usage.input_tokens,
                                categories.logical_total,
                            ));
                        }
                        categories
                    });
                TurnTimingModelRequest {
                    generation_index: request.generation_index,
                    generation_reason: request.generation_reason,
                    generation_purpose: request.generation_purpose,
                    disposition: request.disposition,
                    relevant_state_fingerprint: request.relevant_state_fingerprint.clone(),
                    sampling_request_id: request.sampling_request_id.clone(),
                    physical_attempt_ids: request.physical_attempt_ids.clone(),
                    progress_kinds: request.progress_kinds.clone(),
                    next_structured_action_changed: request.next_structured_action_changed,
                    unchanged_relevant_state: request.unchanged_relevant_state,
                    attempt_kind: request.attempt_kind,
                    is_continuation: request.is_continuation,
                    model_stream_wait_ns: public_ns(
                        request.model_stream_wait_ns,
                        &mut saturation_count,
                    ),
                    decision_latency_ns: request
                        .decision_latency_ns()
                        .map(|value| public_ns(value, &mut saturation_count)),
                    tool_call_count: request.tool_call_count,
                    tool_active_union_ns: public_ns(
                        request.tool_active_union_ns,
                        &mut saturation_count,
                    ),
                    output_tokens: request.output_tokens,
                    reasoning_output_tokens: request.reasoning_output_tokens,
                    token_usage: request.token_usage.clone(),
                    request_token_categories,
                    dispatch_ms: request
                        .dispatch_ns
                        .map(|value| public_ms(value, &mut saturation_count)),
                    first_model_output_ms: request
                        .first_model_output_ns
                        .map(|value| public_ms(value, &mut saturation_count)),
                    first_actionable_output_ms: request
                        .first_actionable_output_ns
                        .map(|value| public_ms(value, &mut saturation_count)),
                    completed_ms: request
                        .completed_ns
                        .map(|value| public_ms(value, &mut saturation_count)),
                }
            })
            .collect();
        let observational_nonprogress_tokens = diagnostic_token_aggregate(
            &profile.model_requests,
            |request| request.unchanged_relevant_state && !request.next_structured_action_changed,
            /*input_only*/ false,
        );
        let observational_nonprogress_latency = diagnostic_latency_aggregate(
            &profile.model_requests,
            |request| request.unchanged_relevant_state && !request.next_structured_action_changed,
            &mut saturation_count,
        );
        let purpose_aggregates = purpose_aggregates(&profile.model_requests, &mut saturation_count);
        let exact_repeated_wait_count = exact_repeated_wait_count(&profile.model_requests);
        let failure_diagnosis_count = primary_generation_count(
            &profile.model_requests,
            TurnTimingGenerationPurpose::FailureDiagnosis,
        );
        let failure_signature_count = unique_primary_failure_signature_count(
            &profile.model_requests,
            TurnTimingGenerationPurpose::FailureDiagnosis,
        );
        let profile_valid =
            profile.profile_valid && saturation_count == profile.counters.saturation_count;
        let counters = TurnTimingCounters {
            logical_generation_count: profile.counters.logical_generation_count,
            generations_by_reason: profile.counters.generations_by_reason.clone(),
            generations_by_purpose: profile.counters.generations_by_purpose.clone(),
            generations_by_disposition: profile.counters.generations_by_disposition.clone(),
            suppressed_deterministic_continuation_count: profile
                .counters
                .suppressed_deterministic_continuation_count,
            residual_deterministic_generation_count: profile
                .counters
                .residual_deterministic_generation_count,
            owner_drained_continuation_count: profile.counters.owner_drained_continuation_count,
            executed_validation_count: profile.counters.executed_validation_count,
            reused_validation_count: profile.counters.reused_validation_count,
            duplicate_validation_count: profile.counters.duplicate_validation_count,
            forced_fresh_validation_count: profile.counters.forced_fresh_validation_count,
            executed_validation_duration_ns: profile.counters.executed_validation_duration_ns,
            suppressed_validation_output_count: profile.counters.suppressed_validation_output_count,
            ready_startup_prewarm_count: profile.counters.ready_startup_prewarm_count,
            completion_review_ready_phase_count: profile
                .counters
                .completion_review_ready_phase_count,
            completion_review_terminal_phase_count: profile
                .counters
                .completion_review_terminal_phase_count,
            purpose_aggregates,
            same_purpose_continuation_count: profile.counters.same_purpose_continuation_count,
            exact_repeated_wait_count,
            planning_generation_count: profile.counters.planning_generation_count,
            plan_revision_generation_count: profile.counters.plan_revision_generation_count,
            planning_fixed_point_iteration_count: profile
                .counters
                .planning_fixed_point_iteration_count,
            planning_invalidation_count: profile.counters.planning_invalidation_count,
            planning_semantic_effect_count: profile.counters.planning_semantic_effect_count,
            planning_failure_count: profile.counters.planning_failure_count,
            failure_signature_count,
            failure_diagnosis_count,
            attempts_by_kind: profile.counters.attempts_by_kind.clone(),
            model_request_count: profile.counters.model_request_count,
            model_retry_count: profile.counters.model_retry_count,
            model_fallback_count: profile.counters.model_fallback_count,
            tool_call_count: profile.counters.tool_call_count,
            approval_wait_count: profile.counters.approval_wait_count,
            permission_wait_count: profile.counters.permission_wait_count,
            user_input_wait_count: profile.counters.user_input_wait_count,
            mcp_elicitation_wait_count: profile.counters.mcp_elicitation_wait_count,
            wait_only_generation_count: profile.counters.wait_only_generation_count,
            internally_drained_wait_count: profile.counters.internally_drained_wait_count,
            no_progress_directive_count: profile.counters.no_progress_directive_count,
            proven_loop_activation_count: profile.counters.proven_loop_activation_count,
            tool_output_truncation_count: profile.counters.tool_output_truncation_count,
            tool_output_projected_token_count: profile.counters.tool_output_projected_token_count,
            tool_output_artifact_reread_count: profile.counters.tool_output_artifact_reread_count,
            tool_output_canonical_byte_count: profile.counters.tool_output_canonical_byte_count,
            tool_output_canonical_token_count: profile.counters.tool_output_canonical_token_count,
            tool_output_model_byte_count: profile.counters.tool_output_model_byte_count,
            tool_output_model_token_count: profile.counters.tool_output_model_token_count,
            tool_output_artifact_creation_count: profile
                .counters
                .tool_output_artifact_creation_count,
            tool_output_projection_truncation_count: profile
                .counters
                .tool_output_projection_truncation_count,
            tool_output_omitted_section_count: profile.counters.tool_output_omitted_section_count,
            tool_output_recovery_call_count: profile.counters.tool_output_recovery_call_count,
            tool_output_recovery_retruncation_count: profile
                .counters
                .tool_output_recovery_retruncation_count,
            tool_output_recursive_spill_count: profile.counters.tool_output_recursive_spill_count,
            attributable_recovery_generation_count: profile
                .counters
                .attributable_recovery_generation_count,
            truncation_induced_continuation_count: profile
                .counters
                .truncation_induced_continuation_count,
            invalid_transition_count: profile.counters.invalid_transition_count,
            clock_regression_count: profile.counters.clock_regression_count,
            saturation_count,
        };

        TurnTiming {
            schema_version: profile.schema_version,
            profile_valid,
            classification_complete: profile.classification_complete,
            started_at_unix_ms: self.started_at_unix_ms,
            completed_at_unix_ms: self.completed_at_unix_ms,
            inclusive_duration_ns,
            inclusive_duration_ms,
            machine_duration_ns,
            machine_duration_ms,
            exclusive,
            unions,
            local,
            milestones,
            counters,
            terminalization: profile.terminalization.clone(),
            model_requests,
            observational_nonprogress_tokens,
            observational_nonprogress_latency,
            deterministic_continuation_receipts: profile
                .deterministic_continuation_receipts
                .clone(),
            deterministic_continuation_receipt_overflow: profile
                .deterministic_continuation_receipt_overflow,
            pre_first_model_output,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TurnTimingProfile {
    pub(crate) schema_version: u16,
    pub(crate) started: bool,
    pub(crate) profile_valid: bool,
    pub(crate) classification_complete: bool,
    pub(crate) inclusive_duration_ns: u128,
    pub(crate) machine_duration_ns: u128,
    pub(crate) exclusive: ExclusiveTiming,
    pub(crate) unions: TimingUnions,
    /// Named local phases are union durations and may intentionally overlap.
    /// The exclusive ledger above remains the canonical wall-clock partition.
    pub(crate) local: LocalTiming,
    pub(crate) milestones: TimingMilestones,
    pub(crate) counters: TimingCounters,
    pub(crate) model_requests: Vec<ModelRequestTiming>,
    pub(crate) deterministic_continuation_receipts: Vec<TurnTimingDeterministicContinuationReceipt>,
    pub(crate) deterministic_continuation_receipt_overflow: u32,
    pub(crate) pre_first_model_output: Option<PreFirstModelOutputTiming>,
    pub(crate) terminalization: TurnTimingTerminalization,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ModelRequestTiming {
    generation_index: u32,
    generation_reason: TurnTimingGenerationReason,
    generation_purpose: Option<TurnTimingGenerationPurpose>,
    disposition: TurnTimingGenerationDisposition,
    relevant_state_fingerprint: Option<String>,
    failure_fingerprint: Option<String>,
    sampling_request_id: Option<String>,
    physical_attempt_ids: Vec<String>,
    progress_kinds: Vec<TurnTimingProgressKind>,
    next_structured_action_changed: bool,
    unchanged_relevant_state: bool,
    attempt_kind: TurnTimingAttemptKind,
    model_stream_wait_ns: u128,
    tool_active_union_ns: u128,
    tool_call_count: u32,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    token_usage: Option<TurnTimingProviderTokenUsage>,
    request_token_categories: Option<TurnTimingRequestTokenCategories>,
    dispatch_ns: Option<u128>,
    first_model_output_ns: Option<u128>,
    first_actionable_output_ns: Option<u128>,
    completed_ns: Option<u128>,
    is_continuation: bool,
}

impl ModelRequestTiming {
    fn decision_latency_ns(&self) -> Option<u128> {
        Some(
            self.first_actionable_output_ns?
                .saturating_sub(self.dispatch_ns?),
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PreFirstModelOutputTiming {
    captured_at_ns: u128,
    first_request_dispatch_ready_ns: u128,
    client_critical_path_ns: u128,
    attributed_client_union_ns: u128,
    unattributed_pre_output_ns: u128,
    history_snapshot_ns: u128,
    normalization_ns: u128,
    prompt_construction_ns: u128,
    request_transformation_ns: u128,
    serialization_ns: u128,
    transport_readiness_ns: u128,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ClientCriticalPhaseTiming {
    history_snapshot_ns: u128,
    normalization_ns: u128,
    prompt_construction_ns: u128,
    request_transformation_ns: u128,
    serialization_ns: u128,
    transport_readiness_ns: u128,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExclusiveTiming {
    pub(crate) model_only_ns: u128,
    pub(crate) tool_only_ns: u128,
    pub(crate) model_tool_overlap_ns: u128,
    pub(crate) interactive_only_wait_ns: u128,
    pub(crate) interactive_machine_overlap_ns: u128,
    pub(crate) retry_only_ns: u128,
    pub(crate) orchestration_ns: u128,
    pub(crate) standalone_work_ns: u128,
    pub(crate) finalization_ns: u128,
    pub(crate) unclassified_ns: u128,
}

impl ExclusiveTiming {
    fn total_ns(&self) -> u128 {
        self.model_only_ns
            .saturating_add(self.tool_only_ns)
            .saturating_add(self.model_tool_overlap_ns)
            .saturating_add(self.interactive_only_wait_ns)
            .saturating_add(self.interactive_machine_overlap_ns)
            .saturating_add(self.retry_only_ns)
            .saturating_add(self.orchestration_ns)
            .saturating_add(self.standalone_work_ns)
            .saturating_add(self.finalization_ns)
            .saturating_add(self.unclassified_ns)
    }

    fn add(&mut self, phase: ExclusivePhase, elapsed_ns: u128) -> bool {
        let target = match phase {
            ExclusivePhase::ModelOnly => &mut self.model_only_ns,
            ExclusivePhase::ToolOnly => &mut self.tool_only_ns,
            ExclusivePhase::ModelToolOverlap => &mut self.model_tool_overlap_ns,
            ExclusivePhase::InteractiveOnly => &mut self.interactive_only_wait_ns,
            ExclusivePhase::InteractiveMachineOverlap => &mut self.interactive_machine_overlap_ns,
            ExclusivePhase::RetryOnly => &mut self.retry_only_ns,
            ExclusivePhase::Orchestration => &mut self.orchestration_ns,
            ExclusivePhase::StandaloneWork => &mut self.standalone_work_ns,
            ExclusivePhase::Finalization => &mut self.finalization_ns,
            ExclusivePhase::Unclassified => &mut self.unclassified_ns,
        };
        let saturated = target.checked_add(elapsed_ns).is_none();
        *target = target.saturating_add(elapsed_ns);
        saturated
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TimingUnions {
    pub(crate) model_active_ns: u128,
    pub(crate) model_request_wait_ns: u128,
    pub(crate) model_stream_wait_ns: u128,
    pub(crate) model_stream_processing_ns: u128,
    pub(crate) tool_active_ns: u128,
    pub(crate) interactive_wait_ns: u128,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LocalTiming {
    pub(crate) preparation_ns: u128,
    pub(crate) planning_ns: u128,
    pub(crate) planning_compaction_overlap_ns: u128,
    pub(crate) compaction_ns: u128,
    pub(crate) persistence_ns: u128,
    pub(crate) serialization_ns: u128,
    pub(crate) router_build_ns: u128,
    pub(crate) startup_prewarm_wait_ns: u128,
    pub(crate) executor_readiness_wait_ns: u128,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TimingMilestones {
    pub(crate) user_input_recorded_ns: Option<u128>,
    pub(crate) first_tool_accepted_ns: Option<u128>,
    pub(crate) first_tool_gate_admitted_ns: Option<u128>,
    pub(crate) first_tool_handler_entry_ns: Option<u128>,
    pub(crate) first_useful_tool_accepted_ns: Option<u128>,
    pub(crate) first_useful_tool_gate_admitted_ns: Option<u128>,
    pub(crate) first_useful_action_ns: Option<u128>,
    pub(crate) first_successful_useful_action_ns: Option<u128>,
    pub(crate) first_model_output_ns: Option<u128>,
    pub(crate) first_actionable_output_ns: Option<u128>,
    pub(crate) first_visible_output_ns: Option<u128>,
    pub(crate) first_agent_message_ns: Option<u128>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TimingCounters {
    pub(crate) logical_generation_count: u32,
    pub(crate) generations_by_reason: TurnTimingGenerationReasonCounts,
    pub(crate) generations_by_purpose: TurnTimingGenerationPurposeCounts,
    pub(crate) generations_by_disposition: TurnTimingGenerationDispositionCounts,
    pub(crate) suppressed_deterministic_continuation_count: u32,
    pub(crate) residual_deterministic_generation_count: u32,
    pub(crate) owner_drained_continuation_count: u32,
    pub(crate) executed_validation_count: u32,
    pub(crate) reused_validation_count: u32,
    pub(crate) duplicate_validation_count: u32,
    pub(crate) forced_fresh_validation_count: u32,
    pub(crate) executed_validation_duration_ns: u64,
    pub(crate) suppressed_validation_output_count: u32,
    pub(crate) ready_startup_prewarm_count: u32,
    pub(crate) completion_review_ready_phase_count: u32,
    pub(crate) completion_review_terminal_phase_count: u32,
    pub(crate) same_purpose_continuation_count: u32,
    pub(crate) exact_repeated_wait_count: u32,
    pub(crate) planning_generation_count: u32,
    pub(crate) plan_revision_generation_count: u32,
    pub(crate) planning_fixed_point_iteration_count: u32,
    pub(crate) planning_invalidation_count: u32,
    pub(crate) planning_semantic_effect_count: u32,
    pub(crate) planning_failure_count: u32,
    pub(crate) attempts_by_kind: TurnTimingAttemptKindCounts,
    pub(crate) model_request_count: u32,
    pub(crate) model_retry_count: u32,
    pub(crate) model_fallback_count: u32,
    pub(crate) tool_call_count: u32,
    pub(crate) approval_wait_count: u32,
    pub(crate) permission_wait_count: u32,
    pub(crate) user_input_wait_count: u32,
    pub(crate) mcp_elicitation_wait_count: u32,
    pub(crate) wait_only_generation_count: u32,
    pub(crate) internally_drained_wait_count: u32,
    pub(crate) no_progress_directive_count: u32,
    pub(crate) proven_loop_activation_count: u32,
    pub(crate) tool_output_truncation_count: u32,
    pub(crate) tool_output_projected_token_count: u64,
    pub(crate) tool_output_artifact_reread_count: u32,
    pub(crate) tool_output_canonical_byte_count: u64,
    pub(crate) tool_output_canonical_token_count: u64,
    pub(crate) tool_output_model_byte_count: u64,
    pub(crate) tool_output_model_token_count: u64,
    pub(crate) tool_output_artifact_creation_count: u32,
    pub(crate) tool_output_projection_truncation_count: u32,
    pub(crate) tool_output_omitted_section_count: u64,
    pub(crate) tool_output_recovery_call_count: u32,
    pub(crate) tool_output_recovery_retruncation_count: u32,
    pub(crate) tool_output_recursive_spill_count: u32,
    pub(crate) attributable_recovery_generation_count: u32,
    pub(crate) truncation_induced_continuation_count: u32,
    pub(crate) invalid_transition_count: u32,
    pub(crate) clock_regression_count: u32,
    pub(crate) saturation_count: u32,
}

#[derive(Debug, Default)]
struct TurnTimingStateInner {
    started_sample: Option<ClockSample>,
    last_monotonic_ns: Option<u128>,
    activity: ActiveSet,
    exclusive: ExclusiveTiming,
    unions: TimingUnions,
    local: LocalTiming,
    milestones: TimingMilestones,
    counters: TimingCounters,
    model_requests: Vec<ModelRequestTiming>,
    current_generation_index: Option<u32>,
    current_generation_reason: TurnTimingGenerationReason,
    current_generation_purpose: Option<TurnTimingGenerationPurpose>,
    current_generation_disposition: TurnTimingGenerationDisposition,
    current_relevant_state_fingerprint: Option<String>,
    current_failure_fingerprint: Option<String>,
    next_attempt_kind: TurnTimingAttemptKind,
    legacy: LegacyProfileState,
    completed_snapshot: Option<TurnTimingSnapshot>,
    attributed_client_union_ns: u128,
    client_critical_phases: ClientCriticalPhaseTiming,
    dispatch_ready_snapshot: Option<(u128, u128, ClientCriticalPhaseTiming)>,
    pre_first_model_output: Option<PreFirstModelOutputTiming>,
    terminalization: TurnTimingTerminalization,
    tool_output_projection_pending_continuation: bool,
    deterministic_continuation_receipts:
        BTreeMap<String, TurnTimingDeterministicContinuationReceipt>,
    deterministic_continuation_receipt_overflow: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ActiveSet {
    model: u32,
    model_request_wait: u32,
    model_stream_wait: u32,
    model_stream_processing: u32,
    tool: u32,
    interactive: u32,
    retry: u32,
    standalone: u32,
    preparation: u32,
    planning: u32,
    history_snapshot: u32,
    normalization: u32,
    prompt_construction: u32,
    request_transformation: u32,
    compaction: u32,
    persistence: u32,
    serialization: u32,
    router_build: u32,
    startup_prewarm_wait: u32,
    executor_readiness_wait: u32,
    transport_readiness: u32,
    finalizing: bool,
}

impl ActiveSet {
    fn has_explicit_machine_activity(self) -> bool {
        self.model > 0 || self.tool > 0 || self.retry > 0 || self.standalone > 0
    }

    fn is_supported(self) -> bool {
        if self.finalizing {
            return !self.has_explicit_machine_activity() && self.interactive == 0;
        }
        if self.standalone > 0 {
            return self.model == 0 && self.tool == 0 && self.retry == 0;
        }
        if self.retry > 0 {
            return self.model == 0 && self.tool == 0;
        }
        true
    }

    fn is_contradictory(self) -> bool {
        self.finalizing && (self.has_explicit_machine_activity() || self.interactive > 0)
    }

    fn exclusive_phase(self) -> ExclusivePhase {
        if !self.is_supported() {
            return ExclusivePhase::Unclassified;
        }
        if self.interactive > 0 {
            return if self.has_explicit_machine_activity() {
                ExclusivePhase::InteractiveMachineOverlap
            } else {
                ExclusivePhase::InteractiveOnly
            };
        }
        if self.finalizing {
            return ExclusivePhase::Finalization;
        }
        if self.model > 0 && self.tool > 0 {
            return ExclusivePhase::ModelToolOverlap;
        }
        if self.model > 0 {
            return ExclusivePhase::ModelOnly;
        }
        if self.tool > 0 {
            return ExclusivePhase::ToolOnly;
        }
        if self.retry > 0 {
            return ExclusivePhase::RetryOnly;
        }
        if self.standalone > 0 {
            return ExclusivePhase::StandaloneWork;
        }
        ExclusivePhase::Orchestration
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExclusivePhase {
    ModelOnly,
    ToolOnly,
    ModelToolOverlap,
    InteractiveOnly,
    InteractiveMachineOverlap,
    RetryOnly,
    Orchestration,
    StandaloneWork,
    Finalization,
    Unclassified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GuardKind {
    LegacySampling,
    LegacyToolBlocking,
    ModelRequestWait,
    ModelStreamWait,
    ModelStreamProcessing,
    ToolExecution,
    InteractiveWait(InteractiveWaitKind),
    RetryBackoff,
    StandaloneWork,
    Local(TurnLocalPhase),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InteractiveWaitKind {
    Approval,
    Permission,
    UserInput,
    McpElicitation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnLocalPhase {
    Preparation,
    Planning,
    HistorySnapshot,
    Normalization,
    PromptConstruction,
    RequestTransformation,
    Compaction,
    Persistence,
    Serialization,
    RouterBuild,
    StartupPrewarmWait,
    ExecutorReadinessWait,
    TransportReadiness,
}

#[must_use]
pub(crate) struct TurnTimingGuard {
    timing: Arc<TurnTimingState>,
    kind: GuardKind,
    active: bool,
}

impl TurnTimingState {
    fn new(clock: Arc<dyn TurnClock>) -> Self {
        Self {
            clock,
            state: StdMutex::new(TurnTimingStateInner::default()),
        }
    }

    #[cfg(test)]
    fn with_clock(clock: Arc<dyn TurnClock>) -> Self {
        Self::new(clock)
    }

    pub(crate) fn mark_turn_started(&self) -> i64 {
        let sample = self.clock.sample();
        self.state().start(sample);
        sample.time.wall_unix_ms
    }

    pub(crate) async fn started_at_unix_secs(&self) -> Option<i64> {
        self.state()
            .started_sample
            .map(|sample| sample.time.wall_unix_ms / 1_000)
    }

    pub(crate) fn complete_snapshot(&self) -> TurnTimingSnapshot {
        let sample = self.clock.sample();
        self.state().complete(sample)
    }

    pub(crate) fn begin_sampling(self: &Arc<Self>) -> TurnTimingGuard {
        self.begin_guard(GuardKind::LegacySampling)
    }

    pub(crate) fn record_sampling_retry(&self) {
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        state.legacy.record_sampling_retry();
        state.record_model_retry();
    }

    pub(crate) fn record_model_retry(&self) {
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        state.record_model_retry();
    }

    #[cfg(test)]
    pub(crate) fn begin_model_generation(
        &self,
        pending: &mut Option<ContinuationCause>,
        session_source: &SessionSource,
    ) {
        self.begin_model_generation_with_metadata(
            pending,
            session_source,
            Some(TurnTimingGenerationPurpose::InitialReasoning),
            TurnTimingGenerationDisposition::DecisionBearing,
            None,
        );
    }

    pub(crate) fn begin_model_generation_with_metadata(
        &self,
        pending: &mut Option<ContinuationCause>,
        session_source: &SessionSource,
        purpose: Option<TurnTimingGenerationPurpose>,
        disposition: TurnTimingGenerationDisposition,
        relevant_state_fingerprint: Option<String>,
    ) {
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        let cause = pending.take();
        if let Some(cause) = cause {
            state.legacy.record_continuation(cause);
        }
        let projected_tool_output = state.tool_output_projection_pending_continuation;
        state.tool_output_projection_pending_continuation = false;
        if projected_tool_output && matches!(cause, Some(ContinuationCause::ToolResult)) {
            state.counters.truncation_induced_continuation_count = state
                .counters
                .truncation_induced_continuation_count
                .saturating_add(1);
            state.counters.attributable_recovery_generation_count = state
                .counters
                .attributable_recovery_generation_count
                .saturating_add(1);
        }
        let reason = match (cause, session_source) {
            (Some(ContinuationCause::CompletionReviewRepair), _) => {
                TurnTimingGenerationReason::CompletionRepairRereview
            }
            (Some(ContinuationCause::Compaction), _) => TurnTimingGenerationReason::Compaction,
            (_, SessionSource::SubAgent(SubAgentSource::Compact)) => {
                TurnTimingGenerationReason::Compaction
            }
            (_, SessionSource::SubAgent(SubAgentSource::Review)) => {
                TurnTimingGenerationReason::CompletionReview
            }
            (_, SessionSource::SubAgent(_)) => TurnTimingGenerationReason::Subagent,
            (Some(ContinuationCause::ToolResult), _) => {
                TurnTimingGenerationReason::ToolContinuation
            }
            (None, _) if state.counters.logical_generation_count == 0 => {
                TurnTimingGenerationReason::Initial
            }
            _ => TurnTimingGenerationReason::Other,
        };
        state.start_generation(reason, purpose, disposition, relevant_state_fingerprint);
    }

    pub(crate) fn begin_model_generation_with_failure_metadata(
        &self,
        pending: &mut Option<ContinuationCause>,
        session_source: &SessionSource,
        purpose: Option<TurnTimingGenerationPurpose>,
        disposition: TurnTimingGenerationDisposition,
        relevant_state_fingerprint: Option<String>,
        failure_fingerprint: Option<String>,
    ) {
        self.begin_model_generation_with_metadata(
            pending,
            session_source,
            purpose,
            disposition,
            relevant_state_fingerprint,
        );
        self.state().current_failure_fingerprint = failure_fingerprint;
    }

    pub(crate) fn begin_compaction_generation(&self) {
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        state.start_generation(
            TurnTimingGenerationReason::Compaction,
            Some(TurnTimingGenerationPurpose::CompactionRecovery),
            TurnTimingGenerationDisposition::DecisionBearing,
            None,
        );
    }

    pub(crate) fn record_model_fallback(&self) {
        let mut state = self.state();
        state.counters.model_fallback_count = state.counters.model_fallback_count.saturating_add(1);
        state.next_attempt_kind = TurnTimingAttemptKind::Fallback;
    }

    pub(crate) fn current_model_attempt_metadata(&self) -> Option<ModelAttemptGenerationMetadata> {
        let state = self.state();
        let request = state.model_requests.last()?;
        Some(ModelAttemptGenerationMetadata {
            generation_index: request.generation_index,
            generation_purpose: request.generation_purpose,
            disposition: request.disposition,
            relevant_state_fingerprint: request.relevant_state_fingerprint.clone(),
        })
    }

    pub(crate) fn record_model_attempt_identity(
        &self,
        sampling_request_id: &str,
        physical_attempt_id: &str,
    ) {
        let mut state = self.state();
        let Some(request) = state.model_requests.last_mut() else {
            state.invalid_transition();
            return;
        };
        let mismatched_sampling_request = match request.sampling_request_id.as_deref() {
            None => {
                request.sampling_request_id = Some(sampling_request_id.to_string());
                false
            }
            Some(existing) => existing != sampling_request_id,
        };
        if mismatched_sampling_request {
            state.invalid_transition();
            return;
        }
        if !request
            .physical_attempt_ids
            .iter()
            .any(|existing| existing == physical_attempt_id)
        {
            request
                .physical_attempt_ids
                .push(physical_attempt_id.to_string());
        }
    }

    pub(crate) fn record_user_input(&self) {
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        if state.milestones.user_input_recorded_ns.is_none()
            && let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns)
        {
            state.milestones.user_input_recorded_ns = Some(elapsed_ns);
        }
    }

    pub(crate) fn record_tool_call(&self, tool_name: &str) {
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        if let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns) {
            state
                .milestones
                .first_tool_accepted_ns
                .get_or_insert(elapsed_ns);
            if tool_counts_as_useful_first_action(tool_name) {
                state
                    .milestones
                    .first_useful_tool_accepted_ns
                    .get_or_insert(elapsed_ns);
            }
        }
        state.counters.tool_call_count = state.counters.tool_call_count.saturating_add(1);
        if let Some(generation_index) = state.current_generation_index
            && let Some(request) = state.model_requests.iter_mut().find(|request| {
                request.generation_index == generation_index
                    && request.attempt_kind == TurnTimingAttemptKind::Primary
            })
        {
            request.tool_call_count = request.tool_call_count.saturating_add(1);
        }
    }

    pub(crate) fn record_tool_gate_admitted(&self, tool_name: &str) {
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        if let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns) {
            state
                .milestones
                .first_tool_gate_admitted_ns
                .get_or_insert(elapsed_ns);
            if tool_counts_as_useful_first_action(tool_name) {
                state
                    .milestones
                    .first_useful_tool_gate_admitted_ns
                    .get_or_insert(elapsed_ns);
            }
        }
    }

    pub(crate) fn record_tool_handler_entry(&self, tool_name: &str) {
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        if let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns) {
            state
                .milestones
                .first_tool_handler_entry_ns
                .get_or_insert(elapsed_ns);
            if tool_counts_as_useful_first_action(tool_name) {
                state
                    .milestones
                    .first_useful_action_ns
                    .get_or_insert(elapsed_ns);
            }
        }
    }

    pub(crate) fn record_tool_completion(&self, tool_name: &str, successful: bool) {
        if !successful || !tool_counts_as_useful_first_action(tool_name) {
            return;
        }
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        if state.milestones.first_successful_useful_action_ns.is_none()
            && let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns)
        {
            state.milestones.first_successful_useful_action_ns = Some(elapsed_ns);
        }
    }

    pub(crate) fn record_initial_plan_generation(&self) {
        let mut state = self.state();
        state.counters.planning_generation_count =
            state.counters.planning_generation_count.saturating_add(1);
    }

    pub(crate) fn record_plan_revision_generation(&self) {
        let mut state = self.state();
        state.counters.plan_revision_generation_count = state
            .counters
            .plan_revision_generation_count
            .saturating_add(1);
    }

    pub(crate) fn record_planning_fixed_point_iteration(&self) {
        let mut state = self.state();
        state.counters.planning_fixed_point_iteration_count = state
            .counters
            .planning_fixed_point_iteration_count
            .saturating_add(1);
    }

    pub(crate) fn record_planning_invalidation(&self) {
        let mut state = self.state();
        state.counters.planning_invalidation_count =
            state.counters.planning_invalidation_count.saturating_add(1);
    }

    pub(crate) fn record_planning_semantic_effect(&self) {
        let mut state = self.state();
        state.counters.planning_semantic_effect_count = state
            .counters
            .planning_semantic_effect_count
            .saturating_add(1);
    }

    pub(crate) fn record_planning_failure(&self) {
        let mut state = self.state();
        state.counters.planning_failure_count =
            state.counters.planning_failure_count.saturating_add(1);
    }

    pub(crate) fn record_wait_only_generation(&self) {
        let mut state = self.state();
        state.counters.wait_only_generation_count =
            state.counters.wait_only_generation_count.saturating_add(1);
    }

    pub(crate) fn record_internally_drained_waits(&self, count: u32) {
        if count == 0 {
            return;
        }
        let mut state = self.state();
        state.counters.internally_drained_wait_count = state
            .counters
            .internally_drained_wait_count
            .saturating_add(count);
    }

    pub(crate) fn record_residual_deterministic_generation(&self) {
        let mut state = self.state();
        state.counters.residual_deterministic_generation_count = state
            .counters
            .residual_deterministic_generation_count
            .saturating_add(1);
    }

    pub(crate) fn record_owner_drained_continuation(&self) {
        let mut state = self.state();
        state.counters.owner_drained_continuation_count = state
            .counters
            .owner_drained_continuation_count
            .saturating_add(1);
    }

    pub(crate) fn record_reused_validation(&self) {
        let mut state = self.state();
        state.counters.reused_validation_count =
            state.counters.reused_validation_count.saturating_add(1);
        state.counters.duplicate_validation_count =
            state.counters.duplicate_validation_count.saturating_add(1);
    }

    pub(crate) fn record_executed_validation(&self, duration_ms: u64, force_fresh: bool) {
        let mut state = self.state();
        state.counters.executed_validation_count =
            state.counters.executed_validation_count.saturating_add(1);
        state.counters.executed_validation_duration_ns = state
            .counters
            .executed_validation_duration_ns
            .saturating_add(duration_ms.saturating_mul(1_000_000));
        if force_fresh {
            state.counters.forced_fresh_validation_count = state
                .counters
                .forced_fresh_validation_count
                .saturating_add(1);
        }
    }

    pub(crate) fn record_suppressed_validation_output(&self) {
        let mut state = self.state();
        state.counters.suppressed_validation_output_count = state
            .counters
            .suppressed_validation_output_count
            .saturating_add(1);
    }

    pub(crate) fn record_ready_startup_prewarm(&self) {
        let mut state = self.state();
        state.counters.ready_startup_prewarm_count =
            state.counters.ready_startup_prewarm_count.saturating_add(1);
    }

    pub(crate) fn record_completion_review_ready_phase(&self) {
        let mut state = self.state();
        state.counters.completion_review_ready_phase_count = state
            .counters
            .completion_review_ready_phase_count
            .saturating_add(1);
    }

    pub(crate) fn record_completion_review_terminal_phase(&self) {
        let mut state = self.state();
        state.counters.completion_review_terminal_phase_count = state
            .counters
            .completion_review_terminal_phase_count
            .saturating_add(1);
    }

    pub(crate) fn record_generation_outcome(
        &self,
        progress_kinds: Vec<TurnTimingProgressKind>,
        next_structured_action_changed: bool,
        unchanged_relevant_state: bool,
    ) {
        let mut state = self.state();
        let Some(generation_index) = state.current_generation_index else {
            return;
        };
        if let Some(request) = state.model_requests.iter_mut().find(|request| {
            request.generation_index == generation_index
                && request.attempt_kind == TurnTimingAttemptKind::Primary
        }) {
            request.progress_kinds = progress_kinds;
            request.next_structured_action_changed = next_structured_action_changed;
            request.unchanged_relevant_state = unchanged_relevant_state;
        }
    }

    pub(crate) fn record_generation_token_usage(&self, usage: Option<&TokenUsage>) {
        let Some(usage) = usage else {
            return;
        };
        let mut state = self.state();
        let Some(generation_index) = state.current_generation_index else {
            return;
        };
        if let Some(request) = state
            .model_requests
            .iter_mut()
            .rev()
            .find(|request| request.generation_index == generation_index)
        {
            let output_tokens = u64::try_from(usage.output_tokens.max(0)).unwrap_or(u64::MAX);
            let reasoning_tokens =
                u64::try_from(usage.reasoning_output_tokens.max(0)).unwrap_or(u64::MAX);
            request.output_tokens = output_tokens;
            request.reasoning_output_tokens = reasoning_tokens;
            request.token_usage = Some(TurnTimingProviderTokenUsage {
                input_tokens: u64::try_from(usage.input_tokens.max(0)).unwrap_or(u64::MAX),
                cached_input_tokens: u64::try_from(usage.cached_input_tokens.max(0))
                    .unwrap_or(u64::MAX),
                visible_output_tokens: output_tokens.saturating_sub(reasoning_tokens),
                reasoning_tokens,
                total_tokens: u64::try_from(usage.total_tokens.max(0)).unwrap_or(u64::MAX),
            });
        }
    }

    pub(crate) fn record_model_request_token_categories(
        &self,
        categories: TurnTimingRequestTokenCategories,
    ) {
        if let Some(request) = self.state().model_requests.last_mut() {
            request.request_token_categories = Some(categories);
        }
    }

    pub(crate) fn record_accepted_deterministic_continuation_receipts(
        &self,
        accepted_receipts: &[TurnTimingDeterministicContinuationReceipt],
    ) {
        let mut state = self.state();
        for receipt in accepted_receipts {
            if receipt.suppressed_continuation_count == 0
                || receipt.resource_identity_hash.is_empty()
                || receipt.state_revision.is_empty()
                || receipt.action_bounds_hash.is_empty()
            {
                continue;
            }
            state.counters.suppressed_deterministic_continuation_count = state
                .counters
                .suppressed_deterministic_continuation_count
                .saturating_add(receipt.suppressed_continuation_count);
            let Some(identity) = receipt.runtime_identity() else {
                continue;
            };
            if let Some(existing) = state.deterministic_continuation_receipts.get_mut(&identity) {
                existing.suppressed_continuation_count = existing
                    .suppressed_continuation_count
                    .saturating_add(receipt.suppressed_continuation_count);
                continue;
            }
            if state.deterministic_continuation_receipts.len()
                >= MAX_DETERMINISTIC_CONTINUATION_RECEIPTS
            {
                state.deterministic_continuation_receipt_overflow = state
                    .deterministic_continuation_receipt_overflow
                    .saturating_add(1);
                continue;
            }
            state
                .deterministic_continuation_receipts
                .insert(identity, receipt.clone());
        }
    }

    pub(crate) fn record_no_progress_directive(&self) {
        let mut state = self.state();
        state.counters.no_progress_directive_count =
            state.counters.no_progress_directive_count.saturating_add(1);
    }

    pub(crate) fn record_proven_loop_activation(&self) {
        let mut state = self.state();
        state.counters.proven_loop_activation_count = state
            .counters
            .proven_loop_activation_count
            .saturating_add(1);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_tool_output_projection_facts(
        &self,
        canonical_bytes: u64,
        canonical_tokens: u64,
        model_bytes: u64,
        model_tokens: u64,
        artifact_created: bool,
        projection_truncated: bool,
        omitted_sections: u64,
        provider_visible: bool,
    ) {
        let mut state = self.state();
        state.counters.tool_output_truncation_count = state
            .counters
            .tool_output_truncation_count
            .saturating_add(u32::from(projection_truncated));
        state.counters.tool_output_projected_token_count = state
            .counters
            .tool_output_projected_token_count
            .saturating_add(model_tokens);
        state.counters.tool_output_canonical_byte_count = state
            .counters
            .tool_output_canonical_byte_count
            .saturating_add(canonical_bytes);
        state.counters.tool_output_canonical_token_count = state
            .counters
            .tool_output_canonical_token_count
            .saturating_add(canonical_tokens);
        state.counters.tool_output_model_byte_count = state
            .counters
            .tool_output_model_byte_count
            .saturating_add(model_bytes);
        state.counters.tool_output_model_token_count = state
            .counters
            .tool_output_model_token_count
            .saturating_add(model_tokens);
        state.counters.tool_output_artifact_creation_count = state
            .counters
            .tool_output_artifact_creation_count
            .saturating_add(u32::from(artifact_created));
        state.counters.tool_output_projection_truncation_count = state
            .counters
            .tool_output_projection_truncation_count
            .saturating_add(u32::from(projection_truncated));
        state.counters.tool_output_omitted_section_count = state
            .counters
            .tool_output_omitted_section_count
            .saturating_add(omitted_sections);
        if provider_visible {
            state.tool_output_projection_pending_continuation = true;
        }
    }

    pub(crate) fn record_tool_output_recovery(&self, retruncation_count: u32) {
        let mut state = self.state();
        state.counters.tool_output_recovery_call_count = state
            .counters
            .tool_output_recovery_call_count
            .saturating_add(1);
        state.counters.tool_output_recovery_retruncation_count = state
            .counters
            .tool_output_recovery_retruncation_count
            .saturating_add(retruncation_count);
    }

    pub(crate) fn record_completion_review_preflight(&self, elapsed: Duration) {
        let mut state = self.state();
        state.terminalization.review_preflight_ns = state
            .terminalization
            .review_preflight_ns
            .saturating_add(duration_to_u64_ns(elapsed));
    }

    pub(crate) fn record_completion_review(&self, elapsed: Duration) {
        let mut state = self.state();
        state.terminalization.review_ns = state
            .terminalization
            .review_ns
            .saturating_add(duration_to_u64_ns(elapsed));
    }

    pub(crate) fn record_reviewer_infrastructure_memo_hit(&self) {
        let mut state = self.state();
        state.terminalization.reviewer_infrastructure_memo_hit_count = state
            .terminalization
            .reviewer_infrastructure_memo_hit_count
            .saturating_add(1);
    }

    pub(crate) fn record_review_prevented_by_correctness(&self) {
        let mut state = self.state();
        state.terminalization.reviews_prevented_by_correctness_count = state
            .terminalization
            .reviews_prevented_by_correctness_count
            .saturating_add(1);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_final_proof_telemetry(
        &self,
        checkpoint_tokens: u64,
        validation_launch_count: u32,
        validation_process_ns: u64,
        validation_aggregate_ns: u64,
        validation_aggregate_count: u32,
        proof_reuse_count: u32,
        conservative_rerun_count: u32,
        diff_reuse_count: u32,
    ) {
        let mut state = self.state();
        let timing = &mut state.terminalization;
        timing.checkpoint_tokens = timing.checkpoint_tokens.max(checkpoint_tokens);
        timing.validation_launch_count = timing
            .validation_launch_count
            .saturating_add(validation_launch_count);
        timing.validation_process_ns = timing
            .validation_process_ns
            .saturating_add(validation_process_ns);
        timing.validation_aggregate_ns = timing
            .validation_aggregate_ns
            .saturating_add(validation_aggregate_ns);
        timing.validation_aggregate_count = timing
            .validation_aggregate_count
            .saturating_add(validation_aggregate_count);
        timing.proof_reuse_count = timing.proof_reuse_count.saturating_add(proof_reuse_count);
        timing.conservative_rerun_count = timing
            .conservative_rerun_count
            .saturating_add(conservative_rerun_count);
        timing.diff_reuse_count = timing.diff_reuse_count.saturating_add(diff_reuse_count);
    }

    pub(crate) fn record_tool_output_artifact_reread(&self) {
        let mut state = self.state();
        state.counters.tool_output_artifact_reread_count = state
            .counters
            .tool_output_artifact_reread_count
            .saturating_add(1);
    }

    #[cfg(test)]
    pub(crate) fn record_truncation_induced_continuation(&self) {
        let mut state = self.state();
        state.counters.truncation_induced_continuation_count = state
            .counters
            .truncation_induced_continuation_count
            .saturating_add(1);
    }

    pub(crate) fn begin_tool_blocking(self: &Arc<Self>) -> TurnTimingGuard {
        self.begin_guard(GuardKind::LegacyToolBlocking)
    }

    pub(crate) fn begin_model_request_wait(self: &Arc<Self>) -> TurnTimingGuard {
        {
            let mut state = self.state();
            if state.current_generation_index.is_none() {
                state.start_generation(
                    TurnTimingGenerationReason::Other,
                    Some(TurnTimingGenerationPurpose::InitialReasoning),
                    TurnTimingGenerationDisposition::DecisionBearing,
                    None,
                );
            }
            let is_continuation = !state.model_requests.is_empty();
            let generation_index = state.current_generation_index.unwrap_or_default();
            let generation_reason = state.current_generation_reason;
            let generation_purpose = state.current_generation_purpose;
            let disposition = state.current_generation_disposition;
            let relevant_state_fingerprint = state.current_relevant_state_fingerprint.clone();
            let failure_fingerprint = state.current_failure_fingerprint.clone();
            let attempt_kind = std::mem::take(&mut state.next_attempt_kind);
            state.model_requests.push(ModelRequestTiming {
                generation_index,
                generation_reason,
                generation_purpose,
                disposition,
                relevant_state_fingerprint,
                failure_fingerprint,
                attempt_kind,
                is_continuation,
                ..Default::default()
            });
            match attempt_kind {
                TurnTimingAttemptKind::Primary => {
                    state.counters.attempts_by_kind.primary =
                        state.counters.attempts_by_kind.primary.saturating_add(1);
                }
                TurnTimingAttemptKind::Retry => {
                    state.counters.attempts_by_kind.retry =
                        state.counters.attempts_by_kind.retry.saturating_add(1);
                }
                TurnTimingAttemptKind::Fallback => {
                    state.counters.attempts_by_kind.fallback =
                        state.counters.attempts_by_kind.fallback.saturating_add(1);
                }
            }
            state.counters.model_request_count =
                state.counters.model_request_count.saturating_add(1);
        }
        self.begin_guard(GuardKind::ModelRequestWait)
    }

    pub(crate) fn begin_model_stream_wait(self: &Arc<Self>) -> TurnTimingGuard {
        self.begin_guard(GuardKind::ModelStreamWait)
    }

    pub(crate) fn begin_model_stream_processing(self: &Arc<Self>) -> TurnTimingGuard {
        self.begin_guard(GuardKind::ModelStreamProcessing)
    }

    pub(crate) fn begin_tool_execution(self: &Arc<Self>) -> TurnTimingGuard {
        self.begin_guard(GuardKind::ToolExecution)
    }

    pub(crate) fn begin_interactive_wait(
        self: &Arc<Self>,
        kind: InteractiveWaitKind,
    ) -> TurnTimingGuard {
        self.begin_guard(GuardKind::InteractiveWait(kind))
    }

    pub(crate) fn begin_retry_backoff(self: &Arc<Self>) -> TurnTimingGuard {
        self.begin_guard(GuardKind::RetryBackoff)
    }

    pub(crate) fn begin_standalone_work(self: &Arc<Self>) -> TurnTimingGuard {
        self.begin_guard(GuardKind::StandaloneWork)
    }

    pub(crate) fn begin_local_phase(self: &Arc<Self>, phase: TurnLocalPhase) -> TurnTimingGuard {
        self.begin_guard(GuardKind::Local(phase))
    }

    pub(crate) fn begin_finalization(&self) {
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        if state.completed_snapshot.is_some() || state.activity.finalizing {
            state.invalid_transition();
            return;
        }
        state.activity.finalizing = true;
        state.validate_activity();
    }

    pub(crate) fn record_response_event_milestones(
        &self,
        event: &ResponseEvent,
    ) -> Option<Duration> {
        let records_model_output = response_event_records_model_output(event);
        let records_actionable_output = response_event_records_actionable_output(event);
        let records_visible_output = response_event_records_visible_output(event);
        let records_completion = matches!(event, ResponseEvent::Completed { .. });
        if !records_model_output
            && !records_actionable_output
            && !records_visible_output
            && !records_completion
        {
            return None;
        }
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        let elapsed_ns = state.elapsed_since_start(sample.time.monotonic_ns)?;
        if records_completion
            && let Some(request) = state.model_requests.last_mut()
            && request.completed_ns.is_none()
        {
            request.completed_ns = Some(elapsed_ns);
        }
        if records_model_output
            && let Some(request) = state.model_requests.last_mut()
            && request.first_model_output_ns.is_none()
        {
            request.first_model_output_ns = Some(elapsed_ns);
        }
        if records_actionable_output
            && let Some(request) = state.model_requests.last_mut()
            && request.first_actionable_output_ns.is_none()
        {
            request.first_actionable_output_ns = Some(elapsed_ns);
        }
        if records_actionable_output && state.milestones.first_actionable_output_ns.is_none() {
            state.milestones.first_actionable_output_ns = Some(elapsed_ns);
        }
        if records_model_output && state.milestones.first_model_output_ns.is_none() {
            state.milestones.first_model_output_ns = Some(elapsed_ns);
            if state.pre_first_model_output.is_none()
                && let Some((dispatch_ready_ns, attributed_ns, phases)) =
                    state.dispatch_ready_snapshot.clone()
            {
                state.pre_first_model_output = Some(PreFirstModelOutputTiming {
                    captured_at_ns: elapsed_ns,
                    first_request_dispatch_ready_ns: dispatch_ready_ns,
                    client_critical_path_ns: dispatch_ready_ns,
                    attributed_client_union_ns: attributed_ns.min(dispatch_ready_ns),
                    unattributed_pre_output_ns: dispatch_ready_ns
                        .saturating_sub(attributed_ns.min(dispatch_ready_ns)),
                    history_snapshot_ns: phases.history_snapshot_ns,
                    normalization_ns: phases.normalization_ns,
                    prompt_construction_ns: phases.prompt_construction_ns,
                    request_transformation_ns: phases.request_transformation_ns,
                    serialization_ns: phases.serialization_ns,
                    transport_readiness_ns: phases.transport_readiness_ns,
                });
            }
        }
        if !records_visible_output || state.milestones.first_visible_output_ns.is_some() {
            return None;
        }
        state.milestones.first_visible_output_ns = Some(elapsed_ns);
        Some(duration_from_nanos(elapsed_ns))
    }

    pub(crate) fn mark_model_request_dispatched(&self) {
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        if state.dispatch_ready_snapshot.is_none()
            && let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns)
        {
            state.dispatch_ready_snapshot = Some((
                elapsed_ns,
                state.attributed_client_union_ns,
                state.client_critical_phases.clone(),
            ));
        }
        if let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns)
            && let Some(request) = state.model_requests.last_mut()
            && request.dispatch_ns.is_none()
        {
            request.dispatch_ns = Some(elapsed_ns);
        }
    }

    pub(crate) fn record_ttfm_for_turn_item(&self, item: &TurnItem) -> Option<Duration> {
        if !matches!(item, TurnItem::AgentMessage(_)) {
            return None;
        }
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        if state.milestones.first_agent_message_ns.is_some() {
            return None;
        }
        let elapsed_ns = state.elapsed_since_start(sample.time.monotonic_ns)?;
        state.milestones.first_agent_message_ns = Some(elapsed_ns);
        Some(duration_from_nanos(elapsed_ns))
    }

    fn begin_guard(self: &Arc<Self>, kind: GuardKind) -> TurnTimingGuard {
        let sample = self.clock.sample();
        let active = self.state().begin_guard(sample.time.monotonic_ns, kind);
        TurnTimingGuard {
            timing: Arc::clone(self),
            kind,
            active,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, TurnTimingStateInner> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for TurnTimingGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let sample = self.timing.clock.sample();
        self.timing
            .state()
            .end_guard(sample.time.monotonic_ns, self.kind);
    }
}

impl TurnTimingStateInner {
    fn record_model_retry(&mut self) {
        self.counters.model_retry_count = self.counters.model_retry_count.saturating_add(1);
        if self.next_attempt_kind != TurnTimingAttemptKind::Fallback {
            self.next_attempt_kind = TurnTimingAttemptKind::Retry;
        }
    }

    fn start_generation(
        &mut self,
        reason: TurnTimingGenerationReason,
        purpose: Option<TurnTimingGenerationPurpose>,
        disposition: TurnTimingGenerationDisposition,
        relevant_state_fingerprint: Option<String>,
    ) {
        let generation_index = self.counters.logical_generation_count;
        self.counters.logical_generation_count =
            self.counters.logical_generation_count.saturating_add(1);
        let count = match reason {
            TurnTimingGenerationReason::Initial => &mut self.counters.generations_by_reason.initial,
            TurnTimingGenerationReason::ToolContinuation => {
                &mut self.counters.generations_by_reason.tool_continuation
            }
            TurnTimingGenerationReason::CompletionReview => {
                &mut self.counters.generations_by_reason.completion_review
            }
            TurnTimingGenerationReason::CompletionRepairRereview => {
                &mut self
                    .counters
                    .generations_by_reason
                    .completion_repair_rereview
            }
            TurnTimingGenerationReason::Compaction => {
                &mut self.counters.generations_by_reason.compaction
            }
            TurnTimingGenerationReason::Subagent => {
                &mut self.counters.generations_by_reason.subagent
            }
            TurnTimingGenerationReason::Other => &mut self.counters.generations_by_reason.other,
        };
        *count = count.saturating_add(1);
        if let Some(purpose) = purpose {
            let purpose_count = match purpose {
                TurnTimingGenerationPurpose::InitialReasoning => {
                    &mut self.counters.generations_by_purpose.initial_reasoning
                }
                TurnTimingGenerationPurpose::ImplementationDecision => {
                    &mut self.counters.generations_by_purpose.implementation_decision
                }
                TurnTimingGenerationPurpose::Wait => &mut self.counters.generations_by_purpose.wait,
                TurnTimingGenerationPurpose::FailureDiagnosis => {
                    &mut self.counters.generations_by_purpose.failure_diagnosis
                }
                TurnTimingGenerationPurpose::ValidationInterpretation => {
                    &mut self
                        .counters
                        .generations_by_purpose
                        .validation_interpretation
                }
                TurnTimingGenerationPurpose::Repair => {
                    &mut self.counters.generations_by_purpose.repair
                }
                TurnTimingGenerationPurpose::Coordination => {
                    &mut self.counters.generations_by_purpose.coordination
                }
                TurnTimingGenerationPurpose::ArtifactContinuation => {
                    &mut self.counters.generations_by_purpose.artifact_continuation
                }
                TurnTimingGenerationPurpose::CompactionRecovery => {
                    &mut self.counters.generations_by_purpose.compaction_recovery
                }
                TurnTimingGenerationPurpose::TerminalCompletionReasoning => {
                    &mut self
                        .counters
                        .generations_by_purpose
                        .terminal_completion_reasoning
                }
            };
            *purpose_count = purpose_count.saturating_add(1);
        }
        match disposition {
            TurnTimingGenerationDisposition::Unknown => {
                self.counters.generations_by_disposition.unknown = self
                    .counters
                    .generations_by_disposition
                    .unknown
                    .saturating_add(1);
            }
            TurnTimingGenerationDisposition::DecisionBearing => {
                self.counters.generations_by_disposition.decision_bearing = self
                    .counters
                    .generations_by_disposition
                    .decision_bearing
                    .saturating_add(1);
            }
            TurnTimingGenerationDisposition::Deterministic => {
                self.counters.generations_by_disposition.deterministic = self
                    .counters
                    .generations_by_disposition
                    .deterministic
                    .saturating_add(1);
            }
        }
        if generation_index > 0 && purpose.is_some() && self.current_generation_purpose == purpose {
            self.counters.same_purpose_continuation_count = self
                .counters
                .same_purpose_continuation_count
                .saturating_add(1);
        }
        self.current_generation_index = Some(generation_index);
        self.current_generation_reason = reason;
        self.current_generation_purpose = purpose;
        self.current_generation_disposition = disposition;
        self.current_relevant_state_fingerprint = relevant_state_fingerprint;
        self.current_failure_fingerprint = None;
        self.next_attempt_kind = TurnTimingAttemptKind::Primary;
    }

    fn start(&mut self, sample: ClockSample) {
        *self = Self {
            started_sample: Some(sample),
            last_monotonic_ns: Some(sample.time.monotonic_ns),
            legacy: LegacyProfileState::new(sample.time.monotonic_ns),
            ..Self::default()
        };
    }

    fn begin_guard(&mut self, now_ns: u128, kind: GuardKind) -> bool {
        self.advance(now_ns);
        if self.started_sample.is_none() || self.completed_snapshot.is_some() {
            self.invalid_transition();
            return false;
        }
        match kind {
            GuardKind::LegacySampling => {
                if !self.legacy.begin(now_ns, LegacyPhase::Sampling) {
                    self.invalid_transition();
                    return false;
                }
            }
            GuardKind::LegacyToolBlocking => {
                if !self.legacy.begin(now_ns, LegacyPhase::ToolBlocking) {
                    self.invalid_transition();
                    return false;
                }
            }
            GuardKind::ModelRequestWait => {
                self.activity.model = self.activity.model.saturating_add(1);
                self.activity.model_request_wait =
                    self.activity.model_request_wait.saturating_add(1);
            }
            GuardKind::ModelStreamWait => {
                self.activity.model = self.activity.model.saturating_add(1);
                self.activity.model_stream_wait = self.activity.model_stream_wait.saturating_add(1);
            }
            GuardKind::ModelStreamProcessing => {
                self.activity.model = self.activity.model.saturating_add(1);
                self.activity.model_stream_processing =
                    self.activity.model_stream_processing.saturating_add(1);
            }
            GuardKind::ToolExecution => {
                self.activity.tool = self.activity.tool.saturating_add(1);
            }
            GuardKind::InteractiveWait(kind) => {
                self.activity.interactive = self.activity.interactive.saturating_add(1);
                self.increment_wait_count(kind);
            }
            GuardKind::RetryBackoff => {
                self.activity.retry = self.activity.retry.saturating_add(1);
            }
            GuardKind::StandaloneWork => {
                self.activity.standalone = self.activity.standalone.saturating_add(1);
            }
            GuardKind::Local(phase) => self.increment_local_activity(phase),
        }
        self.validate_activity();
        true
    }

    fn end_guard(&mut self, now_ns: u128, kind: GuardKind) {
        if self.completed_snapshot.is_some() {
            return;
        }
        self.advance(now_ns);
        let valid = match kind {
            GuardKind::LegacySampling => self.legacy.end(now_ns, LegacyPhase::Sampling),
            GuardKind::LegacyToolBlocking => self.legacy.end(now_ns, LegacyPhase::ToolBlocking),
            GuardKind::ModelRequestWait => {
                decrement(&mut self.activity.model_request_wait)
                    && decrement(&mut self.activity.model)
            }
            GuardKind::ModelStreamWait => {
                decrement(&mut self.activity.model_stream_wait)
                    && decrement(&mut self.activity.model)
            }
            GuardKind::ModelStreamProcessing => {
                decrement(&mut self.activity.model_stream_processing)
                    && decrement(&mut self.activity.model)
            }
            GuardKind::ToolExecution => decrement(&mut self.activity.tool),
            GuardKind::InteractiveWait(_) => decrement(&mut self.activity.interactive),
            GuardKind::RetryBackoff => decrement(&mut self.activity.retry),
            GuardKind::StandaloneWork => decrement(&mut self.activity.standalone),
            GuardKind::Local(phase) => self.decrement_local_activity(phase),
        };
        if !valid {
            self.invalid_transition();
        }
        self.validate_activity();
    }

    fn advance(&mut self, observed_now_ns: u128) {
        if self.completed_snapshot.is_some() {
            return;
        }
        let Some(previous_ns) = self.last_monotonic_ns else {
            return;
        };
        let now_ns = if observed_now_ns < previous_ns {
            self.counters.clock_regression_count =
                self.counters.clock_regression_count.saturating_add(1);
            previous_ns
        } else {
            observed_now_ns
        };
        let elapsed_ns = now_ns.saturating_sub(previous_ns);
        self.last_monotonic_ns = Some(now_ns);
        if self
            .exclusive
            .add(self.activity.exclusive_phase(), elapsed_ns)
        {
            self.saturated();
        }
        self.add_unions(elapsed_ns);
        self.add_local_unions(elapsed_ns);
        self.add_client_critical_phase_unions(elapsed_ns);
        if self.dispatch_ready_snapshot.is_none()
            // The named client-critical phases are the wire/request detail,
            // while local phases own the rest of pending-turn preparation.
            // Attribute their union here so nested work is counted once and
            // pre-dispatch orchestration does not fall into "unattributed".
            && (self.activity.preparation > 0
                || self.activity.planning > 0
                || self.activity.compaction > 0
                || self.activity.persistence > 0
                || self.activity.router_build > 0
                || self.activity.startup_prewarm_wait > 0
                || self.activity.executor_readiness_wait > 0
                || self.activity.history_snapshot > 0
                || self.activity.normalization > 0
                || self.activity.prompt_construction > 0
                || self.activity.request_transformation > 0
                || self.activity.serialization > 0
                || self.activity.transport_readiness > 0)
        {
            self.attributed_client_union_ns =
                self.attributed_client_union_ns.saturating_add(elapsed_ns);
        }
        self.legacy.advance(now_ns);
    }

    fn add_unions(&mut self, elapsed_ns: u128) {
        if self.activity.model > 0 {
            add_saturating(
                &mut self.unions.model_active_ns,
                elapsed_ns,
                &mut self.counters.saturation_count,
            );
        }
        if self.activity.model_request_wait > 0 {
            add_saturating(
                &mut self.unions.model_request_wait_ns,
                elapsed_ns,
                &mut self.counters.saturation_count,
            );
        }
        if self.activity.model_stream_wait > 0 {
            add_saturating(
                &mut self.unions.model_stream_wait_ns,
                elapsed_ns,
                &mut self.counters.saturation_count,
            );
            if let Some(request) = self.model_requests.last_mut() {
                add_saturating(
                    &mut request.model_stream_wait_ns,
                    elapsed_ns,
                    &mut self.counters.saturation_count,
                );
            }
        }
        if self.activity.model_stream_processing > 0 {
            add_saturating(
                &mut self.unions.model_stream_processing_ns,
                elapsed_ns,
                &mut self.counters.saturation_count,
            );
        }
        if self.activity.tool > 0 {
            add_saturating(
                &mut self.unions.tool_active_ns,
                elapsed_ns,
                &mut self.counters.saturation_count,
            );
            if let Some(generation_index) = self.current_generation_index
                && let Some(request) = self.model_requests.iter_mut().find(|request| {
                    request.generation_index == generation_index
                        && request.attempt_kind == TurnTimingAttemptKind::Primary
                })
            {
                add_saturating(
                    &mut request.tool_active_union_ns,
                    elapsed_ns,
                    &mut self.counters.saturation_count,
                );
            }
        }
        if self.activity.interactive > 0 {
            add_saturating(
                &mut self.unions.interactive_wait_ns,
                elapsed_ns,
                &mut self.counters.saturation_count,
            );
        }
    }

    fn add_local_unions(&mut self, elapsed_ns: u128) {
        let active = self.activity;
        let saturation_count = &mut self.counters.saturation_count;
        for (is_active, target) in [
            (active.preparation > 0, &mut self.local.preparation_ns),
            (active.planning > 0, &mut self.local.planning_ns),
            (active.compaction > 0, &mut self.local.compaction_ns),
            (active.persistence > 0, &mut self.local.persistence_ns),
            (active.serialization > 0, &mut self.local.serialization_ns),
            (active.router_build > 0, &mut self.local.router_build_ns),
            (
                active.startup_prewarm_wait > 0,
                &mut self.local.startup_prewarm_wait_ns,
            ),
            (
                active.executor_readiness_wait > 0,
                &mut self.local.executor_readiness_wait_ns,
            ),
        ] {
            if is_active {
                add_saturating(target, elapsed_ns, saturation_count);
            }
        }
        if active.planning > 0 && active.compaction > 0 {
            add_saturating(
                &mut self.local.planning_compaction_overlap_ns,
                elapsed_ns,
                saturation_count,
            );
        }
    }

    fn add_client_critical_phase_unions(&mut self, elapsed_ns: u128) {
        if self.dispatch_ready_snapshot.is_some() {
            return;
        }
        let active = self.activity;
        let phases = &mut self.client_critical_phases;
        let saturation_count = &mut self.counters.saturation_count;
        for (is_active, target) in [
            (active.history_snapshot > 0, &mut phases.history_snapshot_ns),
            (active.normalization > 0, &mut phases.normalization_ns),
            (
                active.prompt_construction > 0,
                &mut phases.prompt_construction_ns,
            ),
            (
                active.request_transformation > 0,
                &mut phases.request_transformation_ns,
            ),
            (active.serialization > 0, &mut phases.serialization_ns),
            (
                active.transport_readiness > 0,
                &mut phases.transport_readiness_ns,
            ),
        ] {
            if is_active {
                add_saturating(target, elapsed_ns, saturation_count);
            }
        }
    }

    fn complete(&mut self, sample: ClockSample) -> TurnTimingSnapshot {
        if let Some(snapshot) = self.completed_snapshot.as_ref() {
            return snapshot.clone();
        }
        self.advance(sample.time.monotonic_ns);
        let started_sample = self.started_sample;
        let inclusive_duration_ns = started_sample
            .map(|started| {
                self.last_monotonic_ns
                    .unwrap_or(started.time.monotonic_ns)
                    .saturating_sub(started.time.monotonic_ns)
            })
            .unwrap_or_default();
        let partition_valid = self.exclusive.total_ns() == inclusive_duration_ns;
        if !partition_valid {
            self.invalid_transition();
        }
        let profile_valid = started_sample.is_some()
            && partition_valid
            && self.counters.invalid_transition_count == 0
            && self.counters.clock_regression_count == 0
            && self.counters.saturation_count == 0;
        let profile = TurnTimingProfile {
            schema_version: TIMING_SCHEMA_VERSION,
            started: started_sample.is_some(),
            profile_valid,
            classification_complete: self.exclusive.unclassified_ns == 0,
            inclusive_duration_ns,
            machine_duration_ns: inclusive_duration_ns
                .saturating_sub(self.exclusive.interactive_only_wait_ns),
            exclusive: self.exclusive.clone(),
            unions: self.unions.clone(),
            local: self.local.clone(),
            milestones: self.milestones.clone(),
            counters: self.counters.clone(),
            model_requests: self.model_requests.clone(),
            deterministic_continuation_receipts: self
                .deterministic_continuation_receipts
                .values()
                .cloned()
                .collect(),
            deterministic_continuation_receipt_overflow: self
                .deterministic_continuation_receipt_overflow,
            pre_first_model_output: self.pre_first_model_output.clone(),
            terminalization: self.terminalization.clone(),
        };
        let legacy_profile = self
            .legacy
            .complete(self.last_monotonic_ns.unwrap_or(sample.time.monotonic_ns));
        let snapshot = TurnTimingSnapshot {
            started_at_unix_ms: started_sample.map(|started| started.time.wall_unix_ms),
            completed_at_unix_ms: started_sample.map(|_| sample.time.wall_unix_ms),
            completed_at_unix_secs: started_sample.map(|_| sample.time.wall_unix_ms / 1_000),
            duration_ms: started_sample.map(|_| u128_to_i64_ms(inclusive_duration_ns)),
            time_to_first_token_ms: self.milestones.first_model_output_ns.map(u128_to_i64_ms),
            legacy_profile,
            profile,
        };
        self.completed_snapshot = Some(snapshot.clone());
        snapshot
    }

    fn elapsed_since_start(&self, observed_now_ns: u128) -> Option<u128> {
        let started_ns = self.started_sample?.time.monotonic_ns;
        Some(
            self.last_monotonic_ns
                .unwrap_or(observed_now_ns)
                .saturating_sub(started_ns),
        )
    }

    fn increment_wait_count(&mut self, kind: InteractiveWaitKind) {
        let counter = match kind {
            InteractiveWaitKind::Approval => &mut self.counters.approval_wait_count,
            InteractiveWaitKind::Permission => &mut self.counters.permission_wait_count,
            InteractiveWaitKind::UserInput => &mut self.counters.user_input_wait_count,
            InteractiveWaitKind::McpElicitation => &mut self.counters.mcp_elicitation_wait_count,
        };
        *counter = counter.saturating_add(1);
    }

    fn increment_local_activity(&mut self, phase: TurnLocalPhase) {
        let counter = match phase {
            TurnLocalPhase::Preparation => &mut self.activity.preparation,
            TurnLocalPhase::Planning => &mut self.activity.planning,
            TurnLocalPhase::HistorySnapshot => &mut self.activity.history_snapshot,
            TurnLocalPhase::Normalization => &mut self.activity.normalization,
            TurnLocalPhase::PromptConstruction => &mut self.activity.prompt_construction,
            TurnLocalPhase::RequestTransformation => &mut self.activity.request_transformation,
            TurnLocalPhase::Compaction => &mut self.activity.compaction,
            TurnLocalPhase::Persistence => &mut self.activity.persistence,
            TurnLocalPhase::Serialization => &mut self.activity.serialization,
            TurnLocalPhase::RouterBuild => &mut self.activity.router_build,
            TurnLocalPhase::StartupPrewarmWait => &mut self.activity.startup_prewarm_wait,
            TurnLocalPhase::ExecutorReadinessWait => &mut self.activity.executor_readiness_wait,
            TurnLocalPhase::TransportReadiness => &mut self.activity.transport_readiness,
        };
        *counter = counter.saturating_add(1);
    }

    fn decrement_local_activity(&mut self, phase: TurnLocalPhase) -> bool {
        let counter = match phase {
            TurnLocalPhase::Preparation => &mut self.activity.preparation,
            TurnLocalPhase::Planning => &mut self.activity.planning,
            TurnLocalPhase::HistorySnapshot => &mut self.activity.history_snapshot,
            TurnLocalPhase::Normalization => &mut self.activity.normalization,
            TurnLocalPhase::PromptConstruction => &mut self.activity.prompt_construction,
            TurnLocalPhase::RequestTransformation => &mut self.activity.request_transformation,
            TurnLocalPhase::Compaction => &mut self.activity.compaction,
            TurnLocalPhase::Persistence => &mut self.activity.persistence,
            TurnLocalPhase::Serialization => &mut self.activity.serialization,
            TurnLocalPhase::RouterBuild => &mut self.activity.router_build,
            TurnLocalPhase::StartupPrewarmWait => &mut self.activity.startup_prewarm_wait,
            TurnLocalPhase::ExecutorReadinessWait => &mut self.activity.executor_readiness_wait,
            TurnLocalPhase::TransportReadiness => &mut self.activity.transport_readiness,
        };
        decrement(counter)
    }

    fn validate_activity(&mut self) {
        if self.activity.is_contradictory() {
            self.invalid_transition();
        }
    }

    fn invalid_transition(&mut self) {
        self.counters.invalid_transition_count =
            self.counters.invalid_transition_count.saturating_add(1);
    }

    fn saturated(&mut self) {
        self.counters.saturation_count = self.counters.saturation_count.saturating_add(1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LegacyPhase {
    Sampling,
    ToolBlocking,
}

#[derive(Debug, Default)]
struct LegacyProfileState {
    started_at_ns: Option<u128>,
    last_transition_ns: Option<u128>,
    active_phase: Option<LegacyPhase>,
    seen_sampling: bool,
    before_first_sampling_ns: u128,
    sampling_ns: u128,
    between_sampling_overhead_ns: u128,
    tool_blocking_ns: u128,
    pending_idle_after_sampling_ns: u128,
    sampling_request_count: u32,
    sampling_retry_count: u32,
    compaction: u32,
    tool_result: u32,
    server_end_turn_false: u32,
    pending_input: u32,
    stop_hook: u32,
    completion_review_repair: u32,
    invalid_image_recovery: u32,
}

impl LegacyProfileState {
    fn new(started_at_ns: u128) -> Self {
        Self {
            started_at_ns: Some(started_at_ns),
            last_transition_ns: Some(started_at_ns),
            ..Self::default()
        }
    }

    fn begin(&mut self, now_ns: u128, phase: LegacyPhase) -> bool {
        if self.started_at_ns.is_none() || self.active_phase.is_some() {
            return false;
        }
        self.advance(now_ns);
        if phase == LegacyPhase::Sampling {
            if self.seen_sampling {
                self.between_sampling_overhead_ns = self
                    .between_sampling_overhead_ns
                    .saturating_add(std::mem::take(&mut self.pending_idle_after_sampling_ns));
            }
            self.seen_sampling = true;
            self.sampling_request_count = self.sampling_request_count.saturating_add(1);
        }
        self.active_phase = Some(phase);
        true
    }

    fn end(&mut self, now_ns: u128, phase: LegacyPhase) -> bool {
        if self.active_phase != Some(phase) {
            return false;
        }
        self.advance(now_ns);
        self.active_phase = None;
        true
    }

    fn record_sampling_retry(&mut self) {
        if self.started_at_ns.is_some() {
            self.sampling_retry_count = self.sampling_retry_count.saturating_add(1);
        }
    }

    fn record_continuation(&mut self, cause: ContinuationCause) {
        if self.started_at_ns.is_none() {
            return;
        }
        let counter = match cause {
            ContinuationCause::Compaction => &mut self.compaction,
            ContinuationCause::ToolResult => &mut self.tool_result,
            ContinuationCause::ServerEndTurnFalse => &mut self.server_end_turn_false,
            ContinuationCause::PendingInput => &mut self.pending_input,
            ContinuationCause::StopHook => &mut self.stop_hook,
            ContinuationCause::CompletionReviewRepair => &mut self.completion_review_repair,
            ContinuationCause::InvalidImageRecovery => &mut self.invalid_image_recovery,
        };
        *counter = counter.saturating_add(1);
    }

    fn advance(&mut self, now_ns: u128) {
        let Some(previous_ns) = self.last_transition_ns.replace(now_ns) else {
            return;
        };
        let elapsed_ns = now_ns.saturating_sub(previous_ns);
        match self.active_phase {
            Some(LegacyPhase::Sampling) => {
                self.sampling_ns = self.sampling_ns.saturating_add(elapsed_ns)
            }
            Some(LegacyPhase::ToolBlocking) => {
                self.tool_blocking_ns = self.tool_blocking_ns.saturating_add(elapsed_ns)
            }
            None if self.seen_sampling => {
                self.pending_idle_after_sampling_ns = self
                    .pending_idle_after_sampling_ns
                    .saturating_add(elapsed_ns)
            }
            None => {
                self.before_first_sampling_ns =
                    self.before_first_sampling_ns.saturating_add(elapsed_ns)
            }
        }
    }

    fn complete(&mut self, now_ns: u128) -> TurnProfile {
        let final_phase = self.active_phase;
        self.advance(now_ns);
        let after_last_sampling_ns = if self.seen_sampling {
            std::mem::take(&mut self.pending_idle_after_sampling_ns)
        } else {
            0
        };
        let mut profile = TurnProfile {
            before_first_sampling_ms: u128_to_u64_ms(self.before_first_sampling_ns),
            sampling_ms: u128_to_u64_ms(self.sampling_ns),
            between_sampling_overhead_ms: u128_to_u64_ms(self.between_sampling_overhead_ns),
            tool_blocking_ms: u128_to_u64_ms(self.tool_blocking_ns),
            after_last_sampling_ms: u128_to_u64_ms(after_last_sampling_ns),
            sampling_request_count: self.sampling_request_count,
            sampling_retry_count: self.sampling_retry_count,
            compaction: self.compaction,
            tool_result: self.tool_result,
            server_end_turn_false: self.server_end_turn_false,
            pending_input: self.pending_input,
            stop_hook: self.stop_hook,
            completion_review_repair: self.completion_review_repair,
            invalid_image_recovery: self.invalid_image_recovery,
        };
        let total_ms = self
            .started_at_ns
            .map(|started_at_ns| u128_to_u64_ms(now_ns.saturating_sub(started_at_ns)))
            .unwrap_or_default();
        let classified_ms = profile
            .before_first_sampling_ms
            .saturating_add(profile.sampling_ms)
            .saturating_add(profile.between_sampling_overhead_ms)
            .saturating_add(profile.tool_blocking_ms)
            .saturating_add(profile.after_last_sampling_ms);
        let rounding_ms = total_ms.saturating_sub(classified_ms);
        match final_phase {
            Some(LegacyPhase::Sampling) => {
                profile.sampling_ms = profile.sampling_ms.saturating_add(rounding_ms)
            }
            Some(LegacyPhase::ToolBlocking) => {
                profile.tool_blocking_ms = profile.tool_blocking_ms.saturating_add(rounding_ms)
            }
            None if self.seen_sampling => {
                profile.after_last_sampling_ms =
                    profile.after_last_sampling_ms.saturating_add(rounding_ms)
            }
            None => {
                profile.before_first_sampling_ms =
                    profile.before_first_sampling_ms.saturating_add(rounding_ms)
            }
        }
        self.active_phase = None;
        profile
    }
}

pub(crate) fn response_event_records_model_output(event: &ResponseEvent) -> bool {
    match event {
        ResponseEvent::OutputItemDone(item) | ResponseEvent::OutputItemAdded(item) => {
            response_item_records_model_output(item)
        }
        ResponseEvent::OutputTextDelta(text)
        | ResponseEvent::ReasoningSummaryDelta { delta: text, .. }
        | ResponseEvent::ReasoningContentDelta { delta: text, .. }
        | ResponseEvent::ToolCallInputDelta { delta: text, .. } => !text.is_empty(),
        ResponseEvent::ReasoningSummaryDone { .. }
        | ResponseEvent::Created
        | ResponseEvent::ServerModel(_)
        | ResponseEvent::ModelVerifications(_)
        | ResponseEvent::TurnModerationMetadata(_)
        | ResponseEvent::SafetyBuffering(_)
        | ResponseEvent::ServerReasoningIncluded(_)
        | ResponseEvent::Completed { .. }
        | ResponseEvent::ReasoningSummaryPartAdded { .. }
        | ResponseEvent::RateLimits(_)
        | ResponseEvent::ModelsEtag(_) => false,
    }
}

/// Records the point at which the model has produced an executable tool call
/// or begun a user-facing answer. Reasoning deltas and partial tool arguments
/// intentionally do not satisfy this milestone.
pub(crate) fn response_event_records_actionable_output(event: &ResponseEvent) -> bool {
    match event {
        ResponseEvent::OutputItemDone(item) => response_item_records_actionable_output(item),
        ResponseEvent::OutputTextDelta(text) => !text.is_empty(),
        ResponseEvent::OutputItemAdded(_)
        | ResponseEvent::ReasoningSummaryDelta { .. }
        | ResponseEvent::ReasoningContentDelta { .. }
        | ResponseEvent::ToolCallInputDelta { .. }
        | ResponseEvent::ReasoningSummaryDone { .. }
        | ResponseEvent::Created
        | ResponseEvent::ServerModel(_)
        | ResponseEvent::ModelVerifications(_)
        | ResponseEvent::TurnModerationMetadata(_)
        | ResponseEvent::SafetyBuffering(_)
        | ResponseEvent::ServerReasoningIncluded(_)
        | ResponseEvent::Completed { .. }
        | ResponseEvent::ReasoningSummaryPartAdded { .. }
        | ResponseEvent::RateLimits(_)
        | ResponseEvent::ModelsEtag(_) => false,
    }
}

pub(crate) fn response_event_records_visible_output(event: &ResponseEvent) -> bool {
    match event {
        ResponseEvent::OutputItemDone(item) | ResponseEvent::OutputItemAdded(item) => {
            response_item_records_visible_output(item)
        }
        ResponseEvent::OutputTextDelta(text)
        | ResponseEvent::ReasoningSummaryDelta { delta: text, .. }
        | ResponseEvent::ReasoningContentDelta { delta: text, .. } => !text.is_empty(),
        ResponseEvent::Created
        | ResponseEvent::ServerModel(_)
        | ResponseEvent::ModelVerifications(_)
        | ResponseEvent::TurnModerationMetadata(_)
        | ResponseEvent::SafetyBuffering(_)
        | ResponseEvent::ServerReasoningIncluded(_)
        | ResponseEvent::ToolCallInputDelta { .. }
        | ResponseEvent::Completed { .. }
        | ResponseEvent::ReasoningSummaryDone { .. }
        | ResponseEvent::ReasoningSummaryPartAdded { .. }
        | ResponseEvent::RateLimits(_)
        | ResponseEvent::ModelsEtag(_) => false,
    }
}

fn response_item_records_model_output(item: &ResponseItem) -> bool {
    response_item_records_visible_output(item)
        || matches!(
            item,
            ResponseItem::LocalShellCall { .. }
                | ResponseItem::FunctionCall { .. }
                | ResponseItem::CustomToolCall { .. }
                | ResponseItem::ToolSearchCall { .. }
                | ResponseItem::WebSearchCall { .. }
                | ResponseItem::ImageGenerationCall { .. }
                | ResponseItem::Compaction { .. }
                | ResponseItem::ContextCompaction { .. }
        )
}

fn response_item_records_actionable_output(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::ContextCompaction { .. }
    ) || matches!(item, ResponseItem::Message { .. })
        && raw_assistant_output_text_from_item(item).is_some_and(|text| !text.is_empty())
}

fn response_item_records_visible_output(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { .. } => {
            raw_assistant_output_text_from_item(item).is_some_and(|text| !text.is_empty())
        }
        ResponseItem::Reasoning {
            summary, content, ..
        } => {
            summary.iter().any(|entry| match entry {
                codex_protocol::models::ReasoningItemReasoningSummary::SummaryText { text } => {
                    !text.is_empty()
                }
            }) || content.as_ref().is_some_and(|entries| {
                entries.iter().any(|entry| match entry {
                    codex_protocol::models::ReasoningItemContent::ReasoningText { text }
                    | codex_protocol::models::ReasoningItemContent::Text { text } => {
                        !text.is_empty()
                    }
                })
            })
        }
        ResponseItem::AgentMessage { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::AdditionalTools { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::Other => false,
    }
}

fn decrement(counter: &mut u32) -> bool {
    if *counter == 0 {
        return false;
    }
    *counter -= 1;
    true
}

fn add_saturating(target: &mut u128, value: u128, saturation_count: &mut u32) {
    if target.checked_add(value).is_none() {
        *saturation_count = saturation_count.saturating_add(1);
    }
    *target = target.saturating_add(value);
}

fn duration_from_nanos(nanos: u128) -> Duration {
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

fn duration_to_u64_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn u128_to_u64_ms(nanos: u128) -> u64 {
    u64::try_from(nanos / NANOS_PER_MILLISECOND).unwrap_or(u64::MAX)
}

fn u128_to_i64_ms(nanos: u128) -> i64 {
    i64::try_from(nanos / NANOS_PER_MILLISECOND).unwrap_or(i64::MAX)
}

fn public_ns(nanos: u128, saturation_count: &mut u32) -> u64 {
    match u64::try_from(nanos) {
        Ok(nanos) => nanos,
        Err(_) => {
            *saturation_count = saturation_count.saturating_add(1);
            u64::MAX
        }
    }
}

fn primary_generation_count(
    requests: &[ModelRequestTiming],
    purpose: TurnTimingGenerationPurpose,
) -> u32 {
    requests
        .iter()
        .filter(|request| {
            request.attempt_kind == TurnTimingAttemptKind::Primary
                && request.generation_purpose == Some(purpose)
        })
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

fn unique_primary_failure_signature_count(
    requests: &[ModelRequestTiming],
    purpose: TurnTimingGenerationPurpose,
) -> u32 {
    requests
        .iter()
        .filter(|request| {
            request.attempt_kind == TurnTimingAttemptKind::Primary
                && request.generation_purpose == Some(purpose)
        })
        .filter_map(|request| request.failure_fingerprint.as_deref())
        .collect::<BTreeSet<_>>()
        .len()
        .try_into()
        .unwrap_or(u32::MAX)
}

fn exact_repeated_wait_count(requests: &[ModelRequestTiming]) -> u32 {
    let mut seen = BTreeSet::new();
    requests
        .iter()
        .filter(|request| {
            request.attempt_kind == TurnTimingAttemptKind::Primary
                && request.generation_purpose == Some(TurnTimingGenerationPurpose::Wait)
        })
        .filter_map(|request| request.relevant_state_fingerprint.as_deref())
        .filter(|fingerprint| !seen.insert(*fingerprint))
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

fn purpose_aggregates(
    requests: &[ModelRequestTiming],
    saturation_count: &mut u32,
) -> Vec<TurnTimingGenerationPurposeAggregate> {
    const PURPOSES: [TurnTimingGenerationPurpose; 10] = [
        TurnTimingGenerationPurpose::InitialReasoning,
        TurnTimingGenerationPurpose::ImplementationDecision,
        TurnTimingGenerationPurpose::Wait,
        TurnTimingGenerationPurpose::FailureDiagnosis,
        TurnTimingGenerationPurpose::ValidationInterpretation,
        TurnTimingGenerationPurpose::Repair,
        TurnTimingGenerationPurpose::Coordination,
        TurnTimingGenerationPurpose::ArtifactContinuation,
        TurnTimingGenerationPurpose::CompactionRecovery,
        TurnTimingGenerationPurpose::TerminalCompletionReasoning,
    ];
    PURPOSES
        .into_iter()
        .filter_map(|purpose| {
            let matching = requests
                .iter()
                .filter(|request| request.generation_purpose == Some(purpose))
                .collect::<Vec<_>>();
            if matching.is_empty() {
                return None;
            }
            let generations = matching
                .iter()
                .filter(|request| request.attempt_kind == TurnTimingAttemptKind::Primary)
                .count()
                .try_into()
                .unwrap_or(u32::MAX);
            let wait_ns = matching.iter().fold(0_u128, |total, request| {
                total.saturating_add(request.model_stream_wait_ns)
            });
            let decision_latencies = matching
                .iter()
                .filter_map(|request| request.decision_latency_ns())
                .collect::<Vec<_>>();
            let decision_latency_ns = decision_latencies
                .iter()
                .fold(0_u128, |total, value| total.saturating_add(*value));
            let tool_active_union_ns = matching.iter().fold(0_u128, |total, request| {
                total.saturating_add(request.tool_active_union_ns)
            });
            Some(TurnTimingGenerationPurposeAggregate {
                purpose,
                generations,
                model_stream_wait_ns: public_ns(wait_ns, saturation_count),
                decision_latency_ns: public_ns(decision_latency_ns, saturation_count),
                decision_ready_requests: decision_latencies.len().try_into().unwrap_or(u32::MAX),
                tool_calls: matching.iter().fold(0_u32, |total, request| {
                    total.saturating_add(request.tool_call_count)
                }),
                tool_active_union_ns: public_ns(tool_active_union_ns, saturation_count),
                output_tokens: matching.iter().fold(0_u64, |total, request| {
                    total.saturating_add(request.output_tokens)
                }),
                reasoning_output_tokens: matching.iter().fold(0_u64, |total, request| {
                    total.saturating_add(request.reasoning_output_tokens)
                }),
                input_tokens: matching.iter().fold(0_u64, |total, request| {
                    total.saturating_add(
                        request
                            .token_usage
                            .as_ref()
                            .map_or(0, |usage| usage.input_tokens),
                    )
                }),
                cached_input_tokens: matching.iter().fold(0_u64, |total, request| {
                    total.saturating_add(
                        request
                            .token_usage
                            .as_ref()
                            .map_or(0, |usage| usage.cached_input_tokens),
                    )
                }),
                visible_output_tokens: matching.iter().fold(0_u64, |total, request| {
                    total.saturating_add(
                        request
                            .token_usage
                            .as_ref()
                            .map_or(0, |usage| usage.visible_output_tokens),
                    )
                }),
                total_tokens: matching.iter().fold(0_u64, |total, request| {
                    total.saturating_add(
                        request
                            .token_usage
                            .as_ref()
                            .map_or(0, |usage| usage.total_tokens),
                    )
                }),
            })
        })
        .collect()
}

fn diagnostic_token_aggregate(
    requests: &[ModelRequestTiming],
    includes: impl Fn(&ModelRequestTiming) -> bool,
    input_only: bool,
) -> TurnTimingDiagnosticTokenAggregate {
    let generation_ids = requests
        .iter()
        .filter(|request| {
            request.attempt_kind == TurnTimingAttemptKind::Primary && includes(request)
        })
        .map(|request| request.generation_index)
        .collect::<std::collections::BTreeSet<_>>();
    let mut aggregate = TurnTimingDiagnosticTokenAggregate {
        logical_generations: u32::try_from(generation_ids.len()).unwrap_or(u32::MAX),
        ..Default::default()
    };
    for usage in requests
        .iter()
        .filter(|request| generation_ids.contains(&request.generation_index))
        .filter_map(|request| request.token_usage.as_ref())
    {
        aggregate.input_tokens = aggregate.input_tokens.saturating_add(usage.input_tokens);
        aggregate.cached_input_tokens = aggregate
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        if !input_only {
            aggregate.visible_output_tokens = aggregate
                .visible_output_tokens
                .saturating_add(usage.visible_output_tokens);
            aggregate.reasoning_tokens = aggregate
                .reasoning_tokens
                .saturating_add(usage.reasoning_tokens);
            aggregate.total_tokens = aggregate.total_tokens.saturating_add(usage.total_tokens);
        }
    }
    aggregate
}

fn diagnostic_latency_aggregate(
    requests: &[ModelRequestTiming],
    includes: impl Fn(&ModelRequestTiming) -> bool,
    saturation_count: &mut u32,
) -> TurnTimingDiagnosticLatencyAggregate {
    let generation_ids = requests
        .iter()
        .filter(|request| {
            request.attempt_kind == TurnTimingAttemptKind::Primary && includes(request)
        })
        .map(|request| request.generation_index)
        .collect::<BTreeSet<_>>();
    let matching = requests
        .iter()
        .filter(|request| generation_ids.contains(&request.generation_index))
        .collect::<Vec<_>>();
    let decision_latencies = matching
        .iter()
        .filter_map(|request| request.decision_latency_ns())
        .collect::<Vec<_>>();
    TurnTimingDiagnosticLatencyAggregate {
        logical_generations: generation_ids.len().try_into().unwrap_or(u32::MAX),
        physical_attempts: matching.len().try_into().unwrap_or(u32::MAX),
        model_stream_wait_ns: public_ns(
            matching.iter().fold(0_u128, |total, request| {
                total.saturating_add(request.model_stream_wait_ns)
            }),
            saturation_count,
        ),
        decision_ready_attempts: decision_latencies.len().try_into().unwrap_or(u32::MAX),
        decision_latency_ns: public_ns(
            decision_latencies
                .iter()
                .fold(0_u128, |total, value| total.saturating_add(*value)),
            saturation_count,
        ),
        tool_calls: matching.iter().fold(0_u32, |total, request| {
            total.saturating_add(request.tool_call_count)
        }),
        tool_active_union_ns: public_ns(
            matching.iter().fold(0_u128, |total, request| {
                total.saturating_add(request.tool_active_union_ns)
            }),
            saturation_count,
        ),
    }
}

fn public_ms(nanos: u128, saturation_count: &mut u32) -> u64 {
    public_ns(nanos / NANOS_PER_MILLISECOND, saturation_count)
}

fn signed_difference(lhs: u64, rhs: u64) -> i64 {
    i128::from(lhs)
        .saturating_sub(i128::from(rhs))
        .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

pub(crate) fn now_unix_timestamp_ms() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[path = "turn_timing_tests.rs"]
mod tests;
