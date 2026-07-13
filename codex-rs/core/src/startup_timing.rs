use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const STARTUP_TIMING_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug)]
struct ClockSample {
    monotonic_ns: u128,
    wall_unix_ms: i64,
}

trait StartupClock: Send + Sync {
    fn sample(&self) -> ClockSample;
}

#[derive(Debug)]
struct SystemStartupClock {
    origin: Instant,
}

impl Default for SystemStartupClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl StartupClock for SystemStartupClock {
    fn sample(&self) -> ClockSample {
        ClockSample {
            monotonic_ns: Instant::now()
                .saturating_duration_since(self.origin)
                .as_nanos(),
            wall_unix_ms: now_unix_timestamp_ms(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupPhase {
    TransportPreconnect,
    PrewarmPreparation,
    PrewarmRequest,
    FirstTurnPrewarmWait,
    ExecutorReadiness,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StartupPhaseTiming {
    pub(crate) session_initialization_ns: u128,
    pub(crate) transport_preconnect_ns: u128,
    pub(crate) prewarm_preparation_ns: u128,
    pub(crate) prewarm_request_ns: u128,
    pub(crate) first_turn_prewarm_wait_ns: u128,
    pub(crate) executor_readiness_ns: u128,
    pub(crate) preconnect_preparation_overlap_ns: u128,
    pub(crate) preconnect_executor_overlap_ns: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StartupTimingSnapshot {
    pub(crate) schema_version: u16,
    pub(crate) correlation_id: String,
    pub(crate) started_at_unix_ms: i64,
    pub(crate) completed_at_unix_ms: i64,
    pub(crate) inclusive_duration_ns: u128,
    pub(crate) profile_valid: bool,
    pub(crate) prewarm_status: Option<String>,
    pub(crate) phases: StartupPhaseTiming,
    pub(crate) invalid_transition_count: u32,
    pub(crate) clock_regression_count: u32,
    pub(crate) saturation_count: u32,
}

pub(crate) struct StartupTimingState {
    clock: Arc<dyn StartupClock>,
    inner: Mutex<StartupTimingInner>,
}

impl std::fmt::Debug for StartupTimingState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StartupTimingState")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ActivePhases {
    transport_preconnect: u32,
    prewarm_preparation: u32,
    prewarm_request: u32,
    first_turn_prewarm_wait: u32,
    executor_readiness: u32,
}

#[derive(Debug)]
struct StartupTimingInner {
    correlation_id: String,
    started: ClockSample,
    last_monotonic_ns: u128,
    session_initializing: bool,
    active: ActivePhases,
    phases: StartupPhaseTiming,
    prewarm_status: Option<String>,
    invalid_transition_count: u32,
    clock_regression_count: u32,
    saturation_count: u32,
    completed_snapshot: Option<StartupTimingSnapshot>,
}

#[must_use]
pub(crate) struct StartupTimingGuard {
    timing: Arc<StartupTimingState>,
    phase: StartupPhase,
    active: bool,
}

impl StartupTimingState {
    pub(crate) fn new(correlation_id: String) -> Arc<Self> {
        Self::with_clock(correlation_id, Arc::new(SystemStartupClock::default()))
    }

    fn with_clock(correlation_id: String, clock: Arc<dyn StartupClock>) -> Arc<Self> {
        let started = clock.sample();
        Arc::new(Self {
            clock,
            inner: Mutex::new(StartupTimingInner {
                correlation_id,
                started,
                last_monotonic_ns: started.monotonic_ns,
                session_initializing: true,
                active: ActivePhases::default(),
                phases: StartupPhaseTiming::default(),
                prewarm_status: None,
                invalid_transition_count: 0,
                clock_regression_count: 0,
                saturation_count: 0,
                completed_snapshot: None,
            }),
        })
    }

    pub(crate) fn finish_session_initialization(&self) {
        let sample = self.clock.sample();
        let mut inner = self.inner();
        inner.advance(sample.monotonic_ns);
        if inner.completed_snapshot.is_some() || !inner.session_initializing {
            inner.invalid_transition();
            return;
        }
        inner.session_initializing = false;
    }

    pub(crate) fn begin_phase(self: &Arc<Self>, phase: StartupPhase) -> StartupTimingGuard {
        let sample = self.clock.sample();
        let active = self.inner().begin_phase(sample.monotonic_ns, phase);
        StartupTimingGuard {
            timing: Arc::clone(self),
            phase,
            active,
        }
    }

    pub(crate) fn record_prewarm_status(&self, status: impl Into<String>) {
        let mut inner = self.inner();
        if inner.completed_snapshot.is_none() {
            inner.prewarm_status = Some(status.into());
        }
    }

    /// Freeze the correlated startup snapshot at the first real model-send boundary.
    /// Repeated calls return the exact same snapshot.
    pub(crate) fn complete_snapshot(&self) -> StartupTimingSnapshot {
        let sample = self.clock.sample();
        self.inner().complete(sample)
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, StartupTimingInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for StartupTimingGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let sample = self.timing.clock.sample();
        self.timing
            .inner()
            .end_phase(sample.monotonic_ns, self.phase);
    }
}

impl StartupTimingInner {
    fn begin_phase(&mut self, now_ns: u128, phase: StartupPhase) -> bool {
        if self.completed_snapshot.is_some() {
            return false;
        }
        self.advance(now_ns);
        let counter = self.counter_mut(phase);
        if *counter == u32::MAX {
            self.saturation_count = self.saturation_count.saturating_add(1);
            return false;
        }
        *counter += 1;
        true
    }

    fn end_phase(&mut self, now_ns: u128, phase: StartupPhase) {
        if self.completed_snapshot.is_some() {
            return;
        }
        self.advance(now_ns);
        let counter = self.counter_mut(phase);
        if *counter == 0 {
            self.invalid_transition();
        } else {
            *counter -= 1;
        }
    }

    fn advance(&mut self, observed_now_ns: u128) {
        let now_ns = if observed_now_ns < self.last_monotonic_ns {
            self.clock_regression_count = self.clock_regression_count.saturating_add(1);
            self.last_monotonic_ns
        } else {
            observed_now_ns
        };
        let elapsed_ns = now_ns.saturating_sub(self.last_monotonic_ns);
        self.last_monotonic_ns = now_ns;
        if elapsed_ns == 0 {
            return;
        }

        if self.session_initializing {
            add_saturating(
                &mut self.phases.session_initialization_ns,
                elapsed_ns,
                &mut self.saturation_count,
            );
        }
        if self.active.transport_preconnect > 0 {
            add_saturating(
                &mut self.phases.transport_preconnect_ns,
                elapsed_ns,
                &mut self.saturation_count,
            );
        }
        if self.active.prewarm_preparation > 0 {
            add_saturating(
                &mut self.phases.prewarm_preparation_ns,
                elapsed_ns,
                &mut self.saturation_count,
            );
        }
        if self.active.prewarm_request > 0 {
            add_saturating(
                &mut self.phases.prewarm_request_ns,
                elapsed_ns,
                &mut self.saturation_count,
            );
        }
        if self.active.first_turn_prewarm_wait > 0 {
            add_saturating(
                &mut self.phases.first_turn_prewarm_wait_ns,
                elapsed_ns,
                &mut self.saturation_count,
            );
        }
        if self.active.executor_readiness > 0 {
            add_saturating(
                &mut self.phases.executor_readiness_ns,
                elapsed_ns,
                &mut self.saturation_count,
            );
        }
        if self.active.transport_preconnect > 0 && self.active.prewarm_preparation > 0 {
            add_saturating(
                &mut self.phases.preconnect_preparation_overlap_ns,
                elapsed_ns,
                &mut self.saturation_count,
            );
        }
        if self.active.transport_preconnect > 0 && self.active.executor_readiness > 0 {
            add_saturating(
                &mut self.phases.preconnect_executor_overlap_ns,
                elapsed_ns,
                &mut self.saturation_count,
            );
        }
    }

    fn complete(&mut self, sample: ClockSample) -> StartupTimingSnapshot {
        if let Some(snapshot) = &self.completed_snapshot {
            return snapshot.clone();
        }
        self.advance(sample.monotonic_ns);
        if self.session_initializing {
            self.invalid_transition();
            self.session_initializing = false;
        }
        let active_count = self.active.transport_preconnect
            + self.active.prewarm_preparation
            + self.active.prewarm_request
            + self.active.first_turn_prewarm_wait
            + self.active.executor_readiness;
        if active_count != 0 {
            self.invalid_transition();
        }
        let snapshot = StartupTimingSnapshot {
            schema_version: STARTUP_TIMING_SCHEMA_VERSION,
            correlation_id: self.correlation_id.clone(),
            started_at_unix_ms: self.started.wall_unix_ms,
            completed_at_unix_ms: sample.wall_unix_ms,
            inclusive_duration_ns: self
                .last_monotonic_ns
                .saturating_sub(self.started.monotonic_ns),
            profile_valid: self.invalid_transition_count == 0
                && self.clock_regression_count == 0
                && self.saturation_count == 0,
            prewarm_status: self.prewarm_status.clone(),
            phases: self.phases.clone(),
            invalid_transition_count: self.invalid_transition_count,
            clock_regression_count: self.clock_regression_count,
            saturation_count: self.saturation_count,
        };
        self.completed_snapshot = Some(snapshot.clone());
        snapshot
    }

    fn counter_mut(&mut self, phase: StartupPhase) -> &mut u32 {
        match phase {
            StartupPhase::TransportPreconnect => &mut self.active.transport_preconnect,
            StartupPhase::PrewarmPreparation => &mut self.active.prewarm_preparation,
            StartupPhase::PrewarmRequest => &mut self.active.prewarm_request,
            StartupPhase::FirstTurnPrewarmWait => &mut self.active.first_turn_prewarm_wait,
            StartupPhase::ExecutorReadiness => &mut self.active.executor_readiness,
        }
    }

    fn invalid_transition(&mut self) {
        self.invalid_transition_count = self.invalid_transition_count.saturating_add(1);
    }
}

fn add_saturating(target: &mut u128, elapsed_ns: u128, saturation_count: &mut u32) {
    let (value, saturated) = target.overflowing_add(elapsed_ns);
    if saturated {
        *target = u128::MAX;
        *saturation_count = saturation_count.saturating_add(1);
    } else {
        *target = value;
    }
}

fn now_unix_timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI64;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    #[derive(Debug, Default)]
    struct ManualClock {
        monotonic_ns: AtomicU64,
        wall_unix_ms: AtomicI64,
    }

    impl ManualClock {
        fn advance(&self, nanos: u64) {
            self.monotonic_ns.fetch_add(nanos, Ordering::SeqCst);
            self.wall_unix_ms
                .fetch_add((nanos / 1_000_000) as i64, Ordering::SeqCst);
        }
    }

    impl StartupClock for ManualClock {
        fn sample(&self) -> ClockSample {
            ClockSample {
                monotonic_ns: self.monotonic_ns.load(Ordering::SeqCst).into(),
                wall_unix_ms: self.wall_unix_ms.load(Ordering::SeqCst),
            }
        }
    }

    #[test]
    fn snapshot_tracks_overlap_and_freezes_at_first_send() {
        let clock = Arc::new(ManualClock::default());
        let timing = StartupTimingState::with_clock("thread-1".into(), clock.clone());
        clock.advance(5);
        timing.finish_session_initialization();

        let preconnect = timing.begin_phase(StartupPhase::TransportPreconnect);
        clock.advance(7);
        let preparation = timing.begin_phase(StartupPhase::PrewarmPreparation);
        clock.advance(11);
        drop(preparation);
        clock.advance(13);
        drop(preconnect);
        timing.record_prewarm_status("ready");

        let snapshot = timing.complete_snapshot();
        assert!(snapshot.profile_valid);
        assert_eq!(snapshot.phases.session_initialization_ns, 5);
        assert_eq!(snapshot.phases.transport_preconnect_ns, 31);
        assert_eq!(snapshot.phases.prewarm_preparation_ns, 11);
        assert_eq!(snapshot.phases.preconnect_preparation_overlap_ns, 11);
        assert_eq!(snapshot.prewarm_status.as_deref(), Some("ready"));

        clock.advance(100);
        assert_eq!(timing.complete_snapshot(), snapshot);
    }
}
