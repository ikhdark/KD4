use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use tokio::sync::Notify;

use codex_analytics::TurnProfile;
use codex_otel::TURN_TTFM_DURATION_METRIC;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::NextSampleBlockReason;
use codex_protocol::protocol::SamplingGenerationId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::ToolExecutionId;
use codex_protocol::protocol::ToolLifecycleBoundary;
use codex_protocol::protocol::ToolLifecycleContext;
use codex_protocol::protocol::ToolLifecycleTimerWait;
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
use codex_protocol::protocol::TurnTimingRequestDiagnosticsStatus;
use codex_protocol::protocol::TurnTimingRequestTokenCategories;
use codex_protocol::protocol::TurnTimingToolCall;
use codex_protocol::protocol::TurnTimingToolCallIdentity;
use codex_protocol::protocol::TurnTimingToolCallSource;
use codex_protocol::protocol::TurnTimingToolClosure;
use codex_protocol::protocol::TurnTimingUnions;
use sha2::Digest;
use sha2::Sha256;

use crate::ResponseEvent;
use crate::session::turn_context::TurnContext;
use crate::stream_events_utils::raw_assistant_output_text_from_item;
use crate::tools::tool_dispatch_trace::ToolDispatchTimingSnapshot;

mod blocking;
pub(crate) mod retries;

const NANOS_PER_MILLISECOND: u128 = 1_000_000;
// Schema 37 counts retries/fallbacks only at unique physical transport entry.
// Schema 36 adds paired await suspensions and explicit dependency edges.
// Schema 35 separates host assistant-text publication from reasoning/completion
// and names continuation-after-truncation without a causal claim.
// Schema 34 counts identity-bound transport attempts, including same-row auth retries.
// Schema 33 records unique tool admission (not polling) and locally observed
// nonempty model content for TTFT. Schemas <=32 aliased TTFT to visible output.
pub(crate) const TIMING_SCHEMA_VERSION: u16 = 37;
const MAX_DETERMINISTIC_CONTINUATION_RECEIPTS: usize = 64;
const MAX_TOOL_CALL_TIMINGS: usize = 1_024;
// These records are diagnostic histories, not the source of truth for the
// additive counters above. Retain a useful tail while keeping a long-running
// turn from accumulating identifiers and request metadata without bound.
const MAX_MODEL_REQUEST_TIMINGS: usize = 1_024;
const MAX_MODEL_REQUEST_PHYSICAL_ATTEMPT_IDS: usize = 64;
const MAX_MODEL_REQUEST_PROGRESS_KINDS: usize = 64;
const RESERVED_TOOL_OUTPUT_RECURSIVE_SPILL_COUNT: u32 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolActionClass {
    Infrastructure,
    Discovery,
    Domain,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ToolCallTimingLineage<'a> {
    pub(crate) parent_call_id: Option<&'a str>,
    pub(crate) parent_cell_id: Option<&'a str>,
    pub(crate) runtime_tool_call_id: Option<&'a str>,
}

fn tool_action_class(tool_name: &str) -> ToolActionClass {
    // Only known built-in namespaces carry our registration semantics. An
    // external MCP tool with a coincidentally identical suffix is domain work.
    let builtin = !tool_name.contains('.')
        || ["functions.", "collaboration.", "agents."].iter().any(|prefix| tool_name.starts_with(prefix));
    if !builtin {
        return ToolActionClass::Domain;
    }
    let tool_name = tool_name.rsplit('.').next().unwrap_or(tool_name);
    if matches!(
        tool_name,
        "exec"
            | "update_plan"
            | "request_user_input"
            | "request_permissions"
            | "wait"
            | "wait_agent"
            | "spawn_agent"
            | "send_message"
            | "send_input"
            | "followup_task"
            | "list_agents"
            | "interrupt_agent"
            | "close_agent"
            | "resume_agent"
            | "wait_for_environment"
            | "write_stdin"
    ) {
        ToolActionClass::Infrastructure
    } else if matches!(
        tool_name,
        "tool_search" | "list_mcp_resources" | "list_mcp_resource_templates"
    ) {
        ToolActionClass::Discovery
    } else {
        ToolActionClass::Domain
    }
}

