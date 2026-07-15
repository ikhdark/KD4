use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_analytics::TurnProfile;
use codex_otel::TURN_TTFM_DURATION_METRIC;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TurnTiming;
use codex_protocol::protocol::TurnTimingCounters;
use codex_protocol::protocol::TurnTimingExclusive;
use codex_protocol::protocol::TurnTimingLocal;
use codex_protocol::protocol::TurnTimingMilestones;
use codex_protocol::protocol::TurnTimingUnions;

use crate::ResponseEvent;
use crate::session::turn_context::TurnContext;
use crate::stream_events_utils::raw_assistant_output_text_from_item;

const NANOS_PER_MILLISECOND: u128 = 1_000_000;
const TIMING_SCHEMA_VERSION: u16 = 1;
const MODEL_OUTPUT_SETTLED: u8 = 1 << 0;
const VISIBLE_OUTPUT_SETTLED: u8 = 1 << 1;
const AGENT_MESSAGE_SETTLED: u8 = 1 << 2;

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
    milestone_mask: AtomicU8,
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
            first_model_output_ms: profile
                .milestones
                .first_model_output_ns
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
        let profile_valid =
            profile.profile_valid && saturation_count == profile.counters.saturation_count;
        let counters = TurnTimingCounters {
            model_request_count: profile.counters.model_request_count,
            model_retry_count: profile.counters.model_retry_count,
            model_fallback_count: profile.counters.model_fallback_count,
            tool_call_count: profile.counters.tool_call_count,
            approval_wait_count: profile.counters.approval_wait_count,
            permission_wait_count: profile.counters.permission_wait_count,
            user_input_wait_count: profile.counters.user_input_wait_count,
            mcp_elicitation_wait_count: profile.counters.mcp_elicitation_wait_count,
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
    pub(crate) compaction_ns: u128,
    pub(crate) persistence_ns: u128,
    pub(crate) serialization_ns: u128,
    pub(crate) router_build_ns: u128,
    pub(crate) startup_prewarm_wait_ns: u128,
    pub(crate) executor_readiness_wait_ns: u128,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TimingMilestones {
    pub(crate) first_model_output_ns: Option<u128>,
    pub(crate) first_visible_output_ns: Option<u128>,
    pub(crate) first_agent_message_ns: Option<u128>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TimingCounters {
    pub(crate) model_request_count: u32,
    pub(crate) model_retry_count: u32,
    pub(crate) model_fallback_count: u32,
    pub(crate) tool_call_count: u32,
    pub(crate) approval_wait_count: u32,
    pub(crate) permission_wait_count: u32,
    pub(crate) user_input_wait_count: u32,
    pub(crate) mcp_elicitation_wait_count: u32,
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
    legacy: LegacyProfileState,
    completed_snapshot: Option<TurnTimingSnapshot>,
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
    compaction: u32,
    persistence: u32,
    serialization: u32,
    router_build: u32,
    startup_prewarm_wait: u32,
    executor_readiness_wait: u32,
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
    Compaction,
    Persistence,
    Serialization,
    RouterBuild,
    StartupPrewarmWait,
    ExecutorReadinessWait,
}

#[must_use]
pub(crate) struct TurnTimingGuard {
    timing: Arc<TurnTimingState>,
    kind: GuardKind,
    active: bool,
}

/// Owns the wait/processing timing transition for one response stream request.
///
/// Dropping or explicitly finishing this guard closes the currently active phase exactly once.
#[must_use]
pub(crate) struct ModelStreamTimingGuard {
    timing: Option<Arc<TurnTimingState>>,
    active: Option<TurnTimingGuard>,
    finalized: bool,
}

impl ModelStreamTimingGuard {
    pub(crate) fn new(timing: Option<&Arc<TurnTimingState>>) -> Self {
        Self {
            timing: timing.cloned(),
            active: None,
            finalized: false,
        }
    }

    pub(crate) fn begin_wait(&mut self) {
        self.transition(GuardKind::ModelStreamWait);
    }

    pub(crate) fn begin_processing(&mut self) {
        self.transition(GuardKind::ModelStreamProcessing);
    }

    pub(crate) fn finish(&mut self) {
        if self.finalized {
            return;
        }
        self.active.take();
        self.finalized = true;
    }

    fn transition(&mut self, kind: GuardKind) {
        if self.finalized {
            return;
        }
        self.active.take();
        self.active = self.timing.as_ref().map(|timing| timing.begin_guard(kind));
    }
}

impl Drop for ModelStreamTimingGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

impl TurnTimingState {
    fn new(clock: Arc<dyn TurnClock>) -> Self {
        Self {
            clock,
            state: StdMutex::new(TurnTimingStateInner::default()),
            milestone_mask: AtomicU8::new(0),
        }
    }

    #[cfg(test)]
    fn with_clock(clock: Arc<dyn TurnClock>) -> Self {
        Self::new(clock)
    }

    pub(crate) fn mark_turn_started(&self) -> i64 {
        let sample = self.clock.sample();
        let mut state = self.state();
        state.start(sample);
        self.milestone_mask.store(0, Ordering::Release);
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
        state.counters.model_retry_count = state.counters.model_retry_count.saturating_add(1);
    }

    pub(crate) fn record_model_fallback(&self) {
        let mut state = self.state();
        state.counters.model_fallback_count = state.counters.model_fallback_count.saturating_add(1);
    }

    pub(crate) fn record_tool_call(&self) {
        let mut state = self.state();
        state.counters.tool_call_count = state.counters.tool_call_count.saturating_add(1);
    }

    pub(crate) fn begin_tool_blocking(self: &Arc<Self>) -> TurnTimingGuard {
        self.begin_guard(GuardKind::LegacyToolBlocking)
    }

    pub(crate) fn begin_model_request_wait(self: &Arc<Self>) -> TurnTimingGuard {
        {
            let mut state = self.state();
            state.counters.model_request_count =
                state.counters.model_request_count.saturating_add(1);
        }
        self.begin_guard(GuardKind::ModelRequestWait)
    }

    #[allow(dead_code)]
    pub(crate) fn begin_model_stream_wait(self: &Arc<Self>) -> TurnTimingGuard {
        self.begin_guard(GuardKind::ModelStreamWait)
    }

    #[allow(dead_code)]
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
        let (visible_duration, publish) = self.commit_response_event_milestones(event);
        self.publish_milestones(publish);
        visible_duration
    }

    fn commit_response_event_milestones(&self, event: &ResponseEvent) -> (Option<Duration>, u8) {
        let records_model_output = response_event_records_model_output(event);
        let records_visible_output = response_event_records_visible_output(event);
        if !records_model_output && !records_visible_output {
            return (None, 0);
        }
        let settled = self.milestone_mask.load(Ordering::Acquire);
        let needs_model = records_model_output && settled & MODEL_OUTPUT_SETTLED == 0;
        let needs_visible = records_visible_output && settled & VISIBLE_OUTPUT_SETTLED == 0;
        if !needs_model && !needs_visible {
            return (None, 0);
        }
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns) else {
            return (None, 0);
        };
        if records_model_output && state.milestones.first_model_output_ns.is_none() {
            state.milestones.first_model_output_ns = Some(elapsed_ns);
        }
        let visible_duration =
            if records_visible_output && state.milestones.first_visible_output_ns.is_none() {
                state.milestones.first_visible_output_ns = Some(elapsed_ns);
                Some(duration_from_nanos(elapsed_ns))
            } else {
                None
            };
        let mut publish = 0;
        if needs_model && state.milestones.first_model_output_ns.is_some() {
            publish |= MODEL_OUTPUT_SETTLED;
        }
        if needs_visible && state.milestones.first_visible_output_ns.is_some() {
            publish |= VISIBLE_OUTPUT_SETTLED;
        }
        (visible_duration, publish)
    }

    pub(crate) fn record_ttfm_for_turn_item(&self, item: &TurnItem) -> Option<Duration> {
        let (duration, publish) = self.commit_agent_message_milestone(item);
        self.publish_milestones(publish);
        duration
    }

    fn commit_agent_message_milestone(&self, item: &TurnItem) -> (Option<Duration>, u8) {
        if !matches!(item, TurnItem::AgentMessage(_)) {
            return (None, 0);
        }
        if self.milestone_mask.load(Ordering::Acquire) & AGENT_MESSAGE_SETTLED != 0 {
            return (None, 0);
        }
        let sample = self.clock.sample();
        let mut state = self.state();
        state.advance(sample.time.monotonic_ns);
        if state.milestones.first_agent_message_ns.is_some() {
            return (None, AGENT_MESSAGE_SETTLED);
        }
        let Some(elapsed_ns) = state.elapsed_since_start(sample.time.monotonic_ns) else {
            return (None, 0);
        };
        state.milestones.first_agent_message_ns = Some(elapsed_ns);
        (Some(duration_from_nanos(elapsed_ns)), AGENT_MESSAGE_SETTLED)
    }

    fn publish_milestones(&self, publish: u8) {
        if publish != 0 {
            self.milestone_mask.fetch_or(publish, Ordering::Release);
        }
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
        };
        let legacy_profile = self
            .legacy
            .complete(self.last_monotonic_ns.unwrap_or(sample.time.monotonic_ns));
        let snapshot = TurnTimingSnapshot {
            started_at_unix_ms: started_sample.map(|started| started.time.wall_unix_ms),
            completed_at_unix_ms: started_sample.map(|_| sample.time.wall_unix_ms),
            completed_at_unix_secs: started_sample.map(|_| sample.time.wall_unix_ms / 1_000),
            duration_ms: started_sample.map(|_| u128_to_i64_ms(inclusive_duration_ns)),
            time_to_first_token_ms: self.milestones.first_visible_output_ns.map(u128_to_i64_ms),
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
            TurnLocalPhase::Compaction => &mut self.activity.compaction,
            TurnLocalPhase::Persistence => &mut self.activity.persistence,
            TurnLocalPhase::Serialization => &mut self.activity.serialization,
            TurnLocalPhase::RouterBuild => &mut self.activity.router_build,
            TurnLocalPhase::StartupPrewarmWait => &mut self.activity.startup_prewarm_wait,
            TurnLocalPhase::ExecutorReadinessWait => &mut self.activity.executor_readiness_wait,
        };
        *counter = counter.saturating_add(1);
    }

    fn decrement_local_activity(&mut self, phase: TurnLocalPhase) -> bool {
        let counter = match phase {
            TurnLocalPhase::Preparation => &mut self.activity.preparation,
            TurnLocalPhase::Planning => &mut self.activity.planning,
            TurnLocalPhase::Compaction => &mut self.activity.compaction,
            TurnLocalPhase::Persistence => &mut self.activity.persistence,
            TurnLocalPhase::Serialization => &mut self.activity.serialization,
            TurnLocalPhase::RouterBuild => &mut self.activity.router_build,
            TurnLocalPhase::StartupPrewarmWait => &mut self.activity.startup_prewarm_wait,
            TurnLocalPhase::ExecutorReadinessWait => &mut self.activity.executor_readiness_wait,
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

fn response_event_records_model_output(event: &ResponseEvent) -> bool {
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

fn response_event_records_visible_output(event: &ResponseEvent) -> bool {
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

fn public_ms(nanos: u128, saturation_count: &mut u32) -> u64 {
    public_ns(nanos / NANOS_PER_MILLISECOND, saturation_count)
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