/// Compatibility predicate for the legacy useful-action fields. Useful now
/// means domain work: neither control plumbing nor tool-schema discovery.
pub(crate) fn tool_counts_as_useful_first_action(tool_name: &str) -> bool {
    tool_action_class(tool_name) == ToolActionClass::Domain
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContinuationCause {
    Compaction,
    ToolResult,
    ServerEndTurnFalse,
    PendingInput,
    StopHook,
}

pub(crate) fn record_turn_ttft_metric(turn_context: &TurnContext, event: &ResponseEvent) {
    let Some(duration) = turn_context
        .turn_timing_state
        .record_response_event_milestones(event)
    else {
        return;
    };
    turn_context.session_telemetry.record_turn_ttft(duration);
}

pub(crate) fn record_turn_ttfm_metric(turn_context: &TurnContext, item: &TurnItem) {
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
    fn domain_id(&self) -> Option<&str> {
        None
    }
}

#[derive(Debug, Default)]
struct SystemTurnClock;

impl TurnClock for SystemTurnClock {
    fn domain_id(&self) -> Option<&str> {
        Some(codex_protocol::timing::process_clock_domain_id())
    }
    fn sample(&self) -> ClockSample {
        ClockSample {
            time: TimeSample {
                monotonic_ns: codex_protocol::timing::process_monotonic_ns(),
                wall_unix_ms: now_unix_timestamp_ms(),
            },
        }
    }
}

pub(crate) struct TurnTimingState {
    live_phase: StdMutex<LiveTurnPhase>,
    clock: Arc<dyn TurnClock>,
    state: StdMutex<TurnTimingStateInner>,
    relay_queue_depth: AtomicU32,
    parallel_gate_waiter_count: AtomicU32,
    sampling_gate_waiter_count: AtomicU32,
    process_output_waiter_count: AtomicU32,
    active_tool_count: AtomicU32,
    tool_closure_changed: Notify,
    /// Per-request, receipt, and activity-interval entries already written by
    /// an earlier sampling checkpoint of this turn. Checkpoints persist only new
    /// or changed entries, so their size no longer grows with every prior request.
    checkpointed_entries: StdMutex<CheckpointedEntries>,
    /// Essential per-physical-attempt receipts outlive optional detail eviction.
    /// The session accounting lock must be held when claiming a settlement.
    model_usage_receipts: StdMutex<BTreeMap<(String, String), (TokenUsage, bool)>>,
    canonical_input_section: StdMutex<Option<CanonicalInputSection>>,
}

/// One bounded optional diagnostic baseline, never an authority/cache receipt.
struct CanonicalInputSection {
    raw_items: Vec<String>,
    before_close: Sha256,
}

impl CanonicalInputSection {
    fn hash(cache: &mut Option<Self>, raw: &serde_json::value::RawValue) -> Option<String> {
        let items: Vec<&serde_json::value::RawValue> = serde_json::from_str(raw.get()).ok()?;
        let prior = cache.as_ref().filter(|previous| {
            previous.raw_items.len() <= items.len()
                && previous.raw_items.iter().zip(&items).all(|(a, b)| a == b.get())
        });
        let start = prior.map_or(0, |previous| previous.raw_items.len());
        let mut hasher = prior.map_or_else(|| {
            let mut hasher = Sha256::new(); hasher.update(b"["); hasher
        }, |previous| previous.before_close.clone());
        for (index, item) in items.iter().enumerate().skip(start) {
            if index != 0 { hasher.update(b","); }
            let value: serde_json::Value = serde_json::from_str(item.get()).ok()?;
            hasher.update(value.to_string().as_bytes());
        }
        let mut finished = hasher.clone(); finished.update(b"]");
        // Both retained payload and per-item overhead are bounded. A replacement,
        // unknown encoding, or oversized baseline takes the exact cold path.
        if raw.get().len() <= 8 * 1024 * 1024 && items.len() <= 16_384 {
            *cache = Some(Self { raw_items: items.iter().map(|item| item.get().to_owned()).collect(), before_close: hasher });
        } else { *cache = None; }
        Some(hex::encode(finished.finalize()))
    }
}

/// Only current wait counts and the existing event sink, not an execution history.
#[derive(Default)]
struct LiveTurnPhase {
    sink: Option<(
        String,
        async_channel::Sender<codex_protocol::protocol::Event>,
    )>,
    gates: [u32; 4],
    sampling: Option<NextSampleBlockReason>,
    published: Vec<codex_protocol::protocol::HarnessPhase>,
}

impl LiveTurnPhase {
    fn publish(&mut self) {
        use codex_protocol::protocol::HarnessPhase;
        let mut phases = [
            HarnessPhase::ResourceResolution,
            HarnessPhase::DiffTracker,
            HarnessPhase::WorkspaceGate,
            HarnessPhase::EvidenceTracker,
        ]
        .into_iter()
        .zip(self.gates)
        .filter_map(|(phase, count)| (count > 0).then_some(phase))
        .collect::<Vec<_>>();
        let sampling = match self.sampling {
            Some(NextSampleBlockReason::WaitingForDelivery) => Some(HarnessPhase::Delivery),
            Some(NextSampleBlockReason::WaitingForProcessCleanup) => {
                Some(HarnessPhase::ProcessCleanup)
            }
            Some(NextSampleBlockReason::WaitingForGate) => Some(HarnessPhase::WorkspaceGate),
            Some(NextSampleBlockReason::ReadyToSample) if phases.is_empty() => {
                Some(HarnessPhase::ReadyToSample)
            }
            _ => None,
        };
        if let Some(phase) = sampling
            && !phases.contains(&phase)
        {
            phases.push(phase);
        }
        if phases == self.published {
            return;
        }
        self.published = phases.clone();
        if let Some((turn_id, sender)) = &self.sink {
            // The session event channel is unbounded. Closed means its consumer exited.
            let _ = sender.try_send(codex_protocol::protocol::Event {
                id: turn_id.clone(),
                msg: codex_protocol::protocol::EventMsg::TurnPhaseChanged(
                    codex_protocol::protocol::TurnPhaseChangedEvent {
                        turn_id: turn_id.clone(),
                        phases,
                    },
                ),
            });
        }
    }
}

impl Default for TurnTimingState {
    fn default() -> Self {
        Self::new(Arc::new(SystemTurnClock))
    }
}

#[derive(Default)]
struct CheckpointedEntries {
    /// Content digests of every entry an earlier checkpoint wrote.
    digests: HashSet<[u8; 32]>,
    /// The previous checkpoint's complete lists, before filtering. `None`
    /// marks an entry that could not be serialized.
    model_requests: Vec<Option<TurnTimingModelRequest>>,
    deterministic_continuation_receipts: Vec<Option<TurnTimingDeterministicContinuationReceipt>>,
    /// Interval count of the previous checkpoint's complete activity timeline.
    activity_intervals: usize,
    blocking_intervals: Vec<Option<codex_protocol::timing::TurnTimingBlockingInterval>>,
    dependency_edges: usize,
}

/// Activity intervals are only appended, except that the last interval written
/// can still be extended. Keep that interval and every later one, so a reader
/// rebuilds the timeline by replacing its intervals that start at or after the
/// first one kept here. Repeating the whole prefix made every checkpoint grow
/// with the number of earlier generations.
fn retain_new_activity_intervals(
    intervals: &mut Vec<codex_protocol::timing::TurnTimingActivityInterval>,
    previous_len: &mut usize,
) {
    let total = intervals.len();
    intervals.drain(..previous_len.saturating_sub(1).min(total));
    *previous_len = total;
}

/// Keeps only entries whose exact serialized content no earlier checkpoint of
/// this turn recorded. An entry that changed (for example, a request that has
/// since completed) is written again; unserializable entries are always kept.
///
/// Equal entries serialize identically, so an entry equal to its counterpart
/// in `previous` was already recorded and is dropped without serializing it
/// again. Re-serializing every unchanged entry made each checkpoint cost grow
/// with the number of prior requests. Counterparts are aligned on the first
/// entry because a full request list drops its oldest entry.
fn retain_new_checkpoint_entries<T: serde::Serialize + Clone + PartialEq>(
    entries: &mut Vec<T>,
    previous: &mut Vec<Option<T>>,
    digests: &mut HashSet<[u8; 32]>,
) {
    let current = std::mem::take(entries);
    let offset = current
        .first()
        .and_then(|first| {
            previous
                .iter()
                .position(|entry| entry.as_ref() == Some(first))
        })
        .unwrap_or(0);
    let mut observed = Vec::with_capacity(current.len());
    for (index, entry) in current.into_iter().enumerate() {
        if previous
            .get(index + offset)
            .is_some_and(|recorded| recorded.as_ref() == Some(&entry))
        {
            observed.push(Some(entry));
            continue;
        }
        match serde_json::to_vec(&entry) {
            Ok(bytes) => {
                if digests.insert(Sha256::digest(&bytes).into()) {
                    entries.push(entry.clone());
                }
                observed.push(Some(entry));
            }
            Err(_) => {
                entries.push(entry);
                observed.push(None);
            }
        }
    }
    *previous = observed;
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
    pub(crate) activity_timeline: codex_protocol::timing::TurnTimingTimeline,
    pub(crate) credit_delta: Option<String>,
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
            TurnTimingGenerationPurpose::ArtifactContinuation => "tool_result_interpretation",
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
            turn_worker_started_ms: profile
                .milestones
                .turn_worker_started_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_request_preparation_started_ms: profile
                .milestones
                .first_request_preparation_started_ns
                .map(|value| public_ms(value, &mut saturation_count)),
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
            first_infrastructure_action_ms: profile
                .milestones
                .first_infrastructure_action_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_tool_discovery_action_ms: profile
                .milestones
                .first_tool_discovery_action_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_domain_action_ms: profile
                .milestones
                .first_domain_action_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_successful_domain_action_ms: profile
                .milestones
                .first_successful_domain_action_ns
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
            first_assistant_text_ms: profile
                .milestones
                .first_assistant_text_ns
                .map(|value| public_ms(value, &mut saturation_count)),
            first_assistant_text_item_id: profile.milestones.first_assistant_text_item_id.clone(),
            first_assistant_text_phase: profile.milestones.first_assistant_text_phase.clone(),
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
                        if let Some(usage) = request.token_usage.as_ref()
                            && usage.coverage.as_ref().is_some_and(|coverage| {
                                !coverage.saturated && coverage.input_tokens == coverage.receipts
                            })
                        {
                            categories.provider_input_tokens = Some(usage.input_tokens);
                            categories.provider_reconciliation_residual = Some(signed_difference(
                                usage.input_tokens,
                                categories.logical_total,
                            ));
                        }
                        categories
                    });
                TurnTimingModelRequest {
                    stream_delivery_by_attempt: request.stream_delivery_by_attempt.clone(),
                    response_queue_timing_by_attempt: request
                        .response_queue_timing_by_attempt
                        .clone(),
                    setup_phase_ns: request
                        .setup_phases
                        .as_ref()
                        .map_or_else(BTreeMap::new, |phases| {
                            phases.protocol_phases(&mut saturation_count)
                        }),
                    request_sha256_by_attempt: request.request_sha256_by_attempt.clone(),
                    request_section_sha256_by_attempt: request
                        .request_section_sha256_by_attempt
                        .clone(),
                    response_id_by_attempt: request.response_id_by_attempt.clone(),
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
                    model_emitted_tool_call_count: request.model_emitted_tool_call_count,
                    executor_admitted_tool_call_count: request.executor_admitted_tool_call_count,
                    executor_max_concurrent_tool_calls: request.executor_max_concurrent_tool_calls,
                    tool_active_union_ns: public_ns(
                        request.tool_active_union_ns,
                        &mut saturation_count,
                    ),
                    output_tokens: request.output_tokens,
                    reasoning_output_tokens: request.reasoning_output_tokens,
                    token_usage: request.token_usage.clone(),
                    request_token_categories,
                    request_diagnostics_status: request.request_diagnostics_status,
                    fixed_prefix_reuse_eligible: request.fixed_prefix_reuse_eligible,
                    prompt_cache_key_fingerprint: request.prompt_cache_key_fingerprint.clone(),
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
        // A different next command is not itself external or causal progress.
        // Keep action-change telemetry, but do not let it hide unchanged state.
        let observational_nonprogress_tokens = diagnostic_token_aggregate(
            &profile.model_requests,
            |request| request.progress_kinds.is_empty() && request.unchanged_relevant_state,
            /*input_only*/ false,
        );
        let observational_nonprogress_latency = diagnostic_latency_aggregate(
            &profile.model_requests,
            |request| request.progress_kinds.is_empty() && request.unchanged_relevant_state,
            &mut saturation_count,
        );
        let purpose_aggregates = purpose_aggregates(&profile.model_requests, &mut saturation_count);
        let wait_generations_with_same_revision_count =
            wait_generations_with_same_revision_count(&profile.model_requests);
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
            executed_validation_duration_ns: profile.counters.executed_validation_duration_ns,
            ready_startup_prewarm_count: profile.counters.ready_startup_prewarm_count,
            // No aggregate tool-output budget runs; the fields stay for the wire schema.
            tool_output_budget_drop_count: 0,
            tool_output_budget_dropped_token_count: 0,
            purpose_aggregates,
            same_purpose_continuation_count: profile.counters.same_purpose_continuation_count,
            wait_generations_with_same_revision_count,
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
            model_request_detail_eviction_count: profile
                .counters
                .model_request_detail_eviction_count,
            physical_attempt_detail_eviction_count: profile
                .counters
                .physical_attempt_detail_eviction_count,
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
            tool_router_reuse_count: profile.counters.tool_router_reuse_count,
            tool_router_rebuild_count: profile.counters.tool_router_rebuild_count,
            projection_source_dependencies_reuse_count: profile
                .counters
                .projection_source_dependencies_reuse_count,
            projection_source_dependencies_fallback_count: profile
                .counters
                .projection_source_dependencies_fallback_count,
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
            tool_output_artifact_reuse_count: profile.counters.tool_output_artifact_reuse_count,
            tool_output_projection_truncation_count: profile
                .counters
                .tool_output_projection_truncation_count,
            tool_output_omitted_section_count: profile.counters.tool_output_omitted_section_count,
            tool_output_recovery_call_count: profile.counters.tool_output_recovery_call_count,
            tool_output_in_cell_recovery_call_count: profile
                .counters
                .tool_output_in_cell_recovery_call_count,
            tool_output_recovery_retruncation_count: profile
                .counters
                .tool_output_recovery_retruncation_count,
            tool_output_recursive_spill_count: RESERVED_TOOL_OUTPUT_RECURSIVE_SPILL_COUNT,
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
            stop_cause: None,
            activity_timeline: Some(self.activity_timeline.clone()),
            final_answer: None,
            completion_assessment: None,
            credit_delta: self.credit_delta.clone(),
            // No checkout capture runs; the wire field stays for older records.
            checkout_snapshot_sha256: None,
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
            terminalization: None,
            model_requests,
            model_request_history_truncated: Some(profile.model_request_history_truncated),
            terminal_publication_duration_excluded: Some(true),
            tool_calls: profile.tool_calls.clone(),
            tool_call_details_artifact_id: None,
            tool_call_timing_overflow: profile.tool_call_timing_overflow,
            tool_closure: profile.tool_closure.clone(),
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

/// Reuse the authenticated, bounded artifact store instead of inventing a
/// second side-file lifecycle. Keep inline evidence if durable retention fails.
pub(crate) async fn retain_turn_timing_details(
    timing: TurnTiming,
    codex_home: &std::path::Path,
    thread_id: &str,
) -> TurnTiming {
    // The final request may complete after the last sampling checkpoint. Keep
    // the bounded terminal history and its real eviction flag so readers can
    // reconcile checkpoints without losing terminal usage and stream metrics.
    if timing
        .tool_calls
        .iter()
        .all(|call| call.lifecycle_events.is_empty() && call.timer_waits.is_empty())
    {
        return timing;
    }
    let Ok(bytes) = serde_json::to_vec(&timing.tool_calls) else {
        return timing;
    };
    let codex_home = codex_home.to_path_buf();
    let thread_id = thread_id.to_string();
    // Retain creation ownership if storage outlives terminal delivery. In
    // particular, do not cancel between creating a file and retention cleanup.
    let artifact = tokio::spawn(async move {
        crate::tools::command_output_artifact::create_raw_output_artifact(
            &codex_home,
            &thread_id,
            &bytes,
        )
        .await
    });
    retain_turn_timing_artifact(timing, artifact).await
}

const TURN_TIMING_STORAGE_TIMEOUT: Duration = Duration::from_millis(100);

async fn retain_turn_timing_artifact(
    mut timing: TurnTiming,
    artifact: tokio::task::JoinHandle<crate::tools::command_output_artifact::RawOutputArtifact>,
) -> TurnTiming {
    if let Ok(Ok(crate::tools::command_output_artifact::RawOutputArtifact::Stored {
        id,
        truncated: false,
        ..
    })) = tokio::time::timeout(TURN_TIMING_STORAGE_TIMEOUT, artifact).await
    {
        timing.tool_call_details_artifact_id = Some(id.to_string());
        for call in &mut timing.tool_calls {
            call.lifecycle_events.clear();
            call.timer_waits.clear();
        }
    }
    timing
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
    pub(crate) model_request_history_truncated: bool,
    pub(crate) tool_calls: Vec<TurnTimingToolCall>,
    pub(crate) tool_call_timing_overflow: u32,
    pub(crate) tool_closure: TurnTimingToolClosure,
    pub(crate) deterministic_continuation_receipts: Vec<TurnTimingDeterministicContinuationReceipt>,
    pub(crate) deterministic_continuation_receipt_overflow: u32,
    pub(crate) pre_first_model_output: Option<PreFirstModelOutputTiming>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ModelRequestTiming {
    stream_delivery_by_attempt: BTreeMap<String, String>,
    response_queue_timing_by_attempt:
        BTreeMap<String, BTreeMap<String, codex_protocol::timing::ResponseQueueTiming>>,
    setup_phases: Option<ClientCriticalPhaseTiming>,
    request_sha256_by_attempt: BTreeMap<String, String>,
    request_section_sha256_by_attempt: BTreeMap<String, BTreeMap<String, String>>,
    response_id_by_attempt: BTreeMap<String, String>,
    generation_index: u32,
    generation_reason: TurnTimingGenerationReason,
    generation_purpose: Option<TurnTimingGenerationPurpose>,
    disposition: TurnTimingGenerationDisposition,
    relevant_state_fingerprint: Option<String>,
    failure_fingerprint: Option<String>,
    sampling_request_id: Option<String>,
    estimator_model: Option<String>,
    /// Registered once; lower HTTP retries share this logical measurement.
    diagnostics_owner_attempt_id: Option<String>,
    physical_attempt_ids: Vec<String>,
    dispatched_attempt_ids: BTreeSet<String>,
    dispatched_attempt_count: u32,
    progress_kinds: Vec<TurnTimingProgressKind>,
    structured_action_fingerprint: Option<String>,
    next_structured_action_changed: bool,
    unchanged_relevant_state: bool,
    final_answer_emitted: bool,
    unphased_message_emitted: bool,
    attempt_kind: TurnTimingAttemptKind,
    model_stream_wait_ns: u128,
    tool_active_union_ns: u128,
    tool_call_count: u32,
    model_emitted_tool_call_count: u32,
    executor_admitted_tool_call_count: u32,
    executor_max_concurrent_tool_calls: u32,
    executor_active_tool_call_count: u32,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    token_usage: Option<TurnTimingProviderTokenUsage>,
    request_token_categories: Option<TurnTimingRequestTokenCategories>,
    request_diagnostics_status: TurnTimingRequestDiagnosticsStatus,
    fixed_prefix_reuse_eligible: Option<bool>,
    prompt_cache_key_fingerprint: Option<String>,
    dispatch_ns: Option<u128>,
    first_model_output_ns: Option<u128>,
    first_actionable_output_ns: Option<u128>,
    completed_ns: Option<u128>,
    is_continuation: bool,
}

impl ModelRequestTiming {
    fn observe_token_usage(&mut self, usage: &TokenUsage) {
        let output_tokens = u64::try_from(usage.output_tokens.max(0)).unwrap_or(u64::MAX);
        let reasoning_tokens =
            u64::try_from(usage.reasoning_output_tokens.max(0)).unwrap_or(u64::MAX);
        self.output_tokens = output_tokens;
        self.reasoning_output_tokens = reasoning_tokens;
        self.token_usage = Some(TurnTimingProviderTokenUsage {
            coverage: usage.coverage,
            output_excluding_reported_reasoning_tokens: usage
                .known_visible_output()
                .and_then(|tokens| u64::try_from(tokens).ok()),
            input_tokens: u64::try_from(usage.input_tokens.max(0)).unwrap_or(u64::MAX),
            cached_input_tokens: u64::try_from(usage.cached_input_tokens.max(0))
                .unwrap_or(u64::MAX),
            visible_output_tokens: output_tokens.saturating_sub(reasoning_tokens),
            reasoning_tokens,
            total_tokens: u64::try_from(usage.total_tokens.max(0)).unwrap_or(u64::MAX),
        });
    }

    fn physical_attempt_count(&self) -> usize {
        self.dispatched_attempt_count as usize
    }

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
    preparation_ns: u128,
    router_build_ns: u128,
    executor_readiness_wait_ns: u128,
    startup_prewarm_wait_ns: u128,
    persistence_ns: u128,
    history_snapshot_ns: u128,
    normalization_ns: u128,
    prompt_construction_ns: u128,
    request_transformation_ns: u128,
    serialization_ns: u128,
    transport_readiness_ns: u128,
}

impl ClientCriticalPhaseTiming {
    fn protocol_phases(&self, saturation_count: &mut u32) -> BTreeMap<String, u64> {
        [
            ("preparation", self.preparation_ns),
            ("router_build", self.router_build_ns),
            ("executor_readiness_wait", self.executor_readiness_wait_ns),
            ("startup_prewarm_wait", self.startup_prewarm_wait_ns),
            ("persistence", self.persistence_ns),
            ("history_snapshot", self.history_snapshot_ns),
            ("normalization", self.normalization_ns),
            ("prompt_construction", self.prompt_construction_ns),
            ("request_transformation", self.request_transformation_ns),
            ("serialization", self.serialization_ns),
            ("transport_readiness", self.transport_readiness_ns),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), public_ns(value, saturation_count)))
        .collect()
    }
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
    turn_worker_started_ns: Option<u128>,
    first_request_preparation_started_ns: Option<u128>,
    pub(crate) user_input_recorded_ns: Option<u128>,
    pub(crate) first_tool_accepted_ns: Option<u128>,
    pub(crate) first_tool_gate_admitted_ns: Option<u128>,
    pub(crate) first_tool_handler_entry_ns: Option<u128>,
    pub(crate) first_useful_tool_accepted_ns: Option<u128>,
    pub(crate) first_useful_tool_gate_admitted_ns: Option<u128>,
    pub(crate) first_useful_action_ns: Option<u128>,
    pub(crate) first_successful_useful_action_ns: Option<u128>,
    pub(crate) first_infrastructure_action_ns: Option<u128>,
    pub(crate) first_tool_discovery_action_ns: Option<u128>,
    pub(crate) first_domain_action_ns: Option<u128>,
    pub(crate) first_successful_domain_action_ns: Option<u128>,
    pub(crate) first_model_output_ns: Option<u128>,
    pub(crate) first_model_content_ns: Option<u128>,
    pub(crate) first_actionable_output_ns: Option<u128>,
    pub(crate) first_visible_output_ns: Option<u128>,
    pub(crate) first_agent_message_ns: Option<u128>,
    pub(crate) first_assistant_text_ns: Option<u128>,
    first_assistant_text_item_id: Option<String>,
    first_assistant_text_phase: Option<codex_protocol::models::MessagePhase>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TimingCounters {
    pub(crate) logical_generation_count: u32,
    pub(crate) generations_by_reason: TurnTimingGenerationReasonCounts,
    pub(crate) generations_by_purpose: TurnTimingGenerationPurposeCounts,
    pub(crate) generations_by_disposition: TurnTimingGenerationDispositionCounts,
    pub(crate) suppressed_deterministic_continuation_count: u32,
    pub(crate) residual_deterministic_generation_count: Option<u32>,
    pub(crate) owner_drained_continuation_count: u32,
    pub(crate) executed_validation_count: u32,
    pub(crate) executed_validation_duration_ns: u64,
    pub(crate) ready_startup_prewarm_count: u32,
    pub(crate) same_purpose_continuation_count: u32,
    pub(crate) planning_generation_count: u32,
    pub(crate) plan_revision_generation_count: u32,
    pub(crate) planning_fixed_point_iteration_count: u32,
    pub(crate) planning_invalidation_count: u32,
    pub(crate) planning_semantic_effect_count: u32,
    pub(crate) planning_failure_count: u32,
    pub(crate) attempts_by_kind: TurnTimingAttemptKindCounts,
    pub(crate) model_request_detail_eviction_count: u32,
    pub(crate) physical_attempt_detail_eviction_count: u32,
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
    pub(crate) tool_router_reuse_count: u32,
    pub(crate) tool_router_rebuild_count: u32,
    pub(crate) projection_source_dependencies_reuse_count: u32,
    pub(crate) projection_source_dependencies_fallback_count: u32,
    pub(crate) tool_output_truncation_count: u32,
    pub(crate) tool_output_projected_token_count: u64,
    pub(crate) tool_output_artifact_reread_count: u32,
    pub(crate) tool_output_canonical_byte_count: u64,
    pub(crate) tool_output_canonical_token_count: u64,
    pub(crate) tool_output_model_byte_count: u64,
    pub(crate) tool_output_model_token_count: u64,
    pub(crate) tool_output_artifact_creation_count: u32,
    pub(crate) tool_output_artifact_reuse_count: u32,
    pub(crate) tool_output_projection_truncation_count: u32,
    pub(crate) tool_output_omitted_section_count: u64,
    pub(crate) tool_output_recovery_call_count: u32,
    pub(crate) tool_output_in_cell_recovery_call_count: u32,
    pub(crate) tool_output_recovery_retruncation_count: u32,
    pub(crate) attributable_recovery_generation_count: u32,
    pub(crate) truncation_induced_continuation_count: u32,
    pub(crate) invalid_transition_count: u32,
    pub(crate) clock_regression_count: u32,
    pub(crate) saturation_count: u32,
}

#[derive(Clone, Debug, Default)]
struct TurnTimingStateInner {
    retry_decisions: retries::RetryDecisionLedger,
    activity_timeline: codex_protocol::timing::TurnTimingTimeline,
    final_content: FinalContentObservations,
    starting_credit_balance: Option<f64>,
    last_credit_balance: Option<f64>,
    started_sample: Option<ClockSample>,
    last_monotonic_ns: Option<u128>,
    activity: ActiveSet,
    // Only live origins are retained. Elapsed unions are flushed to their
    // bounded request row on quiescence, or projected during a checkpoint.
    active_tool_origins: BTreeMap<u32, (u32, u128)>,
    exclusive: ExclusiveTiming,
    unions: TimingUnions,
    local: LocalTiming,
    milestones: TimingMilestones,
    counters: TimingCounters,
    model_requests: Vec<ModelRequestTiming>,
    model_request_history_truncated: bool,
    accounted_model_responses: u64,
    tool_calls: Vec<TurnTimingToolCall>,
    tool_call_timing_overflow: u32,
    /// Model-visible direct results without a (capped) detail row. The next
    /// dispatched request counts their consumes_tool_result edges as dropped.
    unrecorded_tool_result_consumptions: u64,
    tool_closure: ToolClosureLedger,
    queued_durable_tool_result_execution_ids: BTreeSet<ToolExecutionId>,
    tool_result_persistence_barrier_failed: bool,
    tool_call_acceptance_sealed: bool,
    background_tool_process_exits: BTreeMap<(String, ToolExecutionId), ToolDispatchTimingSnapshot>,
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
    pending_request_setup: Option<ClientCriticalPhaseTiming>,
    dispatch_ready_snapshot: Option<(u128, u128, ClientCriticalPhaseTiming)>,
    ready_to_sample_ns: Option<u128>,
    pre_first_model_output: Option<PreFirstModelOutputTiming>,
    tool_output_truncation_pending_continuation: bool,
    tool_output_recovery_read_pending_continuation: bool,
    deterministic_continuation_receipts:
        BTreeMap<String, TurnTimingDeterministicContinuationReceipt>,
    deterministic_continuation_receipt_overflow: u32,
}

/// Item id, first-content ms, full-content ms and content SHA-256.
type FinalContentObservation = (String, Option<u64>, Option<u64>, Option<String>);

#[derive(Clone, Debug, Default)]
struct FinalContentObservations {
    active_item: Option<String>,
    // Bounded by candidates, not final text length. No content is retained.
    items: VecDeque<FinalContentObservation>,
    direct_ms: Option<u64>,
}

impl FinalContentObservations {
    fn observe(&mut self, event: &ResponseEvent, elapsed_ms: u64) {
        match event {
            ResponseEvent::OutputItemAdded(ResponseItem::Message {
                id: Some(id), role, ..
            }) if role == "assistant" => {
                self.active_item = Some(id.to_string());
                if !self.items.iter().any(|entry| entry.0 == id.to_string()) {
                    if self.items.len() == 32 {
                        self.items.pop_front();
                    }
                    let first = raw_assistant_output_text_from_item(match event {
                        ResponseEvent::OutputItemAdded(item) => item,
                        _ => unreachable!(),
                    })
                    .filter(|text| !text.is_empty())
                    .map(|_| elapsed_ms);
                    self.items.push_back((id.to_string(), first, None, None));
                }
            }
            ResponseEvent::OutputItemAdded(_) | ResponseEvent::Completed { .. } => {
                self.active_item = None;
            }
            ResponseEvent::OutputTextDelta(delta) if !delta.is_empty() => {
                if let Some(entry) = self
                    .items
                    .iter_mut()
                    .find(|entry| Some(&entry.0) == self.active_item.as_ref())
                {
                    entry.1.get_or_insert(elapsed_ms);
                }
            }
            ResponseEvent::OutputItemDone(
                item @ ResponseItem::Message {
                    id: Some(id), role, ..
                },
            ) if role == "assistant" => {
                if !self.items.iter().any(|entry| entry.0 == id.to_string()) {
                    if self.items.len() == 32 {
                        self.items.pop_front();
                    }
                    self.items.push_back((id.to_string(), None, None, None));
                }
                if let Some(entry) = self
                    .items
                    .iter_mut()
                    .find(|entry| entry.0 == id.to_string())
                    && let Some(text) = raw_assistant_output_text_from_item(item)
                {
                    let digest = hex::encode(Sha256::digest(text.as_bytes()));
                    if entry.3.as_ref().is_some_and(|previous| *previous != digest) {
                        // Reused identity with different content is not an
                        // idempotent replay of the earlier full payload.
                        entry.1 = None;
                        entry.2 = Some(elapsed_ms);
                    }
                    if !text.is_empty() {
                        entry.1.get_or_insert(elapsed_ms);
                    }
                    entry.2.get_or_insert(elapsed_ms);
                    entry.3 = Some(digest);
                }
                self.active_item = None;
            }
            _ => {}
        }
    }
}

impl TurnTimingStateInner {
    /// Clone only the live observation inputs, not correctness history or tool
    /// detail that the checkpoint projection deliberately omits.
    fn checkpoint_clone(&self) -> Self {
        if self.completed_snapshot.is_some() {
            return self.clone();
        }
        Self {
            retry_decisions: self.retry_decisions.clone(),
            final_content: self.final_content.clone(),
            activity_timeline: self.activity_timeline.clone(),
            starting_credit_balance: self.starting_credit_balance,
            last_credit_balance: self.last_credit_balance,
            started_sample: self.started_sample,
            last_monotonic_ns: self.last_monotonic_ns,
            activity: self.activity,
            active_tool_origins: self.active_tool_origins.clone(),
            exclusive: self.exclusive.clone(),
            unions: self.unions.clone(),
            local: self.local.clone(),
            milestones: self.milestones.clone(),
            counters: self.counters.clone(),
            model_requests: self.model_requests.clone(),
            model_request_history_truncated: self.model_request_history_truncated,
            accounted_model_responses: self.accounted_model_responses,
            tool_calls: Vec::new(),
            tool_call_timing_overflow: self.tool_call_timing_overflow,
            unrecorded_tool_result_consumptions: self.unrecorded_tool_result_consumptions,
            tool_closure: ToolClosureLedger::default(),
            queued_durable_tool_result_execution_ids: BTreeSet::new(),
            tool_result_persistence_barrier_failed: self.tool_result_persistence_barrier_failed,
            tool_call_acceptance_sealed: self.tool_call_acceptance_sealed,
            background_tool_process_exits: BTreeMap::new(),
            current_generation_index: self.current_generation_index,
            current_generation_reason: self.current_generation_reason,
            current_generation_purpose: self.current_generation_purpose,
            current_generation_disposition: self.current_generation_disposition,
            current_relevant_state_fingerprint: self.current_relevant_state_fingerprint.clone(),
            current_failure_fingerprint: self.current_failure_fingerprint.clone(),
            next_attempt_kind: self.next_attempt_kind,
            legacy: self.legacy.clone(),
            completed_snapshot: self.completed_snapshot.clone(),
            attributed_client_union_ns: self.attributed_client_union_ns,
            client_critical_phases: self.client_critical_phases.clone(),
            pending_request_setup: self.pending_request_setup.clone(),
            dispatch_ready_snapshot: self.dispatch_ready_snapshot.clone(),
            ready_to_sample_ns: self.ready_to_sample_ns,
            pre_first_model_output: self.pre_first_model_output.clone(),
            tool_output_truncation_pending_continuation: self
                .tool_output_truncation_pending_continuation,
            tool_output_recovery_read_pending_continuation: self
                .tool_output_recovery_read_pending_continuation,
            deterministic_continuation_receipts: self.deterministic_continuation_receipts.clone(),
            deterministic_continuation_receipt_overflow: self
                .deterministic_continuation_receipt_overflow,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ToolClosureEntry {
    identity: Arc<TurnTimingToolCallIdentity>,
    accepted_at_ns: Option<u128>,
    timing_paired: bool,
    terminal: bool,
    persisted: bool,
    /// The exact exec/wait packet that carried this settled nested call.
    delivering_projection: Option<ToolExecutionId>,
}

impl ToolClosureEntry {
    fn new(identity: Arc<TurnTimingToolCallIdentity>) -> Self {
        Self {
            identity,
            accepted_at_ns: None,
            timing_paired: false,
            terminal: false,
            persisted: false,
            delivering_projection: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ToolClosureLedger {
    entries: BTreeMap<ToolExecutionId, ToolClosureEntry>,
    // Collision buckets reference the same immutable identity as entries.
    call_index: BTreeMap<u64, Vec<Arc<TurnTimingToolCallIdentity>>>,
    call_key_hasher: std::collections::hash_map::RandomState,
    orphan_calls: BTreeMap<ToolExecutionId, ToolClosureEntry>,
    duplicate_call_id_count: u32,
    duplicate_acceptance_count: u32,
    duplicate_timing_count: u32,
    duplicate_persistence_count: u32,
    overflow_count: u32,
}

impl ToolClosureLedger {
    fn call_key(
        &self,
        call_id: &str,
        generation: &SamplingGenerationId,
        source: TurnTimingToolCallSource,
    ) -> u64 {
        std::hash::BuildHasher::hash_one(
            &self.call_key_hasher,
            (
                generation.0.as_str(),
                source == TurnTimingToolCallSource::CodeMode,
                call_id,
            ),
        )
    }

    fn has_accepted_call(
        &self,
        call_id: &str,
        generation: &SamplingGenerationId,
        source: TurnTimingToolCallSource,
    ) -> bool {
        self.call_index
            .get(&self.call_key(call_id, generation, source))
            .is_some_and(|bucket| {
                bucket.iter().any(|identity| {
                    identity.call_id == call_id
                        && &identity.sampling_generation_id == generation
                        && identity.source == source
                })
            })
    }

    fn reject_duplicate_call_id(
        &mut self,
        call_id: &str,
        sampling_generation_id: &SamplingGenerationId,
        source: TurnTimingToolCallSource,
    ) -> bool {
        let duplicate = self.has_accepted_call(call_id, sampling_generation_id, source);
        if duplicate {
            self.duplicate_call_id_count = self.duplicate_call_id_count.saturating_add(1);
        }
        duplicate
    }

    fn register_accepted(&mut self, identity: TurnTimingToolCallIdentity) -> bool {
        let execution_id = identity.execution_id.clone();
        if self.entries.contains_key(&execution_id) {
            self.duplicate_acceptance_count = self.duplicate_acceptance_count.saturating_add(1);
            return true;
        }
        if self.has_accepted_call(
            &identity.call_id,
            &identity.sampling_generation_id,
            identity.source,
        ) {
            self.duplicate_call_id_count = self.duplicate_call_id_count.saturating_add(1);
            return false;
        }
        let key = self.call_key(
            &identity.call_id,
            &identity.sampling_generation_id,
            identity.source,
        );
        let identity = Arc::new(identity);
        let entry = match self.orphan_calls.remove(&execution_id) {
            Some(mut entry) => {
                entry.identity = Arc::clone(&identity);
                entry
            }
            None => ToolClosureEntry::new(Arc::clone(&identity)),
        };
        self.call_index.entry(key).or_default().push(identity);
        self.entries.insert(execution_id, entry);
        true
    }

    fn record_timing(&mut self, identity: TurnTimingToolCallIdentity, terminal: bool) {
        let execution_id = identity.execution_id.clone();
        let entry = if let Some(entry) = self.entries.get_mut(&execution_id) {
            entry
        } else {
            self.orphan_calls
                .entry(execution_id)
                .or_insert_with(|| ToolClosureEntry::new(Arc::new(identity)))
        };
        if entry.timing_paired {
            self.duplicate_timing_count = self.duplicate_timing_count.saturating_add(1);
        } else {
            entry.timing_paired = true;
        }
        entry.terminal |= terminal;
    }

    fn record_persisted(&mut self, identity: TurnTimingToolCallIdentity) {
        let execution_id = identity.execution_id.clone();
        let entry = if let Some(entry) = self.entries.get_mut(&execution_id) {
            entry
        } else {
            self.orphan_calls
                .entry(execution_id)
                .or_insert_with(|| ToolClosureEntry::new(Arc::new(identity)))
        };
        if entry.persisted {
            self.duplicate_persistence_count = self.duplicate_persistence_count.saturating_add(1);
        } else {
            entry.persisted = true;
        }
        // A durably persisted direct result is a terminal outcome even when a
        // failed task join prevented its dispatch guard from recording one.
        entry.terminal = true;
    }

    fn record_nested_projection_persisted(&mut self, projection: &TurnTimingToolCallIdentity) {
        for entry in self.entries.values_mut().filter(|entry| {
            entry.identity.source == TurnTimingToolCallSource::CodeMode
                && entry.delivering_projection.as_ref() == Some(&projection.execution_id)
        }) {
            if entry.persisted {
                self.duplicate_persistence_count =
                    self.duplicate_persistence_count.saturating_add(1);
            } else {
                entry.persisted = true;
            }
            // Nested CodeMode results are carried inside their owning direct
            // call's canonical projection, or inside the later `wait`
            // projection that delivered them. Once that projection is
            // persisted, every child represented by it has a terminal
            // model-visible outcome even if cancellation dropped the child
            // future before its timing guard could attach an explicit outcome
            // label.
            entry.terminal = true;
        }
    }

    /// Packet formation, not generation membership or acknowledgment time,
    /// determines which nested outcomes a direct result can attest.
    fn bind_nested_results_to_projection(
        &mut self,
        projection_execution_id: &ToolExecutionId,
        delivered_call_ids: &[String],
    ) {
        let delivered_call_ids = delivered_call_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        for entry in self.entries.values_mut().filter(|entry| {
            entry.identity.source == TurnTimingToolCallSource::CodeMode
                && delivered_call_ids.contains(entry.identity.call_id.as_str())
                && !entry.persisted
                && entry.delivering_projection.is_none()
        }) {
            entry.delivering_projection = Some(projection_execution_id.clone());
        }
    }

    fn record_persisted_execution(&mut self, execution_id: &ToolExecutionId) {
        let Some(identity) = self
            .entries
            .get(execution_id)
            .filter(|entry| {
                entry.identity.source == TurnTimingToolCallSource::Direct && !entry.persisted
            })
            .map(|entry| entry.identity.as_ref().clone())
        else {
            return;
        };
        self.record_nested_projection_persisted(&identity);
        self.record_persisted(identity);
    }

    fn repair_persisted_terminal_entries_missing_timing(&mut self) -> u32 {
        let mut repaired = 0_u32;
        for entry in self
            .entries
            .values_mut()
            .filter(|entry| entry.persisted && entry.terminal && !entry.timing_paired)
        {
            // Durable missing-output repair is authoritative evidence that the
            // accepted call reached a terminal projection. A dropped dispatch
            // guard may have lost only the timing-pair bit; restoring that bit
            // must not erase duplicates, orphans, overflow, or missing output.
            entry.timing_paired = true;
            repaired = repaired.saturating_add(1);
        }
        repaired
    }

    fn snapshot(&self) -> TurnTimingToolClosure {
        self.snapshot_with_checkpoint_filter(false)
    }

    fn snapshot_for_checkpoint(&self) -> TurnTimingToolClosure {
        self.snapshot_with_checkpoint_filter(true)
    }

    fn snapshot_with_checkpoint_filter(&self, checkpoint: bool) -> TurnTimingToolClosure {
        let count = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
        let (mut timing_paired_count, mut terminal_count, mut persisted_count) =
            (0usize, 0usize, 0usize);
        let mut unresolved_calls = Vec::new();
        let mut all_settled = true;
        for entry in self.entries.values() {
            timing_paired_count += usize::from(entry.timing_paired);
            terminal_count += usize::from(entry.terminal);
            persisted_count += usize::from(entry.persisted);
            let settled = entry.timing_paired && entry.terminal && entry.persisted;
            all_settled &= settled;
            if !settled && (!checkpoint || !(entry.timing_paired && entry.terminal)) {
                unresolved_calls.push(entry.identity.as_ref().clone());
            }
        }
        let (mut orphan_timing_count, mut orphan_persistence_count) = (0usize, 0usize);
        let mut orphan_calls = Vec::new();
        for entry in self.orphan_calls.values() {
            orphan_timing_count += usize::from(entry.timing_paired);
            orphan_persistence_count += usize::from(entry.persisted);
            orphan_calls.push(entry.identity.as_ref().clone());
        }
        let complete = all_settled
            && orphan_calls.is_empty()
            && self.duplicate_call_id_count == 0
            && self.duplicate_acceptance_count == 0
            && self.duplicate_timing_count == 0
            && self.duplicate_persistence_count == 0
            && self.overflow_count == 0;
        TurnTimingToolClosure {
            measurement_available: true,
            accepted_count: count(self.entries.len()),
            timing_paired_count: count(timing_paired_count),
            terminal_count: count(terminal_count),
            persisted_count: count(persisted_count),
            duplicate_call_id_count: self.duplicate_call_id_count,
            duplicate_acceptance_count: self.duplicate_acceptance_count,
            duplicate_timing_count: self.duplicate_timing_count,
            duplicate_persistence_count: self.duplicate_persistence_count,
            orphan_timing_count: count(orphan_timing_count),
            orphan_persistence_count: count(orphan_persistence_count),
            overflow_count: self.overflow_count,
            unresolved_calls,
            orphan_calls,
            complete,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ActiveSet {
    observed_processes: u32,
    model: u32,
    model_request_wait: u32,
    provider_request_dispatched: bool,
    provider_terminal_observed: bool,
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

    /// A request wait opens before the client prepares its physical attempt.
    /// Transport readiness closes at dispatch-ready, so a request wait it
    /// overlaps is client-side setup the provider has not received yet.
    fn provider_request_wait(self) -> bool {
        self.model_request_wait > 0
            && self.provider_request_dispatched
            && self.transport_readiness == 0
    }

    /// Model time the provider can be serving. Validation still counts
    /// pre-dispatch request setup as explicit machine activity.
    fn provider_model_active(self) -> bool {
        !self.provider_terminal_observed && (self.provider_request_wait()
            || self.model_stream_wait > 0
            || self.model_stream_processing > 0)
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
        let model = self.provider_model_active();
        if model && self.tool > 0 {
            return ExclusivePhase::ModelToolOverlap;
        }
        if model {
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

impl ExclusivePhase {
    fn wire_name(self) -> &'static str {
        match self {
            Self::ModelOnly => "modelOnly",
            Self::ToolOnly => "toolOnly",
            Self::ModelToolOverlap => "modelPlusTool",
            Self::InteractiveOnly => "interactiveOnlyWait",
            Self::InteractiveMachineOverlap => "interactivePlusMachine",
            Self::RetryOnly => "retryOnly",
            Self::Orchestration => "orchestration",
            Self::StandaloneWork => "standaloneWork",
            Self::Finalization => "finalization",
            Self::Unclassified => "unclassified",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GuardKind {
    LegacySampling,
    LegacyToolBlocking,
    ModelRequestWait,
    ModelStreamWait,
    ModelStreamProcessing,
    ToolExecution(Option<u32>),
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

/// Temporarily attributes a non-preparation phase outside the request-preparation lane.
/// Dropping the guard restores preparation only when it was active before the operation.
#[must_use]
pub(crate) struct TurnPreparationPhaseGuard<'a> {
    timing: Arc<TurnTimingState>,
    preparation: &'a mut Option<TurnTimingGuard>,
    phase: TurnLocalPhase,
    phase_guard: Option<TurnTimingGuard>,
    resume_preparation: bool,
}

impl Drop for TurnPreparationPhaseGuard<'_> {
    fn drop(&mut self) {
        let Some(mut phase_guard) = self.phase_guard.take() else {
            return;
        };
        if self.resume_preparation && phase_guard.active {
            phase_guard.active = false;
            let active = self
                .timing
                .transition_local_phase(self.phase, TurnLocalPhase::Preparation);
            *self.preparation = Some(TurnTimingGuard {
                timing: Arc::clone(&self.timing),
                kind: GuardKind::Local(TurnLocalPhase::Preparation),
                active,
            });
        }
    }
}

impl TurnTimingState {
    pub(crate) fn observe_credits(
        &self,
        credits: Option<&codex_protocol::protocol::CreditsSnapshot>,
    ) {
        let Some(credits) = credits.filter(|credits| !credits.unlimited) else {
            return;
        };
        let Some(balance) = credits
            .balance
            .as_deref()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0)
        else {
            return;
        };
        let mut state = self.state();
        if state.completed_snapshot.is_some() {
            return;
        }
        state.starting_credit_balance.get_or_insert(balance);
        state.last_credit_balance = Some(balance);
    }

    fn new(clock: Arc<dyn TurnClock>) -> Self {
        Self {
            live_phase: Default::default(),
            clock,
            state: StdMutex::new(TurnTimingStateInner::default()),
            relay_queue_depth: AtomicU32::new(0),
            parallel_gate_waiter_count: AtomicU32::new(0),
            sampling_gate_waiter_count: AtomicU32::new(0),
            process_output_waiter_count: AtomicU32::new(0),
            active_tool_count: AtomicU32::new(0),
            tool_closure_changed: Notify::new(),
            checkpointed_entries: Default::default(),
            model_usage_receipts: Default::default(),
            canonical_input_section: Default::default(),
        }
    }

    pub(crate) fn bind_live_phase_events(
        &self,
        turn_id: String,
        sender: async_channel::Sender<codex_protocol::protocol::Event>,
    ) {
        self.live_phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sink = Some((turn_id, sender));
    }

    pub(crate) fn adjust_live_gate(&self, gate: usize, entering: bool) {
        let mut live = self
            .live_phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        live.gates[gate] = if entering {
            live.gates[gate].saturating_add(1)
        } else {
            live.gates[gate].saturating_sub(1)
        };
        live.publish();
    }

    fn set_live_sampling_phase(&self, reason: Option<NextSampleBlockReason>) {
        let mut live = self
            .live_phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        live.sampling = reason;
        live.publish();
    }

    /// Returns a millisecond offset from the turn's sole monotonic clock.
    pub(crate) fn monotonic_offset_ms(&self) -> u64 {
        let state = self.state();
        let sample = self.clock.sample();
        let now = if state.completed_snapshot.is_some() {
            state.last_monotonic_ns.unwrap_or(sample.time.monotonic_ns)
        } else {
            sample
                .time
                .monotonic_ns
                .max(state.last_monotonic_ns.unwrap_or(0))
        };
        state
            .started_sample
            .map(|start| now.saturating_sub(start.time.monotonic_ns))
            .map(u128_to_u64_ms)
            .unwrap_or(0)
    }

    pub(crate) fn sampling_generation_id(&self) -> String {
        self.state()
            .current_generation_index
            .map(|index| format!("generation-{index}"))
            .unwrap_or_else(|| "generation-pending".to_string())
    }

    pub(crate) fn lifecycle_context(&self) -> ToolLifecycleContext {
        ToolLifecycleContext {
            relay_queue_depth: self.relay_queue_depth.load(Ordering::Acquire),
            parallel_gate_waiter_count: self.parallel_gate_waiter_count.load(Ordering::Acquire),
            sampling_gate_waiter_count: self.sampling_gate_waiter_count.load(Ordering::Acquire),
            process_output_waiter_count: self.process_output_waiter_count.load(Ordering::Acquire),
            active_tool_count: self.active_tool_count.load(Ordering::Acquire),
        }
    }

    pub(crate) fn adjust_relay_queue_depth(&self, delta: i32) {
        self.adjust_counter(&self.relay_queue_depth, delta);
    }

    pub(crate) fn adjust_parallel_gate_waiters(&self, delta: i32) {
        self.adjust_counter(&self.parallel_gate_waiter_count, delta);
    }

    pub(crate) fn adjust_sampling_gate_waiters(&self, delta: i32) {
        self.adjust_counter(&self.sampling_gate_waiter_count, delta);
    }

    pub(crate) fn adjust_process_output_waiters(&self, delta: i32) {
        self.adjust_counter(&self.process_output_waiter_count, delta);
    }

    pub(crate) fn adjust_active_tools(&self, delta: i32) {
        self.adjust_counter(&self.active_tool_count, delta);
    }

    #[expect(
        clippy::expect_used,
        reason = "The update closure unconditionally returns Some"
    )]
    fn adjust_counter(&self, counter: &AtomicU32, delta: i32) {
        let magnitude = delta.unsigned_abs();
        let previous = counter
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                Some(if delta >= 0 {
                    value.saturating_add(magnitude)
                } else {
                    value.saturating_sub(magnitude)
                })
            })
            .expect("counter updates always return a value");
        let exact = if delta >= 0 {
            previous.checked_add(magnitude)
        } else {
            previous.checked_sub(magnitude)
        };
        if exact.is_none() {
            self.state().saturated();
        }
    }

    pub(crate) fn mark_turn_started(&self) -> i64 {
        let mut state = self.state();
        let sample = self.clock.sample();
        let started_at = state.start(sample);
        state.activity_timeline.clock_domain_id = self.clock.domain_id().map(str::to_owned);
        started_at
    }

    pub(crate) fn mark_turn_worker_started(&self) {
        let mut state = self.state();
        let now = self.clock.sample().time.monotonic_ns;
        state.advance(now);
        if let Some(elapsed) = state.elapsed_since_start(now) {
            state
                .milestones
                .turn_worker_started_ns
                .get_or_insert(elapsed);
        }
    }

    /// Capture the dispatched bytes independently of optional post-dispatch
    /// token accounting, so cancelled/failed attempts still have an identity.
    pub(crate) fn record_model_request_payload(
        &self,
        sampling_request_id: &str,
        physical_attempt_id: &str,
        bytes: &[u8],
    ) {
        let hash = hex::encode(Sha256::digest(bytes));
        let mut state = self.state();
        if let Some(request) = state.model_requests.iter_mut().find(|request| {
            request.sampling_request_id.as_deref() == Some(sampling_request_id)
                && request
                    .physical_attempt_ids
                    .iter()
                    .any(|id| id == physical_attempt_id)
        }) {
            request
                .request_sha256_by_attempt
                .insert(physical_attempt_id.to_owned(), hash);
        }
    }

    /// Optional canonical section hashes belong to the existing blocking
    /// diagnostics worker, not the dispatch callback. Keep the wire-byte hash
    /// above available even when that worker is cancelled or fails.
    pub(crate) fn record_model_request_sections(
        &self,
        sampling_request_id: &str,
        physical_attempt_id: &str,
        bytes: &[u8],
    ) {
        let sections = serde_json::from_slice::<BTreeMap<String, &serde_json::value::RawValue>>(bytes)
            .ok()
            .map(|fields| {
                fields
                    .into_iter()
                    .filter_map(|(key, value)| {
                        let digest = if key == "input" {
                            CanonicalInputSection::hash(
                                &mut self.canonical_input_section.lock().unwrap_or_else(std::sync::PoisonError::into_inner), value,
                            ).or_else(|| serde_json::from_str::<serde_json::Value>(value.get()).ok()
                                .map(|value| hex::encode(Sha256::digest(value.to_string().as_bytes()))))
                        } else {
                            serde_json::from_str::<serde_json::Value>(value.get()).ok()
                                .map(|value| hex::encode(Sha256::digest(value.to_string().as_bytes())))
                        }?;
                        Some((key, digest))
                    })
                    .collect()
            });
        let mut state = self.state();
        if let Some(request) = state.model_requests.iter_mut().find(|request| {
            request.sampling_request_id.as_deref() == Some(sampling_request_id)
                && request
                    .physical_attempt_ids
                    .iter()
                    .any(|id| id == physical_attempt_id)
        }) && let Some(sections) = sections
        {
            request
                .request_section_sha256_by_attempt
                .insert(physical_attempt_id.to_owned(), sections);
        }
        state.refresh_completed_request_diagnostics();
    }

    pub(crate) fn record_model_response_id(&self, attempt_id: &str, response_id: &str) {
        let mut state = self.state();
        if let Some(request) = state.model_requests.iter_mut().rev().find(|request| {
            request
                .physical_attempt_ids
                .iter()
                .any(|id| id == attempt_id)
        }) {
            request
                .response_id_by_attempt
                .insert(attempt_id.to_owned(), response_id.to_owned());
        }
    }

    pub(crate) async fn started_at_unix_secs(&self) -> Option<i64> {
        self.state()
            .started_sample
            .map(|sample| sample.time.wall_unix_ms / 1_000)
    }

    pub(crate) fn complete_snapshot(&self) -> TurnTimingSnapshot {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.retry_decisions.finish(sample.time.monotonic_ns, None);
        state.complete(sample)
    }

    /// Freeze an observation without terminalizing live timers or task state.
    ///
    /// Scalar aggregates stay cumulative. Per-call details belong only to the
    /// final turn timing; aborted-turn recovery retains their counters instead.
    /// Request, receipt and blocking-interval entries already written unchanged
    /// by an earlier checkpoint of this turn are omitted, as are activity
    /// intervals before the previous checkpoint's last interval and dependency
    /// edges already written.
    pub(crate) fn sampling_checkpoint(&self) -> codex_protocol::protocol::SamplingTimingCheckpoint {
        let (mut observation, tool_closure, sample) = {
            let state = self.state();
            let sample = self.clock.sample();
            (
                state.checkpoint_clone(),
                state.tool_closure.snapshot_for_checkpoint(),
                sample,
            )
        };
        let mut timing = observation.complete(sample).protocol_timing();
        if let Some(timeline) = timing.activity_timeline.as_mut() {
            timeline.tail_unknown = true;
        }
        timing.tool_calls.clear();
        timing.tool_closure = tool_closure;
        {
            let mut checkpointed = self
                .checkpointed_entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let CheckpointedEntries {
                digests,
                model_requests,
                deterministic_continuation_receipts,
                activity_intervals,
                blocking_intervals,
                dependency_edges,
            } = &mut *checkpointed;
            retain_new_checkpoint_entries(&mut timing.model_requests, model_requests, digests);
            retain_new_checkpoint_entries(
                &mut timing.deterministic_continuation_receipts,
                deterministic_continuation_receipts,
                digests,
            );
            if let Some(timeline) = timing.activity_timeline.as_mut() {
                retain_new_activity_intervals(&mut timeline.intervals, activity_intervals);
                if let Some(evidence) = timeline.blocking.as_mut() {
                    retain_new_checkpoint_entries(&mut evidence.intervals, blocking_intervals, digests);
                    let total = evidence.dependencies.len();
                    evidence.dependencies.drain(..(*dependency_edges).min(total));
                    *dependency_edges = total;
                }
            }
        }
        codex_protocol::protocol::SamplingTimingCheckpoint {
            observed_at_unix_ms: sample.time.wall_unix_ms,
            tail_unknown: true,
            timing,
            runtime_identity: BTreeMap::new(),
            harness_build: None,
            incremental: true,
        }
    }

    pub(crate) fn begin_sampling(self: &Arc<Self>) -> TurnTimingGuard {
        self.begin_guard(GuardKind::LegacySampling)
    }

    pub(crate) fn record_sampling_retry(&self) {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        state.legacy.record_sampling_retry();
        state.record_model_retry();
    }

    pub(crate) fn record_model_retry(&self) {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        state.record_model_retry();
    }

    pub(crate) fn begin_model_generation_with_metadata(
        &self,
        pending: &mut Option<ContinuationCause>,
        session_source: &SessionSource,
        purpose: Option<TurnTimingGenerationPurpose>,
        disposition: TurnTimingGenerationDisposition,
        relevant_state_fingerprint: Option<String>,
    ) {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        let cause = pending.take();
        if let Some(cause) = cause {
            state.legacy.record_continuation(cause);
        }
        let truncated_tool_output = state.tool_output_truncation_pending_continuation;
        state.tool_output_truncation_pending_continuation = false;
        let recovery_read = state.tool_output_recovery_read_pending_continuation;
        state.tool_output_recovery_read_pending_continuation = false;
        if truncated_tool_output && matches!(cause, Some(ContinuationCause::ToolResult)) {
            state.counters.truncation_induced_continuation_count = state
                .counters
                .truncation_induced_continuation_count
                .saturating_add(1);
        }
        if recovery_read && matches!(cause, Some(ContinuationCause::ToolResult)) {
            state.counters.attributable_recovery_generation_count = state
                .counters
                .attributable_recovery_generation_count
                .saturating_add(1);
        }
        let reason = match (cause, session_source) {
            (Some(ContinuationCause::Compaction), _) => TurnTimingGenerationReason::Compaction,
            (_, SessionSource::SubAgent(SubAgentSource::Compact)) => {
                TurnTimingGenerationReason::Compaction
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
        self.begin_compaction_generation_with_purpose(
            TurnTimingGenerationPurpose::CompactionRecovery,
        );
    }

    pub(crate) fn begin_compaction_repair_generation(&self) {
        self.begin_compaction_generation_with_purpose(TurnTimingGenerationPurpose::Repair);
    }

    fn begin_compaction_generation_with_purpose(&self, purpose: TurnTimingGenerationPurpose) {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        state.start_generation(
            TurnTimingGenerationReason::Compaction,
            Some(purpose),
            TurnTimingGenerationDisposition::DecisionBearing,
            None,
        );
    }

    pub(crate) fn record_model_fallback(&self) {
        let mut state = self.state();
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
        request.diagnostics_owner_attempt_id.get_or_insert_with(|| physical_attempt_id.to_string());
        let mut detail_evicted = false;
        if !request
            .physical_attempt_ids
            .iter()
            .any(|existing| existing == physical_attempt_id)
        {
            if request.physical_attempt_ids.len() >= MAX_MODEL_REQUEST_PHYSICAL_ATTEMPT_IDS {
                let evicted = request.physical_attempt_ids.remove(0);
                request.dispatched_attempt_ids.remove(&evicted);
                request.request_sha256_by_attempt.remove(&evicted);
                request.request_section_sha256_by_attempt.remove(&evicted);
                request.response_id_by_attempt.remove(&evicted);
                request.response_queue_timing_by_attempt.remove(&evicted);
                request.stream_delivery_by_attempt.remove(&evicted);
                detail_evicted = true;
            }
            request
                .physical_attempt_ids
                .push(physical_attempt_id.to_string());
        }
        if detail_evicted {
            state.model_request_history_truncated = true;
            state.counters.physical_attempt_detail_eviction_count = state
                .counters
                .physical_attempt_detail_eviction_count
                .saturating_add(1);
        }
    }

    pub(crate) fn record_user_input(&self) {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if state.milestones.user_input_recorded_ns.is_none()
            && let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns)
        {
            state.milestones.user_input_recorded_ns = Some(elapsed_ns);
        }
    }

    /// Registers one exact direct or nested execution at the point where the
    /// runtime accepts ownership of it. Completion and abort publication use
    /// this ledger rather than aggregate call counts.
    pub(crate) fn try_record_accepted_tool_call(
        &self,
        call_id: &str,
        execution_id: &ToolExecutionId,
        source: TurnTimingToolCallSource,
        parent_call_id: Option<&str>,
        tool_name: &str,
    ) -> bool {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if state.completed_snapshot.is_some() || state.tool_call_acceptance_sealed {
            state.invalid_transition();
            return false;
        }
        let sampling_generation_id = SamplingGenerationId(
            state
                .current_generation_index
                .map(|index| format!("generation-{index}"))
                .unwrap_or_else(|| "generation-pending".to_string()),
        );
        let is_new = !state.tool_closure.entries.contains_key(execution_id);
        let accepted = state
            .tool_closure
            .register_accepted(TurnTimingToolCallIdentity {
                call_id: call_id.to_string(),
                execution_id: execution_id.clone(),
                source,
                parent_call_id: parent_call_id.map(str::to_string),
                sampling_generation_id,
            });
        if accepted && is_new {
            if let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns) {
                // A new acceptance just inserted this entry; record a missing one
                // as an invalid ledger transition rather than panicking.
                match state.tool_closure.entries.get_mut(execution_id) {
                    Some(entry) => entry.accepted_at_ns = Some(elapsed_ns),
                    None => state.invalid_transition(),
                }
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
        drop(state);
        if accepted {
            self.signal_tool_closure_changed();
        }
        accepted
    }

    pub(crate) fn reject_duplicate_tool_call_id_if_accepted(
        &self,
        call_id: &str,
        source: TurnTimingToolCallSource,
    ) -> bool {
        let mut state = self.state();
        let sampling_generation_id = SamplingGenerationId(
            state
                .current_generation_index
                .map(|index| format!("generation-{index}"))
                .unwrap_or_else(|| "generation-pending".to_string()),
        );
        let duplicate =
            state
                .tool_closure
                .reject_duplicate_call_id(call_id, &sampling_generation_id, source);
        drop(state);
        if duplicate {
            self.signal_tool_closure_changed();
        }
        duplicate
    }

    pub(crate) fn record_tool_call_acceptance_closed(&self) {
        self.state().tool_call_acceptance_sealed = true;
        self.signal_tool_closure_changed();
    }

    pub(crate) fn repair_terminal_tool_timing_after_durable_projection(&self) -> u32 {
        let mut state = self.state();
        if !state.tool_call_acceptance_sealed || state.completed_snapshot.is_some() {
            return 0;
        }
        let repaired = state
            .tool_closure
            .repair_persisted_terminal_entries_missing_timing();
        drop(state);
        if repaired > 0 {
            self.signal_tool_closure_changed();
        }
        repaired
    }

    pub(crate) async fn wait_for_tool_closure_after_seal(&self) -> TurnTimingToolClosure {
        loop {
            let changed = self.tool_closure_changed.notified();
            {
                let state = self.state();
                if state.tool_call_acceptance_sealed {
                    let snapshot = state.tool_closure.snapshot();
                    let lifecycle_closed = snapshot.unresolved_calls.is_empty()
                        && snapshot.orphan_calls.is_empty()
                        && snapshot.timing_paired_count == snapshot.accepted_count
                        && snapshot.terminal_count == snapshot.accepted_count
                        && snapshot.persisted_count == snapshot.accepted_count;
                    if lifecycle_closed || state.tool_result_persistence_barrier_failed {
                        return snapshot;
                    }
                }
            }
            changed.await;
        }
    }

    fn signal_tool_closure_changed(&self) {
        self.tool_closure_changed.notify_waiters();
        self.tool_closure_changed.notify_one();
    }

    pub(crate) fn record_tool_gate_admitted(&self, tool_name: &str) {
        let mut state = self.state();
        let sample = self.clock.sample();
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

    /// Records a direct tool call emitted by the model. Nested code-mode calls
    /// remain in the legacy aggregate but do not inflate this source-side
    /// serialization diagnostic.
    pub(crate) fn record_model_emitted_tool_call(&self) {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if let Some(generation_index) = state.current_generation_index
            && let Some(request) = state.model_requests.iter_mut().find(|request| {
                request.generation_index == generation_index
                    && request.attempt_kind == TurnTimingAttemptKind::Primary
            })
        {
            request.model_emitted_tool_call_count =
                request.model_emitted_tool_call_count.saturating_add(1);
        }
    }

    /// Records executor gate admission and the exact peak number of direct
    /// model-issued calls held concurrently for the owning generation.
    pub(crate) fn record_model_tool_gate_admitted(&self) {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if let Some(generation_index) = state.current_generation_index
            && let Some(request) = state.model_requests.iter_mut().find(|request| {
                request.generation_index == generation_index
                    && request.attempt_kind == TurnTimingAttemptKind::Primary
            })
        {
            request.executor_admitted_tool_call_count =
                request.executor_admitted_tool_call_count.saturating_add(1);
            request.executor_active_tool_call_count =
                request.executor_active_tool_call_count.saturating_add(1);
            request.executor_max_concurrent_tool_calls = request
                .executor_max_concurrent_tool_calls
                .max(request.executor_active_tool_call_count);
        }
    }

    pub(crate) fn record_model_tool_gate_released(&self) {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if let Some(generation_index) = state.current_generation_index
            && let Some(request) = state.model_requests.iter_mut().find(|request| {
                request.generation_index == generation_index
                    && request.attempt_kind == TurnTimingAttemptKind::Primary
            })
        {
            request.executor_active_tool_call_count =
                request.executor_active_tool_call_count.saturating_sub(1);
        }
    }

    pub(crate) fn record_tool_handler_entry(&self, tool_name: &str) {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns) {
            state
                .milestones
                .first_tool_handler_entry_ns
                .get_or_insert(elapsed_ns);
            match tool_action_class(tool_name) {
                ToolActionClass::Infrastructure => {
                    state
                        .milestones
                        .first_infrastructure_action_ns
                        .get_or_insert(elapsed_ns);
                }
                ToolActionClass::Discovery => {
                    state
                        .milestones
                        .first_tool_discovery_action_ns
                        .get_or_insert(elapsed_ns);
                }
                ToolActionClass::Domain => {
                    state
                        .milestones
                        .first_domain_action_ns
                        .get_or_insert(elapsed_ns);
                    state
                        .milestones
                        .first_useful_action_ns
                        .get_or_insert(elapsed_ns);
                }
            }
        }
    }

    pub(crate) fn record_tool_completion(&self, tool_name: &str, successful: bool) {
        if !successful || !tool_counts_as_useful_first_action(tool_name) {
            return;
        }
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if state.milestones.first_successful_useful_action_ns.is_none()
            && let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns)
        {
            state.milestones.first_successful_useful_action_ns = Some(elapsed_ns);
            state.milestones.first_successful_domain_action_ns = Some(elapsed_ns);
        }
    }

    pub(crate) fn record_tool_dispatch_timing(
        &self,
        call_id: &str,
        tool_name: &str,
        source: TurnTimingToolCallSource,
        lineage: ToolCallTimingLineage<'_>,
        timing: ToolDispatchTimingSnapshot,
    ) {
        let ToolCallTimingLineage {
            parent_call_id,
            parent_cell_id,
            runtime_tool_call_id,
        } = lineage;
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if state.completed_snapshot.is_some() {
            state.invalid_transition();
            return;
        }
        state.tool_closure.record_timing(
            TurnTimingToolCallIdentity {
                call_id: call_id.to_string(),
                execution_id: timing.execution_id.clone(),
                source,
                parent_call_id: parent_call_id.map(str::to_string),
                sampling_generation_id: timing.sampling_generation_id.clone(),
            },
            timing.outcome.is_some(),
        );
        self.signal_tool_closure_changed();
        // Only a model-issued call is emitted by its sampling generation; a
        // nested Code Mode call's generation is merely current at dispatch.
        // The per-call detail cap below does not bound dependency evidence.
        if source == TurnTimingToolCallSource::Direct
            && timing.sampling_generation_id.0 != "generation-pending"
            && let Some(evidence) = state.activity_timeline.blocking.as_mut()
        {
            blocking::record_dependency(evidence, timing.execution_id.0.clone(),
                timing.sampling_generation_id.0.clone(), "emitted_by");
        }
        if state.tool_calls.len() >= MAX_TOOL_CALL_TIMINGS {
            state.tool_call_timing_overflow = state.tool_call_timing_overflow.saturating_add(1);
            return;
        }

        let observed_at_ms = state
            .elapsed_since_start(sample.time.monotonic_ns)
            .map(u128_to_u64_ms);
        let legacy_first_poll_at_ms = observed_at_ms
            .zip(timing.total_duration_ms)
            .map(|(observed, total)| observed.saturating_sub(total));
        let request_created_at_ms = lifecycle_boundary_at(
            &timing.lifecycle_events,
            ToolLifecycleBoundary::RequestCreated,
        )
        .or_else(|| {
            legacy_first_poll_at_ms
                .zip(timing.item_to_first_poll_ms)
                .map(|(first_poll, queued)| first_poll.saturating_sub(queued))
        });
        let accepted_at_ms = state
            .tool_closure
            .entries
            .get(&timing.execution_id)
            .and_then(|entry| entry.accepted_at_ns)
            .map(u128_to_u64_ms);
        let parallel_gate_admitted_at_ms =
            lifecycle_boundary_at(&timing.lifecycle_events, ToolLifecycleBoundary::Admitted);
        let handler_entry_at_ms = lifecycle_boundary_at(
            &timing.lifecycle_events,
            ToolLifecycleBoundary::HandlerStart,
        );
        let handler_exit_at_ms = lifecycle_boundary_at(
            &timing.lifecycle_events,
            ToolLifecycleBoundary::HandlerReturn,
        );
        let process_spawned_at_ms = lifecycle_boundary_at(
            &timing.lifecycle_events,
            ToolLifecycleBoundary::ProcessSpawn,
        )
        .or_else(|| {
            request_created_at_ms
                .zip(timing.exec_request_to_spawn_ms)
                .map(|(accepted, request_to_spawn)| accepted.saturating_add(request_to_spawn))
        });
        let process_exited_at_ms =
            lifecycle_boundary_at(&timing.lifecycle_events, ToolLifecycleBoundary::ProcessExit);
        let delivered_at_ms = lifecycle_boundary_at(
            &timing.lifecycle_events,
            ToolLifecycleBoundary::RelayDelivery,
        );
        let first_poll_at_ms = timing
            .first_poll_at_ms
            .or_else(|| {
                request_created_at_ms
                    .zip(timing.item_to_first_poll_ms)
                    .map(|(accepted, queued)| accepted.saturating_add(queued))
            })
            .or(legacy_first_poll_at_ms);
        let output_collected_at_ms = first_poll_at_ms
            .zip(timing.first_poll_to_output_collected_ms)
            .map(|(first_poll, duration)| first_poll.saturating_add(duration));
        let generation_index = timing
            .sampling_generation_id
            .0
            .strip_prefix("generation-")
            .and_then(|index| index.parse::<u32>().ok())
            .filter(|index| {
                state
                    .model_requests
                    .iter()
                    .any(|request| request.generation_index == *index)
            });
        let pending_process_exit = state
            .background_tool_process_exits
            .remove(&(call_id.to_string(), timing.execution_id.clone()));

        let mut tool_call = TurnTimingToolCall {
            call_id: call_id.to_string(),
            parent_call_id: parent_call_id.map(str::to_string),
            parent_cell_id: parent_cell_id.map(str::to_string),
            runtime_tool_call_id: runtime_tool_call_id.map(str::to_string),
            tool_name: tool_name.to_string(),
            source,
            execution_id: timing.execution_id,
            sampling_generation_id: timing.sampling_generation_id,
            outcome: timing.outcome.map(str::to_string),
            generation_index,
            accepted_at_ms,
            first_poll_at_ms,
            parallel_gate_admitted_at_ms,
            handler_entry_at_ms,
            handler_exit_at_ms,
            output_collected_at_ms,
            process_spawned_at_ms,
            process_exited_at_ms,
            delivered_at_ms,
            output_model_visible_at_ms: None,
            model_resumed_at_ms: None,
            ready_to_sample_to_dispatch_ns: None,
            item_to_first_poll_ms: timing.item_to_first_poll_ms,
            parallel_gate_wait_ms: timing.parallel_gate_wait_ms,
            authorization_state_coordination_ms: timing.authorization_state_coordination_ms,
            handler_duration_ms: timing.handler_duration_ms,
            workspace_evidence_before_ms: timing.workspace_evidence_before_ms,
            workspace_evidence_before_cache_hit: timing.workspace_evidence_before_cache_hit,
            workspace_evidence_before_timed_out_git_dependencies: timing
                .workspace_evidence_before_timed_out_git_dependencies,
            workspace_evidence_after_ms: timing.workspace_evidence_after_ms,
            pre_tool_hook_ms: timing.pre_tool_hook_ms,
            post_tool_hook_ms: timing.post_tool_hook_ms,
            output_projection_ms: timing.output_projection_ms,
            history_persistence_ms: timing.history_persistence_ms,
            post_handler_ms: timing.post_handler_ms,
            total_duration_ms: timing.total_duration_ms,
            eager: timing.eager,
            process_alive_at_delivery: timing.exec_process_alive_at_delivery,
            exec_cleanup_state_observed: timing.exec_cleanup_state_observed,
            background_process_expected: timing.exec_background_process_expected,
            running_process_after_cleanup: timing.exec_running_process_after_cleanup,
            lifecycle_events: timing.lifecycle_events,
            timer_waits: timing.timer_waits,
            retry_count: timing.retry_count,
            reentry_count: timing.reentry_count,
            next_sample_block_reason: NextSampleBlockReason::ReadyToSample,
        };
        if let Some(process_exit) = pending_process_exit {
            record_tool_process_exit(&mut tool_call, &process_exit);
        }
        state.tool_calls.push(tool_call);
    }

    pub(crate) fn update_tool_dispatch_lifecycle(
        &self,
        call_id: &str,
        execution_id: &codex_protocol::protocol::ToolExecutionId,
        timing: ToolDispatchTimingSnapshot,
    ) -> bool {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        let Some(tool_call) = state.tool_calls.iter_mut().rev().find(|tool_call| {
            tool_call.call_id == call_id && &tool_call.execution_id == execution_id
        }) else {
            return false;
        };
        tool_call.lifecycle_events = timing.lifecycle_events;
        tool_call.timer_waits = timing.timer_waits;
        tool_call.retry_count = timing.retry_count;
        tool_call.reentry_count = timing.reentry_count;
        tool_call.delivered_at_ms = lifecycle_boundary_at(
            &tool_call.lifecycle_events,
            ToolLifecycleBoundary::RelayDelivery,
        );
        tool_call.output_collected_at_ms = tool_call
            .first_poll_at_ms
            .zip(timing.first_poll_to_output_collected_ms)
            .map(|(first_poll, duration)| first_poll.saturating_add(duration));
        true
    }

    pub(crate) fn record_next_sample_block_reason(&self, reason: NextSampleBlockReason) {
        self.set_live_sampling_phase(Some(reason));
        self.record_tool_sample_block_reason(reason);
    }

    /// Per-tool observers may outlive model work and cannot author the turn's live phase.
    pub(crate) fn record_tool_sample_block_reason(&self, reason: NextSampleBlockReason) {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        state.ready_to_sample_ns = if reason == NextSampleBlockReason::ReadyToSample {
            state
                .ready_to_sample_ns
                .or_else(|| state.elapsed_since_start(sample.time.monotonic_ns))
        } else {
            None
        };
        for tool_call in state.tool_calls.iter_mut().rev() {
            if tool_call.model_resumed_at_ms.is_some() {
                break;
            }
            tool_call.next_sample_block_reason = reason;
        }
    }

    pub(crate) fn record_pending_tool_timer_wait(&self, wait: ToolLifecycleTimerWait) {
        let mut state = self.state();
        for tool_call in state.tool_calls.iter_mut().rev() {
            if tool_call.model_resumed_at_ms.is_some() {
                break;
            }
            crate::tools::tool_dispatch_trace::push_timer_wait(
                &mut tool_call.timer_waits,
                wait.clone(),
            );
            tool_call.reentry_count = tool_call.reentry_count.saturating_add(1);
        }
    }

    /// Records the point at which a direct tool result has been inserted into
    /// model-visible history. This is intentionally separate from durable
    /// rollout persistence.
    pub(crate) fn record_tool_output_model_visible(&self, call_id: &str) {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if state.completed_snapshot.is_some() {
            state.invalid_transition();
            return;
        }
        let model_visible_at_ms = state
            .elapsed_since_start(sample.time.monotonic_ns)
            .map(u128_to_u64_ms);
        // Dispatch timings arrive in completion order, while the relay publishes
        // outputs in request order. Prefer its execution-specific delivery mark
        // when call IDs are reused; legacy callers without relay marks remain FIFO.
        let index = state
            .tool_calls
            .iter()
            .enumerate()
            .filter(|(_, tool_call)| {
                tool_call.call_id == call_id && tool_call.output_model_visible_at_ms.is_none()
            })
            .min_by_key(|(_, tool_call)| tool_call.delivered_at_ms.is_none())
            .map(|(index, _)| index);
        if let Some(index) = index {
            state.tool_calls[index].output_model_visible_at_ms = model_visible_at_ms;
        } else if state.tool_calls.len() >= MAX_TOOL_CALL_TIMINGS {
            // The capped row cannot carry this result's consumption edge.
            state.unrecorded_tool_result_consumptions =
                state.unrecorded_tool_result_consumptions.saturating_add(1);
        }
    }

    /// Resolve wire IDs while their producing generation is still current, before
    /// yielding to persistence. A later generation may reuse the same wire ID.
    pub(crate) fn tool_result_execution_id(&self, call_id: &str) -> Option<ToolExecutionId> {
        let state = self.state();
        let generation = state
            .current_generation_index
            .map(|index| format!("generation-{index}"))
            .unwrap_or_else(|| "generation-pending".to_string());
        state
            .tool_closure
            .entries
            .values()
            .find(|entry| {
                entry.identity.call_id == call_id
                    && entry.identity.source == TurnTimingToolCallSource::Direct
                    && entry.identity.sampling_generation_id.0 == generation
            })
            .map(|entry| entry.identity.execution_id.clone())
    }

    /// Binds only settled calls in the frozen packet, while its producing
    /// direct call's generation is still current.
    pub(crate) fn record_code_mode_projection_delivery(
        &self,
        projection_call_id: &str,
        delivered_call_ids: &[String],
    ) {
        let Some(projection_execution_id) = self.tool_result_execution_id(projection_call_id) else {
            return;
        };
        let mut state = self.state();
        if state.completed_snapshot.is_some() {
            return;
        }
        state
            .tool_closure
            .bind_nested_results_to_projection(&projection_execution_id, delivered_call_ids);
    }

    pub(crate) fn record_tool_result_executions_persisted(&self, executions: &[ToolExecutionId]) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() {
            state.invalid_transition();
            return;
        }
        for execution in executions {
            state
                .queued_durable_tool_result_execution_ids
                .remove(execution);
            state.tool_closure.record_persisted_execution(execution);
        }
        // An empty flush or another call's successful write cannot repair an
        // earlier failed append. Keep waiters released until every accepted
        // result has actually crossed the durability boundary.
        if state
            .tool_closure
            .entries
            .values()
            .all(|entry| entry.persisted)
        {
            state.tool_result_persistence_barrier_failed = false;
        }
        drop(state);
        self.signal_tool_closure_changed();
    }

    pub(crate) fn record_tool_result_executions_queued(&self, executions: &[ToolExecutionId]) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() {
            state.invalid_transition();
            return;
        }
        state
            .queued_durable_tool_result_execution_ids
            .extend(executions.iter().cloned());
    }

    pub(crate) fn queued_tool_result_executions(&self) -> Vec<ToolExecutionId> {
        self.state()
            .queued_durable_tool_result_execution_ids
            .iter()
            .cloned()
            .collect()
    }

    /// Records that the terminal rollout barrier failed. This releases closure waiters without
    /// attesting that any queued result crossed the durability boundary.
    pub(crate) fn record_tool_result_persistence_barrier_failed(&self) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() {
            state.invalid_transition();
            return;
        }
        state.tool_result_persistence_barrier_failed = true;
        drop(state);
        self.signal_tool_closure_changed();
    }

    /// Persists a background process exit into the turn record even when the
    /// outer exec tool already returned a live session to the model.
    pub(crate) fn record_background_tool_process_exit(
        &self,
        call_id: &str,
        timing: ToolDispatchTimingSnapshot,
    ) {
        if timing.exec_spawn_to_exit_ms.is_none()
            && lifecycle_boundary_at(&timing.lifecycle_events, ToolLifecycleBoundary::ProcessExit)
                .is_none()
        {
            return;
        }
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if state.completed_snapshot.is_some() {
            state.invalid_transition();
            return;
        }
        if let Some(tool_call) = state.tool_calls.iter_mut().find(|tool_call| {
            tool_call.call_id == call_id && tool_call.execution_id == timing.execution_id
        }) {
            record_tool_process_exit(tool_call, &timing);
            return;
        }
        if state.background_tool_process_exits.len() < MAX_TOOL_CALL_TIMINGS {
            state
                .background_tool_process_exits
                .insert((call_id.to_string(), timing.execution_id.clone()), timing);
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

    pub(crate) fn record_owner_drained_continuation(&self) {
        let mut state = self.state();
        state.counters.owner_drained_continuation_count = state
            .counters
            .owner_drained_continuation_count
            .saturating_add(1);
    }

    pub(crate) fn record_executed_validation_duration(&self, duration: Duration) {
        let mut state = self.state();
        state.counters.executed_validation_count =
            state.counters.executed_validation_count.saturating_add(1);
        state.counters.executed_validation_duration_ns = state
            .counters
            .executed_validation_duration_ns
            .saturating_add(u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX));
    }

    pub(crate) fn record_ready_startup_prewarm(&self) {
        let mut state = self.state();
        state.counters.ready_startup_prewarm_count =
            state.counters.ready_startup_prewarm_count.saturating_add(1);
    }

    pub(crate) fn record_generation_outcome(
        &self,
        progress_kinds: Vec<TurnTimingProgressKind>,
        structured_action_fingerprint: Option<String>,
        known_empty_actions: bool,
        unchanged_relevant_state: bool,
    ) {
        let mut state = self.state();
        let Some(generation_index) = state.current_generation_index else {
            return;
        };
        let mut terminal_interpretation = false;
        if let Some(index) = state.model_requests.iter().position(|request| {
            request.generation_index == generation_index
                && request.attempt_kind == TurnTimingAttemptKind::Primary
        }) {
            let (previous, current) = state.model_requests.split_at_mut(index);
            let request = &mut current[0];
            if request.is_continuation
                && let Some(previous) = previous.iter_mut().rev().find(|previous| {
                    previous.attempt_kind == TurnTimingAttemptKind::Primary
                        && previous.generation_index.checked_add(1) == Some(generation_index)
                })
            {
                previous.next_structured_action_changed = previous
                    .structured_action_fingerprint
                    .as_ref()
                    .zip(structured_action_fingerprint.as_ref())
                    .is_none_or(|(previous, current)| previous != current);
            }
            request.progress_kinds = progress_kinds
                .into_iter()
                .take(MAX_MODEL_REQUEST_PROGRESS_KINDS)
                .collect();
            // Older providers omit phase. A visible, text-only generation is
            // communication progress, but a preamble before an action is not
            // sufficient to classify that action as final publication.
            request.final_answer_emitted |= request.unphased_message_emitted
                && request.model_emitted_tool_call_count == 0
                && known_empty_actions;
            request.structured_action_fingerprint = structured_action_fingerprint;
            // The next continuation has not been observed yet. Do not count a
            // terminal, interrupted, or unclassified request as a proven repeat.
            request.next_structured_action_changed = true;
            // Publishing a final answer or blocker changes the communication
            // state even when it performs no further tool or repository work.
            request.unchanged_relevant_state =
                unchanged_relevant_state && !request.final_answer_emitted;
            terminal_interpretation = request.final_answer_emitted
                && request.model_emitted_tool_call_count == 0
                && request.generation_purpose
                    == Some(TurnTimingGenerationPurpose::ArtifactContinuation);
        }
        if terminal_interpretation {
            // Classify observed terminal communication, not a predicted purpose.
            // A preamble or a generation that also calls tools is not terminal.
            for request in &mut state.model_requests {
                if request.generation_index == generation_index {
                    request.generation_purpose =
                        Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning);
                }
            }
            state.counters.generations_by_purpose.artifact_continuation = state
                .counters
                .generations_by_purpose
                .artifact_continuation
                .saturating_sub(1);
            state
                .counters
                .generations_by_purpose
                .terminal_completion_reasoning = state
                .counters
                .generations_by_purpose
                .terminal_completion_reasoning
                .saturating_add(1);
            state.current_generation_purpose =
                Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning);
        }
    }

    /// Records that this generation's turn ended by surfacing an existing tool
    /// result as its final answer. That publication is communication progress,
    /// even though the generation itself emitted no final-answer message.
    pub(crate) fn record_surfaced_final_answer(&self) {
        let mut state = self.state();
        let Some(generation_index) = state.current_generation_index else {
            return;
        };
        if let Some(request) = state.model_requests.iter_mut().find(|request| {
            request.generation_index == generation_index
                && request.attempt_kind == TurnTimingAttemptKind::Primary
        }) {
            request.final_answer_emitted = true;
            request.unchanged_relevant_state = false;
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
            request.observe_token_usage(usage);
        }
    }

    /// Provider observation is independent of delivery to the generation's
    /// consumer. Match the dispatch identity, never the current generation.
    pub(crate) fn record_model_attempt_token_usage(
        &self,
        sampling_request_id: &str,
        physical_attempt_id: &str,
        usage: &TokenUsage,
    ) {
        let mut state = self.state();
        if let Some(request) = state.model_requests.iter_mut().find(|request| {
            request.sampling_request_id.as_deref() == Some(sampling_request_id)
                && request
                    .physical_attempt_ids
                    .iter()
                    .any(|id| id == physical_attempt_id)
        }) {
            request.observe_token_usage(usage);
        }
        drop(state);
        // Essential receipts survive bounded diagnostic request/attempt eviction.
        self.model_usage_receipts.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry((sampling_request_id.to_owned(), physical_attempt_id.to_owned()))
            .or_insert_with(|| (usage.clone(), false));
    }

    pub(crate) fn record_model_estimator_model(&self, sampling: &str, attempt: &str, model: &str) {
        let mut state = self.state();
        if let Some(request) = state.model_requests.iter_mut().find(|request|
            request.sampling_request_id.as_deref() == Some(sampling)
                && request.physical_attempt_ids.iter().any(|id| id == attempt)) {
            request.estimator_model = Some(model.to_owned());
        }
    }

    /// Called under the session accounting lock, with no await before commit.
    pub(crate) fn claim_current_model_usage(
        &self, usage: &TokenUsage, delivered: Option<&crate::client_common::ResponseAttemptIdentity>,
    ) -> bool {
        let identity = delivered.map(|identity| (
            identity.sampling_request_id.clone(), identity.physical_attempt_id.clone(),
        )).or_else(|| {
            let state = self.state();
            state.model_requests.last().and_then(|request| Some((
                request.sampling_request_id.clone()?, request.physical_attempt_ids.last()?.clone(),
            )))
        });
        let Some(identity) = identity else { return true; }; // legacy/manual accounting
        let mut receipts = self.model_usage_receipts.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let receipt = receipts.entry(identity).or_insert_with(|| (usage.clone(), false));
        if receipt.1 || receipt.0 != *usage { return false; }
        receipt.1 = true;
        true
    }

    /// Caller holds the session accounting lock through the synchronous commit.
    pub(crate) fn claim_unsettled_model_usage(&self) -> Vec<TokenUsage> {
        self.model_usage_receipts.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .values_mut().filter_map(|(usage, settled)| {
                if *settled { return None; }
                *settled = true;
                Some(usage.clone())
            }).collect()
    }

    pub(crate) fn prompt_token_calibration_with_estimator(
        &self, model: &str, usage: &TokenUsage, local: i64,
    ) -> Option<(u64, u64)> {
        let state = self.state();
        let request = state.model_requests.last()?;
        let input = usage.known_input()?;
        if request.completed_ns.is_none()
            || request.attempt_kind != TurnTimingAttemptKind::Primary
            || request.physical_attempt_ids.len() != 1
            || request.estimator_model.as_deref() != Some(model)
            || request.token_usage.as_ref()?.input_tokens != input as u64
            || input <= 0 || local <= 0
        { return None; }
        Some((local as u64, input as u64))
    }

    pub(crate) fn record_model_request_diagnostics_unavailable(
        &self,
        sampling_request_id: &str,
        physical_attempt_id: &str,
    ) {
        let mut state = self.state();
        if let Some(request) = state.model_requests.iter_mut().find(|request| {
            request.sampling_request_id.as_deref() == Some(sampling_request_id)
                && request.diagnostics_owner_attempt_id.as_deref()
                    == Some(physical_attempt_id)
        }) {
            request.request_diagnostics_status = TurnTimingRequestDiagnosticsStatus::Unavailable;
        }
        if state.completed_snapshot.is_some() {
            tracing::info!(
                sampling_request_id,
                physical_attempt_id,
                request_diagnostics_status = "unavailable",
                "late model request diagnostics"
            );
        }
        state.refresh_completed_request_diagnostics();
    }

    pub(crate) fn record_model_stream_delivery(
        &self,
        sampling_request_id: &str,
        physical_attempt_id: &str,
        summary: &str,
    ) {
        // One constant-size summary per retained attempt. No new retention
        // lifecycle, IO under the state lock, or model/transport work is added.
        if summary.len() > 16 * 1024 {
            return;
        }
        let mut state = self.state();
        if let Some(request) = state.model_requests.iter_mut().find(|request| {
            request.sampling_request_id.as_deref() == Some(sampling_request_id)
                && request.physical_attempt_ids.iter().any(|id| id == physical_attempt_id)
        }) {
            request
                .stream_delivery_by_attempt
                .insert(physical_attempt_id.to_string(), summary.to_string());
        }
        state.refresh_completed_request_diagnostics();
    }

    pub(crate) fn record_response_queue_timing(
        &self,
        sampling_request_id: &str,
        physical_attempt_id: &str,
        stage: &'static str,
        observation: codex_protocol::timing::ResponseQueueTiming,
    ) {
        if !matches!(stage, "api_to_mapper" | "mapper_to_turn") {
            return;
        }
        let mut state = self.state();
        if let Some(request) = state.model_requests.iter_mut().find(|request| {
            request.sampling_request_id.as_deref() == Some(sampling_request_id)
                && request
                    .physical_attempt_ids
                    .iter()
                    .any(|id| id == physical_attempt_id)
        }) {
            request
                .response_queue_timing_by_attempt
                .entry(physical_attempt_id.to_string())
                .or_default()
                .insert(stage.to_string(), observation.clone());
        }
        if state.completed_snapshot.is_some() {
            tracing::info!(event.name = "codex.response_queue_timing",
                sampling_request_id, physical_attempt_id, stage,
                timing_json = %serde_json::to_string(&observation).unwrap_or_default(),
                "late response queue timing");
        }
        state.refresh_completed_request_diagnostics();
    }

    pub(crate) fn record_model_request_token_categories(
        &self,
        sampling_request_id: &str,
        physical_attempt_id: &str,
        categories: TurnTimingRequestTokenCategories,
    ) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() {
            tracing::info!(sampling_request_id, physical_attempt_id,
                request_token_categories = ?categories,
                "late model request token categories");
        }
        if let Some(request) = state.model_requests.iter_mut().find(|request| {
            request.sampling_request_id.as_deref() == Some(sampling_request_id)
                && request.diagnostics_owner_attempt_id.as_deref()
                    == Some(physical_attempt_id)
        }) {
            request.request_token_categories = Some(categories);
            if request.fixed_prefix_reuse_eligible.is_some() {
                request.request_diagnostics_status = TurnTimingRequestDiagnosticsStatus::Complete;
            }
        }
        state.refresh_completed_request_diagnostics();
    }

    /// Records cache-reuse identity on the logical request once. HTTP retries
    /// share that request row, so a later physical attempt must not overwrite
    /// the eligibility observed for the original dispatch. The cache key is
    /// persisted only as a stable SHA-256 fingerprint because callers may
    /// supply an override containing private session metadata.
    pub(crate) fn record_model_request_cache_identity(
        &self,
        sampling_request_id: &str,
        physical_attempt_id: &str,
        fixed_prefix_reuse_eligible: bool,
        prompt_cache_key: Option<&str>,
    ) {
        let mut state = self.state();
        let Some(request) = state.model_requests.iter_mut().find(|request| {
            request.sampling_request_id.as_deref() == Some(sampling_request_id)
                && request.diagnostics_owner_attempt_id.as_deref()
                    == Some(physical_attempt_id)
        }) else {
            return;
        };
        if request.fixed_prefix_reuse_eligible.is_some() {
            return;
        }
        request.fixed_prefix_reuse_eligible = Some(fixed_prefix_reuse_eligible);
        if request.request_token_categories.is_some() {
            request.request_diagnostics_status = TurnTimingRequestDiagnosticsStatus::Complete;
        }
        request.prompt_cache_key_fingerprint =
            prompt_cache_key.map(|key| hex::encode(Sha256::digest(key.as_bytes())));
        if state.completed_snapshot.is_some()
            && let Some(request) = state
                .model_requests
                .iter()
                .find(|request| request.sampling_request_id.as_deref() == Some(sampling_request_id))
        {
            tracing::info!(
                sampling_request_id,
                physical_attempt_id,
                request_diagnostics_status = ?request.request_diagnostics_status,
                fixed_prefix_reuse_eligible,
                prompt_cache_key_fingerprint = ?request.prompt_cache_key_fingerprint,
                "late model request cache identity"
            );
        }
        state.refresh_completed_request_diagnostics();
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

    pub(crate) fn record_tool_router_reuse(&self) {
        let mut state = self.state();
        state.counters.tool_router_reuse_count =
            state.counters.tool_router_reuse_count.saturating_add(1);
    }

    pub(crate) fn record_tool_router_rebuild(&self) {
        let mut state = self.state();
        state.counters.tool_router_rebuild_count =
            state.counters.tool_router_rebuild_count.saturating_add(1);
    }

    pub(crate) fn record_projection_source_dependencies_reuse(&self) {
        let mut state = self.state();
        state.counters.projection_source_dependencies_reuse_count = state
            .counters
            .projection_source_dependencies_reuse_count
            .saturating_add(1);
    }

    pub(crate) fn record_projection_source_dependencies_fallback(&self) {
        let mut state = self.state();
        state.counters.projection_source_dependencies_fallback_count = state
            .counters
            .projection_source_dependencies_fallback_count
            .saturating_add(1);
    }

    /// Keep contended source-dependency bookkeeping off the tool-dispatch worker.
    pub(crate) async fn record_projection_source_dependencies_reuse_async(self: &Arc<Self>) {
        if let Ok(mut state) = self.state.try_lock() {
            state.counters.projection_source_dependencies_reuse_count = state
                .counters
                .projection_source_dependencies_reuse_count
                .saturating_add(1);
            return;
        }
        let timing = Arc::clone(self);
        if let Err(error) = tokio::task::spawn_blocking(move || {
            timing.record_projection_source_dependencies_reuse();
        })
        .await
        {
            tracing::error!(%error, "failed to record projection source dependency reuse");
        }
    }

    pub(crate) async fn record_projection_source_dependencies_fallback_async(self: &Arc<Self>) {
        if let Ok(mut state) = self.state.try_lock() {
            state.counters.projection_source_dependencies_fallback_count = state
                .counters
                .projection_source_dependencies_fallback_count
                .saturating_add(1);
            return;
        }
        let timing = Arc::clone(self);
        if let Err(error) = tokio::task::spawn_blocking(move || {
            timing.record_projection_source_dependencies_fallback();
        })
        .await
        {
            tracing::error!(%error, "failed to record projection source dependency fallback");
        }
    }

    /// Count reduction at the native command -> JavaScript boundary separately
    /// from outer projection. The script can recover it in-cell, so this does
    /// not attribute a provider continuation or count the same tokens twice.
    pub(crate) fn record_nested_tool_output_reduction(&self) {
        let mut state = self.state();
        state.counters.tool_output_truncation_count = state
            .counters
            .tool_output_truncation_count
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
        artifact_reused: bool,
        projection_truncated: bool,
        omitted_sections: u64,
        provider_visible: bool,
    ) {
        Self::record_tool_output_projection_facts_locked(
            &mut self.state(),
            canonical_bytes,
            canonical_tokens,
            model_bytes,
            model_tokens,
            artifact_created,
            artifact_reused,
            projection_truncated,
            omitted_sections,
            provider_visible,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn record_tool_output_projection_facts_locked(
        state: &mut TurnTimingStateInner,
        canonical_bytes: u64,
        canonical_tokens: u64,
        model_bytes: u64,
        model_tokens: u64,
        artifact_created: bool,
        artifact_reused: bool,
        projection_truncated: bool,
        omitted_sections: u64,
        provider_visible: bool,
    ) {
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
        state.counters.tool_output_artifact_reuse_count = state
            .counters
            .tool_output_artifact_reuse_count
            .saturating_add(u32::from(artifact_reused));
        state.counters.tool_output_projection_truncation_count = state
            .counters
            .tool_output_projection_truncation_count
            .saturating_add(u32::from(projection_truncated));
        state.counters.tool_output_omitted_section_count = state
            .counters
            .tool_output_omitted_section_count
            .saturating_add(omitted_sections);
        if provider_visible && (projection_truncated || omitted_sections > 0) {
            state.tool_output_truncation_pending_continuation = true;
        }
    }

    /// Keep contended timing bookkeeping off the async tool-dispatch worker.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_tool_output_projection_facts_async(
        self: &Arc<Self>,
        canonical_bytes: u64,
        canonical_tokens: u64,
        model_bytes: u64,
        model_tokens: u64,
        artifact_created: bool,
        artifact_reused: bool,
        projection_truncated: bool,
        omitted_sections: u64,
        provider_visible: bool,
    ) {
        if let Ok(mut state) = self.state.try_lock() {
            Self::record_tool_output_projection_facts_locked(
                &mut state,
                canonical_bytes,
                canonical_tokens,
                model_bytes,
                model_tokens,
                artifact_created,
                artifact_reused,
                projection_truncated,
                omitted_sections,
                provider_visible,
            );
            return;
        }
        let timing = Arc::clone(self);
        if let Err(error) = tokio::task::spawn_blocking(move || {
            timing.record_tool_output_projection_facts(
                canonical_bytes,
                canonical_tokens,
                model_bytes,
                model_tokens,
                artifact_created,
                artifact_reused,
                projection_truncated,
                omitted_sections,
                provider_visible,
            );
        })
        .await
        {
            // A bookkeeping failure must not skip the accepted tool's terminal
            // projection and lifecycle publication.
            tracing::error!(%error, "failed to record tool output projection timing");
        }
    }

    pub(crate) fn record_tool_output_recovery(&self, in_cell: bool, retruncation_count: u32) {
        let mut state = self.state();
        if in_cell {
            state.counters.tool_output_in_cell_recovery_call_count = state
                .counters
                .tool_output_in_cell_recovery_call_count
                .saturating_add(1);
        } else {
            state.counters.tool_output_recovery_call_count = state
                .counters
                .tool_output_recovery_call_count
                .saturating_add(1);
            state.tool_output_recovery_read_pending_continuation = true;
        }
        state.counters.tool_output_recovery_retruncation_count = state
            .counters
            .tool_output_recovery_retruncation_count
            .saturating_add(retruncation_count);
    }

    /// A cell delivery carrying recovered evidence exceeded its visible output
    /// budget, so that evidence was truncated again. Direct recovery is exempt
    /// from generic re-projection and records its exact zero at the read.
    pub(crate) fn record_tool_output_recovery_retruncation(&self) {
        let mut state = self.state();
        state.counters.tool_output_recovery_retruncation_count = state
            .counters
            .tool_output_recovery_retruncation_count
            .saturating_add(1);
    }

    pub(crate) fn record_tool_output_artifact_reread(&self) {
        let mut state = self.state();
        state.counters.tool_output_artifact_reread_count = state
            .counters
            .tool_output_artifact_reread_count
            .saturating_add(1);
    }

    pub(crate) fn begin_tool_blocking(self: &Arc<Self>) -> TurnTimingGuard {
        self.begin_guard(GuardKind::LegacyToolBlocking)
    }

    pub(crate) fn begin_model_request_wait(self: &Arc<Self>) -> TurnTimingGuard {
        {
            let mut state = self.state();
            state.advance(self.clock.sample().time.monotonic_ns);
            // Ordinary requests start collecting at history preparation. Retry
            // and compaction attempts may enter directly at this boundary.
            state
                .pending_request_setup
                .get_or_insert_with(Default::default);
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
            if state.model_requests.len() >= MAX_MODEL_REQUEST_TIMINGS {
                state.model_requests.remove(0);
                state.model_request_history_truncated = true;
                state.counters.model_request_detail_eviction_count = state
                    .counters
                    .model_request_detail_eviction_count
                    .saturating_add(1);
            }
            state.model_requests.push(ModelRequestTiming {
                generation_index,
                generation_reason,
                generation_purpose,
                disposition,
                relevant_state_fingerprint,
                failure_fingerprint,
                attempt_kind,
                is_continuation,
                next_structured_action_changed: true,
                ..Default::default()
            });
        }
        self.begin_guard(GuardKind::ModelRequestWait)
    }

    pub(crate) fn begin_model_stream_wait(self: &Arc<Self>) -> TurnTimingGuard {
        self.begin_guard(GuardKind::ModelStreamWait)
    }

    pub(crate) fn begin_model_stream_processing(
        self: &Arc<Self>,
        event: &ResponseEvent,
    ) -> Option<TurnTimingGuard> {
        // Once Completed arrives, token accounting, history updates, and output
        // flushing are client bookkeeping, not work overlapping the provider.
        // Keep concurrent tools in their own bucket rather than finalizing.
        (!matches!(event, ResponseEvent::Completed { .. }))
            .then(|| self.begin_guard(GuardKind::ModelStreamProcessing))
    }

    /// Process observation is independent of runtime guards and never extends
    /// the canonical tool bucket through a yield or a later turn.
    pub(crate) fn record_process_spawned(&self) {
        let mut state = self.state();
        if state.started_sample.is_none() || state.completed_snapshot.is_some() {
            return;
        }
        state.advance(self.clock.sample().time.monotonic_ns);
        state.activity.observed_processes = state.activity.observed_processes.saturating_add(1);
    }

    pub(crate) fn record_process_exited(&self) {
        let mut state = self.state();
        if state.started_sample.is_none() || state.completed_snapshot.is_some() {
            return;
        }
        state.advance(self.clock.sample().time.monotonic_ns);
        state.activity.observed_processes = state.activity.observed_processes.saturating_sub(1);
    }

    pub(crate) fn begin_tool_execution(self: &Arc<Self>) -> TurnTimingGuard {
        let origin = crate::tools::tool_dispatch_trace::active_tool_dispatch_timing()
            .map(|timing| timing.sampling_generation_index())
            .unwrap_or_else(|| self.state().current_generation_index);
        self.begin_guard(GuardKind::ToolExecution(origin))
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

    /// Records a policy choice, not an attempt. Only the deduplicated transport
    /// entry below can turn this plan into an executed retry.
    pub(crate) fn adopt_startup_retry_decision(&self, mut decision: retries::RetryDecisionLedger) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() || state.retry_decisions.has_pending() {
            decision.finish(self.clock.sample().time.monotonic_ns, None);
            return;
        }
        state.retry_decisions = decision;
        if let Some(request) = state.model_requests.last_mut()
            && request.dispatched_attempt_count == 0
        {
            request.attempt_kind = TurnTimingAttemptKind::Fallback;
        }
    }

    /// A policy choice is resolved only by a physical transport-entry callback.
    pub(crate) fn record_retry_decision(
        &self,
        trigger: retries::RetryTrigger,
        http_status: Option<u16>,
        action: retries::RetryAction,
        backoff: Option<Duration>,
    ) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() {
            return;
        }
        let source = state.model_requests.last().and_then(|request| {
            if matches!(trigger, retries::RetryTrigger::MissingCredentials) {
                return None;
            }
            let attempt = request.physical_attempt_ids.iter().rev()
                .find(|id| request.dispatched_attempt_ids.contains(*id))?;
            Some((request.sampling_request_id.clone()?, attempt.clone()))
        });
        let now_ns = self.clock.sample().time.monotonic_ns;
        let generation_index = state.current_generation_index;
        state.retry_decisions.plan(now_ns, retries::RetryPlan {
            generation_index,
            clock_id: codex_protocol::timing::process_clock_domain_id(),
            trigger,
            http_status,
            action,
            source: source.as_ref().map(|(request, attempt)| (request.as_str(), attempt.as_str())),
            backoff_ns: backoff.map(|duration| duration.as_nanos()),
        });
        if state.activity.retry > 0 {
            state.retry_decisions.begin_wait(now_ns);
        }
    }

    pub(crate) fn begin_standalone_work(self: &Arc<Self>) -> TurnTimingGuard {
        self.begin_guard(GuardKind::StandaloneWork)
    }

    pub(crate) fn begin_local_phase(self: &Arc<Self>, phase: TurnLocalPhase) -> TurnTimingGuard {
        self.begin_guard(GuardKind::Local(phase))
    }

    pub(crate) fn begin_request_preparation(
        self: &Arc<Self>,
        preparation: &mut Option<TurnTimingGuard>,
    ) {
        debug_assert!(
            preparation.is_none(),
            "request preparation must be consumed before the next generation"
        );
        if preparation.is_none() {
            {
                let mut state = self.state();
                state.advance(self.clock.sample().time.monotonic_ns);
                state.pending_request_setup = Some(ClientCriticalPhaseTiming::default());
                if state.dispatch_ready_snapshot.is_none()
                    && let Some(now) = state.last_monotonic_ns
                    && let Some(elapsed) = state.elapsed_since_start(now)
                {
                    state
                        .milestones
                        .first_request_preparation_started_ns
                        .get_or_insert(elapsed);
                }
            }
            *preparation = Some(self.begin_local_phase(TurnLocalPhase::Preparation));
        }
    }

    pub(crate) fn finish_request_preparation(&self, preparation: &mut Option<TurnTimingGuard>) {
        drop(preparation.take());
        // Dispatch consumes this snapshot; a pre-dispatch failure must discard
        // it rather than leak its setup time into a later generation.
        self.state().pending_request_setup = None;
    }

    pub(crate) fn begin_startup_prewarm_wait_outside_preparation<'a>(
        self: &Arc<Self>,
        preparation: &'a mut Option<TurnTimingGuard>,
    ) -> TurnPreparationPhaseGuard<'a> {
        self.begin_phase_outside_preparation(preparation, TurnLocalPhase::StartupPrewarmWait)
    }

    fn begin_phase_outside_preparation<'a>(
        self: &Arc<Self>,
        preparation: &'a mut Option<TurnTimingGuard>,
        phase: TurnLocalPhase,
    ) -> TurnPreparationPhaseGuard<'a> {
        debug_assert_ne!(phase, TurnLocalPhase::Preparation);
        let mut prior_preparation = preparation.take();
        let resume_preparation = prior_preparation.as_ref().is_some_and(|guard| guard.active);
        let phase_guard = if resume_preparation {
            let Some(prior_preparation) = prior_preparation.as_mut() else {
                unreachable!("active preparation guard must be present");
            };
            debug_assert!(Arc::ptr_eq(&prior_preparation.timing, self));
            debug_assert_eq!(
                prior_preparation.kind,
                GuardKind::Local(TurnLocalPhase::Preparation)
            );
            prior_preparation.active = false;
            TurnTimingGuard {
                timing: Arc::clone(self),
                kind: GuardKind::Local(phase),
                active: self.transition_local_phase(TurnLocalPhase::Preparation, phase),
            }
        } else {
            self.begin_local_phase(phase)
        };
        drop(prior_preparation);
        TurnPreparationPhaseGuard {
            timing: Arc::clone(self),
            preparation,
            phase,
            phase_guard: Some(phase_guard),
            resume_preparation,
        }
    }

    fn transition_local_phase(&self, from: TurnLocalPhase, to: TurnLocalPhase) -> bool {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.transition_local_phase(sample.time.monotonic_ns, from, to)
    }

    pub(crate) fn begin_finalization(&self) {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if state.completed_snapshot.is_some() || state.activity.finalizing {
            state.invalid_transition();
            return;
        }
        state.activity.finalizing = true;
        state.validate_activity();
    }

    pub(crate) fn record_direct_final_content(&self) {
        let mut state = self.state();
        state.final_content.direct_ms = state
            .started_sample
            .and_then(|start| {
                self.clock
                    .sample()
                    .time
                    .monotonic_ns
                    .checked_sub(start.time.monotonic_ns)
            })
            .and_then(|ns| u64::try_from(ns / NANOS_PER_MILLISECOND).ok());
    }

    pub(crate) fn final_tail(
        &self,
        message: Option<&codex_protocol::protocol::AcceptedFinalMessage>,
        surfaced: Option<&codex_protocol::protocol::SurfacedToolResult>,
    ) -> Option<codex_protocol::protocol::TurnFinalTail> {
        if message.is_none() && surfaced.is_none() {
            return None;
        }
        let state = self.state();
        let reconciled_host_ms = state
            .started_sample
            .and_then(|start| {
                self.clock
                    .sample()
                    .time
                    .monotonic_ns
                    .checked_sub(start.time.monotonic_ns)
            })
            .and_then(|ns| u64::try_from(ns / NANOS_PER_MILLISECOND).ok());
        let text = message
            .map(|message| message.text.as_str())
            .or_else(|| surfaced.and_then(|result| result.canonical_message.as_deref()));
        let digest = text.map(|text| hex::encode(Sha256::digest(text.as_bytes())));
        let observed = message.and_then(|message| {
            state
                .final_content
                .items
                .iter()
                .find(|entry| entry.0 == message.item_id && entry.3 == digest)
        });
        let direct_ms = surfaced
            .and(state.final_content.direct_ms)
            .filter(|_| text.is_some());
        Some(codex_protocol::protocol::TurnFinalTail {
            measurement_version: 1,
            item_id: message.map(|message| message.item_id.clone()),
            canonical_sha256: digest,
            text_codepoints: text.map(|text| text.chars().count() as u64),
            text_utf8_bytes: text.map(|text| text.len() as u64),
            whitespace_words: text.map(|text| text.split_whitespace().count() as u64),
            first_content_host_ms: observed.and_then(|entry| entry.1).or(direct_ms),
            full_content_host_ms: observed.and_then(|entry| entry.2).or(direct_ms),
            reconciled_host_ms,
            // Neither requiredness nor a native render is observable here.
            required_evidence: None,
            native_render_ack: None,
        })
    }

    pub(crate) fn record_response_event_milestones(
        &self,
        event: &ResponseEvent,
    ) -> Option<Duration> {
        let records_model_output = response_event_records_model_output(event);
        let records_model_content = response_event_records_model_content(event);
        let records_actionable_output = response_event_records_actionable_output(event);
        let records_visible_output = response_event_records_visible_output(event);
        let records_completion = matches!(event, ResponseEvent::Completed { .. });
        let records_final_candidate = matches!(
            event,
            ResponseEvent::OutputItemAdded(_)
                | ResponseEvent::OutputItemDone(_)
                | ResponseEvent::OutputTextDelta(_)
        );
        if !records_model_output
            && !records_actionable_output
            && !records_visible_output
            && !records_completion
            && !records_final_candidate
        {
            return None;
        }
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        let elapsed_ns = state.elapsed_since_start(sample.time.monotonic_ns)?;
        let compaction = state.current_generation_purpose
            == Some(TurnTimingGenerationPurpose::CompactionRecovery);
        if !compaction {
            state.final_content.observe(
                event,
                u64::try_from(elapsed_ns / NANOS_PER_MILLISECOND).unwrap_or(u64::MAX),
            );
        }
        let records_visible_output = records_visible_output && !compaction;
        let first_content = records_model_content && !compaction
            && state.milestones.first_model_content_ns.is_none();
        if first_content {
            state
                .milestones
                .first_model_content_ns
                .get_or_insert(elapsed_ns);
        }
        if let ResponseEvent::OutputItemDone(ResponseItem::Message { role, phase, .. }) = event
            && role == "assistant"
            && records_visible_output
            && let Some(request) = state.model_requests.last_mut()
        {
            match phase {
                Some(codex_protocol::models::MessagePhase::FinalAnswer) => {
                    request.final_answer_emitted = true;
                    request.unchanged_relevant_state = false;
                }
                None => request.unphased_message_emitted = true,
                _ => {}
            }
        }
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
        if records_actionable_output
            && !compaction
            && state.milestones.first_actionable_output_ns.is_none()
        {
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
        first_content.then(|| duration_from_nanos(elapsed_ns))
    }

    /// First nonempty assistant text accepted by the host after filtering.
    /// Reasoning and completed-message time remain separate; this is not paint.
    pub(crate) fn record_assistant_text_publication(
        &self,
        event: &codex_protocol::protocol::EventMsg,
    ) {
        use codex_protocol::items::AgentMessageContent;
        use codex_protocol::protocol::EventMsg;
        let (item_id, phase) = match event {
            EventMsg::AgentMessageContentDelta(delta) if !delta.delta.is_empty() => {
                (Some(delta.item_id.as_str()), None)
            }
            EventMsg::ItemStarted(codex_protocol::protocol::ItemStartedEvent {
                item: TurnItem::AgentMessage(message), ..
            }) | EventMsg::ItemCompleted(codex_protocol::protocol::ItemCompletedEvent {
                item: TurnItem::AgentMessage(message), ..
            }) => {
                if !message.content.iter().any(|part| matches!(
                    part, AgentMessageContent::Text { text } if !text.is_empty()
                )) {
                    return;
                }
                (Some(message.id.as_str()), message.phase.clone())
            }
            EventMsg::AgentMessage(message) if !message.message.is_empty() => (None, message.phase.clone()),
            _ => return,
        };
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if state.current_generation_purpose == Some(TurnTimingGenerationPurpose::CompactionRecovery)
            || state.milestones.first_assistant_text_ns.is_some()
        {
            return;
        }
        let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns) else {
            return;
        };
        state.milestones.first_assistant_text_ns = Some(elapsed_ns);
        state.milestones.first_assistant_text_item_id = item_id.map(str::to_owned);
        state.milestones.first_assistant_text_phase = phase;
    }

    /// Host queue acceptance after consumer routing, not native rendering.
    pub(crate) fn record_visible_output_publication(&self) {
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if state.current_generation_purpose != Some(TurnTimingGenerationPurpose::CompactionRecovery)
            && let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns)
        {
            state.milestones.first_visible_output_ns.get_or_insert(elapsed_ns);
        }
    }

    pub(crate) fn record_provider_terminal(&self, sampling_request_id: &str, physical_attempt_id: &str) {
        let mut state = self.state();
        if !state.model_requests.last().is_some_and(|request| {
            request.sampling_request_id.as_deref() == Some(sampling_request_id)
                && (request.physical_attempt_ids.last().map(String::as_str) == Some(physical_attempt_id)
                    || request.diagnostics_owner_attempt_id.as_deref() == Some(physical_attempt_id))
        }) {
            return;
        }
        state.advance(self.clock.sample().time.monotonic_ns);
        state.activity.provider_terminal_observed = true;
        state.activity.provider_request_dispatched = false;
    }

    pub(crate) fn mark_model_responses_accounted(&self, count: u64) {
        let mut state = self.state();
        state.accounted_model_responses = state.accounted_model_responses.saturating_add(count);
    }

    pub(crate) fn unaccounted_model_request_count(&self) -> u64 {
        let state = self.state();
        u64::from(state.counters.model_request_count)
            .saturating_sub(state.accounted_model_responses)
    }

    /// Last physical entry reported by the transport dispatch owner, not stream creation.
    pub(crate) fn latest_model_dispatch_identity(
        &self,
    ) -> Option<crate::client_common::ResponseAttemptIdentity> {
        let state = self.state();
        let request = state.model_requests.last()?;
        let physical_attempt_id = request.physical_attempt_ids.iter().rev()
            .find(|id| request.dispatched_attempt_ids.contains(*id))?;
        Some(crate::client_common::ResponseAttemptIdentity {
            sampling_request_id: request.sampling_request_id.clone()?,
            physical_attempt_id: physical_attempt_id.clone(),
        })
    }

    /// Called at transport entry, after preparation/authentication, not at readiness.
    /// Registered identities fence stale callbacks and deduplicate notifications.
    pub(crate) fn mark_model_attempt_dispatched(
        &self,
        sampling_request_id: &str,
        physical_attempt_id: &str,
        internal_retry: bool,
        route: retries::RetryRoute,
    ) {
        self.mark_model_request_dispatched_inner(Some((
            sampling_request_id,
            physical_attempt_id,
            internal_retry,
            route,
        )));
    }

    fn mark_model_request_dispatched_inner(
        &self,
        identity: Option<(&str, &str, bool, retries::RetryRoute)>,
    ) {
        self.set_live_sampling_phase(None);
        let mut state = self.state();
        let Some(request) = state.model_requests.last_mut() else {
            state.invalid_transition();
            return;
        };
        let attempt_kind =
            if let Some((sampling_request_id, physical_attempt_id, internal_retry, _)) = identity {
                if request.sampling_request_id.as_deref() != Some(sampling_request_id)
                    || !request
                        .physical_attempt_ids
                        .iter()
                        .any(|id| id == physical_attempt_id)
                {
                    state.invalid_transition();
                    return;
                }
                if !request
                    .dispatched_attempt_ids
                    .insert(physical_attempt_id.to_owned())
                {
                    return;
                }
                if internal_retry {
                    TurnTimingAttemptKind::Retry
                } else {
                    request.attempt_kind
                }
            } else {
                if request.dispatch_ns.is_some() {
                    return;
                }
                request.attempt_kind
            };
        request.dispatched_attempt_count = request.dispatched_attempt_count.saturating_add(1);
        let generation_index = request.generation_index;
        let resumed_generation_id = format!("generation-{}", request.generation_index);
        let sample = self.clock.sample();
        if let Some((sampling_request_id, physical_attempt_id, _, route)) = identity {
            state.retry_decisions.finish(sample.time.monotonic_ns, Some(retries::RetryExecution {
                generation_index: Some(generation_index),
                sampling_request_id,
                physical_attempt_id,
                route,
            }));
        }
        state.advance(sample.time.monotonic_ns);
        state.activity.provider_request_dispatched = true;
        state.activity.provider_terminal_observed = false;
        let elapsed_ns = state.elapsed_since_start(sample.time.monotonic_ns);
        let lifecycle_context = self.lifecycle_context();
        let mut consumed_tools = Vec::new();
        if let Some(model_resumed_at_ms) = state
            .elapsed_since_start(sample.time.monotonic_ns)
            .map(u128_to_u64_ms)
        {
            for tool_call in state.tool_calls.iter_mut().rev() {
                if tool_call.model_resumed_at_ms.is_some() {
                    break;
                }
                if tool_call.output_model_visible_at_ms.is_some()
                    || tool_call.delivered_at_ms.is_some()
                {
                    // Relay delivery alone does not prove that a nested output
                    // became an input to this model request.
                    if tool_call.output_model_visible_at_ms.is_some() {
                        consumed_tools.push(tool_call.execution_id.0.clone());
                    }
                    tool_call.model_resumed_at_ms = Some(model_resumed_at_ms);
                    if !tool_call
                        .lifecycle_events
                        .iter()
                        .any(|event| event.boundary == ToolLifecycleBoundary::NextModelSampleStart)
                    {
                        tool_call.lifecycle_events.push(
                            codex_protocol::protocol::TurnTimingToolLifecycleEvent {
                                boundary: ToolLifecycleBoundary::NextModelSampleStart,
                                at_ms: model_resumed_at_ms,
                                context: lifecycle_context,
                                retry_count: tool_call.retry_count,
                                reentry_count: tool_call.reentry_count,
                            },
                        );
                    }
                    tool_call.next_sample_block_reason = NextSampleBlockReason::ReadyToSample;
                }
            }
        }
        let unrecorded_consumptions = std::mem::take(&mut state.unrecorded_tool_result_consumptions);
        if let Some(evidence) = state.activity_timeline.blocking.as_mut() {
            for execution_id in consumed_tools {
                blocking::record_dependency(evidence, resumed_generation_id.clone(), execution_id, "consumes_tool_result");
            }
            evidence.dropped_dependencies =
                evidence.dropped_dependencies.saturating_add(unrecorded_consumptions);
        }
        if state.dispatch_ready_snapshot.is_none()
            && let Some(elapsed_ns) = elapsed_ns
        {
            state.dispatch_ready_snapshot = Some((
                elapsed_ns,
                state.attributed_client_union_ns,
                state.client_critical_phases.clone(),
            ));
        }
        if let (Some(elapsed_ns), Some(ready_to_sample_ns)) =
            (elapsed_ns, state.ready_to_sample_ns.take())
        {
            let ready_to_dispatch_ns =
                u64::try_from(elapsed_ns.saturating_sub(ready_to_sample_ns)).unwrap_or(u64::MAX);
            for tool_call in state.tool_calls.iter_mut().rev() {
                if tool_call.ready_to_sample_to_dispatch_ns.is_some() {
                    break;
                }
                if tool_call.model_resumed_at_ms.is_some()
                    && (tool_call.output_model_visible_at_ms.is_some()
                        || tool_call.delivered_at_ms.is_some())
                {
                    tool_call.ready_to_sample_to_dispatch_ns = Some(ready_to_dispatch_ns);
                }
            }
        }
        let setup_phases = state.pending_request_setup.take();
        if let Some(elapsed_ns) = elapsed_ns
            && let Some(request) = state.model_requests.last_mut()
            && request.dispatch_ns.is_none()
        {
            request.setup_phases = setup_phases;
            request.dispatch_ns = Some(elapsed_ns);
        }
        // First-dispatch timing belongs to the sampling row; additive attempt
        // accounting belongs to each unique physical transport identity.
        match attempt_kind {
            TurnTimingAttemptKind::Primary => {
                state.counters.attempts_by_kind.primary =
                    state.counters.attempts_by_kind.primary.saturating_add(1);
            }
            TurnTimingAttemptKind::Retry => {
                state.counters.model_retry_count = state.counters.model_retry_count.saturating_add(1);
                state.counters.attempts_by_kind.retry =
                    state.counters.attempts_by_kind.retry.saturating_add(1);
            }
            TurnTimingAttemptKind::Fallback => {
                state.counters.model_retry_count = state.counters.model_retry_count.saturating_add(1);
                state.counters.model_fallback_count = state.counters.model_fallback_count.saturating_add(1);
                state.counters.attempts_by_kind.fallback =
                    state.counters.attempts_by_kind.fallback.saturating_add(1);
            }
        }
        state.counters.model_request_count = state.counters.model_request_count.saturating_add(1);
    }

    pub(crate) fn record_ttfm_for_turn_item(&self, item: &TurnItem) -> Option<Duration> {
        if !matches!(item, TurnItem::AgentMessage(_)) {
            return None;
        }
        let mut state = self.state();
        let sample = self.clock.sample();
        state.advance(sample.time.monotonic_ns);
        if state.milestones.first_agent_message_ns.is_some() {
            return None;
        }
        let elapsed_ns = state.elapsed_since_start(sample.time.monotonic_ns)?;
        state.milestones.first_agent_message_ns = Some(elapsed_ns);
        Some(duration_from_nanos(elapsed_ns))
    }

    pub(crate) fn mark_model_request_attempt_ended(&self) {
        let mut state = self.state();
        state.advance(self.clock.sample().time.monotonic_ns);
        state.activity.provider_request_dispatched = false;
    }

    fn begin_guard(self: &Arc<Self>, kind: GuardKind) -> TurnTimingGuard {
        let mut state = self.state();
        let sample = self.clock.sample();
        let active = state.begin_guard(sample.time.monotonic_ns, kind);
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
        let mut state = self.timing.state();
        let sample = self.timing.clock.sample();
        state.end_guard(sample.time.monotonic_ns, self.kind);
    }
}

impl TurnTimingStateInner {
    fn record_model_retry(&mut self) {
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
        self.next_attempt_kind = if self.retry_decisions.is_fallback_for(Some(generation_index)) {
            TurnTimingAttemptKind::Fallback
        } else {
            TurnTimingAttemptKind::Primary
        };
    }

    fn start(&mut self, sample: ClockSample) -> i64 {
        if self.started_sample.is_some() || self.completed_snapshot.is_some() {
            self.invalid_transition();
            return self
                .started_sample
                .map(|started| started.time.wall_unix_ms)
                .or_else(|| {
                    self.completed_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.started_at_unix_ms)
                })
                .unwrap_or(sample.time.wall_unix_ms);
        }
        *self = Self {
            activity_timeline: codex_protocol::timing::TurnTimingTimeline {
                version: 1,
                blocking: Some(codex_protocol::timing::TurnTimingBlockingEvidence {
                    version: 1,
                    ..Default::default()
                }),
                started_at_ns: u64::try_from(sample.time.monotonic_ns).unwrap_or(u64::MAX),
                ..Default::default()
            },
            started_sample: Some(sample),
            last_monotonic_ns: Some(sample.time.monotonic_ns),
            legacy: LegacyProfileState::new(sample.time.monotonic_ns),
            ..Self::default()
        };
        sample.time.wall_unix_ms
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
                    if self.legacy.active_phase == Some(LegacyPhase::Sampling) {
                        self.invalid_transition();
                    }
                    return false;
                }
            }
            GuardKind::LegacyToolBlocking => {
                if !self.legacy.begin(now_ns, LegacyPhase::ToolBlocking) {
                    if self.legacy.active_phase == Some(LegacyPhase::ToolBlocking) {
                        self.invalid_transition();
                    }
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
            GuardKind::ToolExecution(origin) => {
                self.activity.tool = self.activity.tool.saturating_add(1);
                if let Some(origin) = origin {
                    if self.active_tool_origins.contains_key(&origin)
                        || self.active_tool_origins.len() < MAX_MODEL_REQUEST_TIMINGS
                    {
                        let count = self.active_tool_origins.entry(origin).or_default();
                        count.0 = count.0.saturating_add(1);
                    } else {
                        self.model_request_history_truncated = true;
                    }
                }
            }
            GuardKind::InteractiveWait(kind) => {
                self.activity.interactive = self.activity.interactive.saturating_add(1);
                self.increment_wait_count(kind);
            }
            GuardKind::RetryBackoff => {
                if self.activity.retry == 0 {
                    self.retry_decisions.begin_wait(now_ns);
                }
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
                self.pending_request_setup = None;
                self.activity.provider_request_dispatched = false;
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
            GuardKind::ToolExecution(origin) => {
                if let Some(origin) = origin
                    && let std::collections::btree_map::Entry::Occupied(mut count) =
                        self.active_tool_origins.entry(origin)
                {
                    let remaining = count.get().0.saturating_sub(1);
                    count.get_mut().0 = remaining;
                    if remaining == 0 {
                        let (_, elapsed_ns) = count.remove();
                        if let Some(request) = self.model_requests.iter_mut().find(|request| {
                            request.generation_index == origin
                                && request.attempt_kind == TurnTimingAttemptKind::Primary
                        }) {
                            add_saturating(
                                &mut request.tool_active_union_ns,
                                elapsed_ns,
                                &mut self.counters.saturation_count,
                            );
                        }
                    }
                }
                decrement(&mut self.activity.tool)
            }
            GuardKind::InteractiveWait(_) => decrement(&mut self.activity.interactive),
            GuardKind::RetryBackoff => {
                let valid = decrement(&mut self.activity.retry);
                if self.activity.retry == 0 {
                    self.retry_decisions.end_wait(now_ns);
                }
                valid
            }
            GuardKind::StandaloneWork => decrement(&mut self.activity.standalone),
            GuardKind::Local(phase) => self.decrement_local_activity(phase),
        };
        if !valid {
            self.invalid_transition();
        }
        self.validate_activity();
    }

    fn transition_local_phase(
        &mut self,
        now_ns: u128,
        from: TurnLocalPhase,
        to: TurnLocalPhase,
    ) -> bool {
        if self.completed_snapshot.is_some() {
            return false;
        }
        self.advance(now_ns);
        if self.started_sample.is_none() {
            self.invalid_transition();
            return false;
        }
        if !self.decrement_local_activity(from) {
            self.invalid_transition();
            return false;
        }
        self.increment_local_activity(to);
        self.validate_activity();
        true
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
        if elapsed_ns > 0
            && let Some(started) = self.started_sample
        {
            let start_ns = u64::try_from(previous_ns.saturating_sub(started.time.monotonic_ns))
                .unwrap_or(u64::MAX);
            let end_ns =
                u64::try_from(now_ns.saturating_sub(started.time.monotonic_ns)).unwrap_or(u64::MAX);
            let mut interval = codex_protocol::timing::TurnTimingActivityInterval {
                start_ns,
                end_ns,
                phase: self.activity.exclusive_phase().wire_name().to_string(),
                model_active: self.activity.provider_model_active(),
                tool_active: self.activity.tool > 0,
                process_in_flight: self.activity.observed_processes > 0,
                interactive_wait: self.activity.interactive > 0,
                compaction: self.activity.compaction > 0,
                planning: self.activity.planning > 0,
            };
            let intervals = &mut self.activity_timeline.intervals;
            let merged = intervals.last_mut().is_some_and(|last| {
                let saved_start = interval.start_ns;
                interval.start_ns = last.start_ns;
                let saved_end = interval.end_ns;
                interval.end_ns = last.end_ns;
                let same = last.end_ns == saved_start && *last == interval;
                interval.start_ns = saved_start;
                interval.end_ns = saved_end;
                if same {
                    last.end_ns = end_ns;
                }
                same
            });
            if !merged {
                if intervals.len() < 8192 {
                    intervals.push(interval);
                } else {
                    self.activity_timeline.dropped_intervals =
                        self.activity_timeline.dropped_intervals.saturating_add(1);
                    self.activity_timeline.record_coarse_interval(&interval);
                }
            }
        }
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
        if self.activity.provider_model_active() {
            add_saturating(
                &mut self.unions.model_active_ns,
                elapsed_ns,
                &mut self.counters.saturation_count,
            );
        }
        if self.activity.provider_request_wait() {
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
            for (_, origin_elapsed_ns) in self.active_tool_origins.values_mut() {
                add_saturating(
                    origin_elapsed_ns,
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
        let active = self.activity;
        let saturation_count = &mut self.counters.saturation_count;
        for phases in [
            self.dispatch_ready_snapshot
                .is_none()
                .then_some(&mut self.client_critical_phases),
            self.pending_request_setup.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            for (is_active, target) in [
                (active.preparation > 0, &mut phases.preparation_ns),
                (active.router_build > 0, &mut phases.router_build_ns),
                (
                    active.executor_readiness_wait > 0,
                    &mut phases.executor_readiness_wait_ns,
                ),
                (
                    active.startup_prewarm_wait > 0,
                    &mut phases.startup_prewarm_wait_ns,
                ),
                (active.persistence > 0, &mut phases.persistence_ns),
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
        let mut model_requests = self.model_requests.clone();
        for (origin, (_, elapsed_ns)) in &self.active_tool_origins {
            if let Some(request) = model_requests.iter_mut().find(|request| {
                request.generation_index == *origin
                    && request.attempt_kind == TurnTimingAttemptKind::Primary
            }) {
                add_saturating(
                    &mut request.tool_active_union_ns,
                    *elapsed_ns,
                    &mut self.counters.saturation_count,
                );
            }
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
            model_requests,
            model_request_history_truncated: self.model_request_history_truncated,
            tool_calls: self.tool_calls.clone(),
            tool_call_timing_overflow: self.tool_call_timing_overflow,
            tool_closure: self.tool_closure.snapshot(),
            deterministic_continuation_receipts: self
                .deterministic_continuation_receipts
                .values()
                .cloned()
                .collect(),
            deterministic_continuation_receipt_overflow: self
                .deterministic_continuation_receipt_overflow,
            pre_first_model_output: self.pre_first_model_output.clone(),
        };
        let legacy_profile = self
            .legacy
            .complete(self.last_monotonic_ns.unwrap_or(sample.time.monotonic_ns));
        let snapshot = TurnTimingSnapshot {
            activity_timeline: codex_protocol::timing::TurnTimingTimeline {
                duration_ns: u64::try_from(inclusive_duration_ns).unwrap_or(u64::MAX),
                coverage_complete: profile_valid && self.activity_timeline.dropped_intervals == 0,
                coverage_including_coarse_complete: Some(profile_valid),
                open_process_count: self.activity.observed_processes,
                ..self.activity_timeline.clone()
            },
            credit_delta: self
                .starting_credit_balance
                .zip(self.last_credit_balance)
                .map(|(start, end)| (start - end).to_string()),
            started_at_unix_ms: started_sample.map(|started| started.time.wall_unix_ms),
            completed_at_unix_ms: started_sample.map(|_| sample.time.wall_unix_ms),
            completed_at_unix_secs: started_sample.map(|_| sample.time.wall_unix_ms / 1_000),
            duration_ms: started_sample.map(|_| u128_to_i64_ms(inclusive_duration_ns)),
            time_to_first_token_ms: self.milestones.first_model_content_ns.map(u128_to_i64_ms),
            legacy_profile,
            profile,
        };
        self.completed_snapshot = Some(snapshot.clone());
        snapshot
    }

    fn refresh_completed_request_diagnostics(&mut self) {
        if let Some(snapshot) = self.completed_snapshot.as_mut() {
            for request in &mut snapshot.profile.model_requests {
                if let Some(current) = self.model_requests.iter().find(|current| {
                    current.sampling_request_id == request.sampling_request_id
                        && current.physical_attempt_ids == request.physical_attempt_ids
                }) {
                    request.request_section_sha256_by_attempt =
                        current.request_section_sha256_by_attempt.clone();
                    request.request_token_categories = current.request_token_categories.clone();
                    request.request_diagnostics_status = current.request_diagnostics_status;
                    request.response_queue_timing_by_attempt =
                        current.response_queue_timing_by_attempt.clone();
                    request.stream_delivery_by_attempt =
                        current.stream_delivery_by_attempt.clone();
                    request.fixed_prefix_reuse_eligible = current.fixed_prefix_reuse_eligible;
                    request.prompt_cache_key_fingerprint =
                        current.prompt_cache_key_fingerprint.clone();
                }
            }
        }
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

#[derive(Clone, Debug, Default)]
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
            // Invalid-image requests fail closed instead of sanitizing and
            // continuing; the field stays in the profile schema as zero.
            invalid_image_recovery: 0,
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

/// Nonempty decoded content observed by the turn consumer, not a provider token
/// timestamp. Tool headers and opaque compaction checkpoints are not content.
pub(crate) fn response_event_records_model_content(event: &ResponseEvent) -> bool {
    match event {
        ResponseEvent::OutputTextDelta(text)
        | ResponseEvent::ReasoningSummaryDelta { delta: text, .. }
        | ResponseEvent::ReasoningContentDelta { delta: text, .. }
        | ResponseEvent::ReasoningSummaryDone { text, .. }
        | ResponseEvent::ToolCallInputDelta { delta: text, .. } => !text.is_empty(),
        ResponseEvent::OutputItemAdded(item) => response_item_records_visible_output(item),
        ResponseEvent::OutputItemDone(item) => {
            response_item_records_visible_output(item)
                || match item {
                    ResponseItem::FunctionCall { arguments, .. } => !arguments.is_empty(),
                    ResponseItem::CustomToolCall { input, .. } => !input.is_empty(),
                    ResponseItem::LocalShellCall {
                        action: codex_protocol::models::LocalShellAction::Exec(action),
                        ..
                    } => action.command.iter().any(|part| !part.is_empty()),
                    ResponseItem::ToolSearchCall { arguments, .. } => {
                        !arguments.is_null()
                            && arguments.as_str().is_none_or(|text| !text.is_empty())
                    }
                    _ => false,
                }
        }
        _ => false,
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
        | ResponseEvent::ReasoningSummaryDone { text, .. }
        | ResponseEvent::ToolCallInputDelta { delta: text, .. } => !text.is_empty(),
        ResponseEvent::Created
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
    match item {
        ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::ImageGenerationCall { .. } => true,
        // Hosted web search and server-side tool search finish inside the provider and
        // give the client neither an executable call nor answer text (see ToolRouter).
        ResponseItem::ToolSearchCall { execution, .. } => execution == "client",
        ResponseItem::Message { .. } => {
            raw_assistant_output_text_from_item(item).is_some_and(|text| !text.is_empty())
        }
        _ => false,
    }
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

fn u128_to_u64_ms(nanos: u128) -> u64 {
    u64::try_from(nanos / NANOS_PER_MILLISECOND).unwrap_or(u64::MAX)
}

fn record_tool_process_exit(
    tool_call: &mut TurnTimingToolCall,
    timing: &ToolDispatchTimingSnapshot,
) {
    let process_exit_event = timing
        .lifecycle_events
        .iter()
        .find(|event| event.boundary == ToolLifecycleBoundary::ProcessExit);
    tool_call.process_exited_at_ms = process_exit_event.map(|event| event.at_ms).or_else(|| {
        tool_call
            .process_spawned_at_ms
            .zip(timing.exec_spawn_to_exit_ms)
            .map(|(spawned_at_ms, spawn_to_exit_ms)| spawned_at_ms.saturating_add(spawn_to_exit_ms))
    });
    if let Some(event) = process_exit_event
        && !tool_call
            .lifecycle_events
            .iter()
            .any(|existing| existing.boundary == ToolLifecycleBoundary::ProcessExit)
    {
        tool_call.lifecycle_events.push(event.clone());
        tool_call.lifecycle_events.sort_by_key(|event| event.at_ms);
    }
}

fn lifecycle_boundary_at(
    events: &[codex_protocol::protocol::TurnTimingToolLifecycleEvent],
    boundary: ToolLifecycleBoundary,
) -> Option<u64> {
    events
        .iter()
        .find(|event| event.boundary == boundary)
        .map(|event| event.at_ms)
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

fn wait_generations_with_same_revision_count(requests: &[ModelRequestTiming]) -> u32 {
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

fn usage_coverage<'a>(requests: impl Iterator<Item = &'a ModelRequestTiming>) -> (u64, u64) {
    let mut expected = 0_u64;
    let mut observed = 0_u64;
    let mut seen = std::collections::BTreeSet::new();
    for request in requests {
        expected = expected.saturating_add(request.physical_attempt_ids.len().max(1) as u64);
        // A logical row with several attempts and one usage object cannot certify
        // each physical attempt. Missing identities and duplicates stay unknown.
        if request.physical_attempt_ids.len() == 1
            && !request.physical_attempt_ids[0].is_empty()
            && seen.insert(request.physical_attempt_ids[0].as_str())
            && request.token_usage.is_some()
        {
            observed = observed.saturating_add(1);
        }
    }
    (expected, observed)
}

fn receipt_component_coverage<'a>(
    requests: impl Iterator<Item = &'a ModelRequestTiming>,
) -> Option<codex_protocol::protocol::TokenUsageCoverage> {
    let mut coverage = codex_protocol::protocol::TokenUsageCoverage {
        reported_receipts: Some(0),
        ..Default::default()
    };
    let mut seen = std::collections::BTreeSet::new();
    for request in requests {
        // A row's receipt cannot establish per-component coverage for several
        // attempts, missing identities, or the same physical work twice.
        if request.physical_attempt_ids.len() != 1
            || request.physical_attempt_ids[0].is_empty()
            || !seen.insert(request.physical_attempt_ids[0].as_str())
        {
            return None;
        }
        match request.token_usage.as_ref() {
            Some(usage) => {
                let receipt = usage.coverage.as_ref()?;
                if receipt.receipts != 1 {
                    return None;
                }
                coverage.add_assign(receipt);
            }
            None => coverage.receipts = coverage.receipts.saturating_add(1),
        }
    }
    Some(coverage)
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
            let (expected, observed) = usage_coverage(matching.iter().copied());
            Some(TurnTimingGenerationPurposeAggregate {
                coverage: receipt_component_coverage(matching.iter().copied()),
                output_excluding_reported_reasoning_tokens: if expected == observed {
                    matching.iter().try_fold(0_u64, |total, request| {
                        total.checked_add(
                            request
                                .token_usage
                                .as_ref()?
                                .output_excluding_reported_reasoning_tokens?,
                        )
                    })
                } else {
                    None
                },
                expected_usage_count: Some(expected),
                observed_usage_count: Some(observed),
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
    let (expected, observed) = usage_coverage(
        requests
            .iter()
            .filter(|request| generation_ids.contains(&request.generation_index)),
    );
    let mut aggregate = TurnTimingDiagnosticTokenAggregate {
        output_excluding_reported_reasoning_tokens: (!input_only && expected == observed)
            .then_some(0),
        coverage: receipt_component_coverage(
            requests
                .iter()
                .filter(|request| generation_ids.contains(&request.generation_index)),
        ),
        expected_usage_count: Some(expected),
        observed_usage_count: Some(observed),
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
            aggregate.output_excluding_reported_reasoning_tokens = aggregate
                .output_excluding_reported_reasoning_tokens
                .zip(usage.output_excluding_reported_reasoning_tokens)
                .and_then(|(total, tokens)| total.checked_add(tokens));
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
        physical_attempts: matching
            .iter()
            .fold(0_usize, |total, request| {
                total.saturating_add(request.physical_attempt_count())
            })
            .try_into()
            .unwrap_or(u32::MAX),
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
