use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const STARTUP_TIMING_SCHEMA_VERSION: u16 = 3;

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
    pub(crate) prewarm_cancel_requested: bool,
    /// Aggregate of the initial selected environments, not process-manager allocation.
    pub(crate) executor_status: &'static str,
    pub(crate) phases: StartupPhaseTiming,
    pub(crate) invalid_transition_count: u32,
    pub(crate) clock_regression_count: u32,
    pub(crate) saturation_count: u32,
}

impl StartupTimingSnapshot {
    pub(crate) fn trace_frozen_at_first_model_send(&self) {
        tracing::trace!(
            target: "codex_core::session::turn",
            startup_timing_schema_version = self.schema_version,
            startup_timing_boundary = "first_client_transport_attempt_entry",
            startup_timing_correlation_id = %self.correlation_id,
            startup_timing_started_at_unix_ms = self.started_at_unix_ms,
            startup_timing_completed_at_unix_ms = self.completed_at_unix_ms,
            startup_timing_duration_ns = self.inclusive_duration_ns,
            startup_timing_profile_valid = self.profile_valid,
            startup_prewarm_status = ?self.prewarm_status,
            startup_prewarm_cancel_requested = self.prewarm_cancel_requested,
            startup_executor_status = self.executor_status,
            startup_session_initialization_ns = self.phases.session_initialization_ns,
            startup_transport_preconnect_ns = self.phases.transport_preconnect_ns,
            startup_prewarm_preparation_ns = self.phases.prewarm_preparation_ns,
            startup_prewarm_request_ns = self.phases.prewarm_request_ns,
            startup_first_turn_wait_ns = self.phases.first_turn_prewarm_wait_ns,
            startup_executor_readiness_ns = self.phases.executor_readiness_ns,
            startup_preconnect_preparation_overlap_ns = self
                .phases
                .preconnect_preparation_overlap_ns,
            startup_preconnect_executor_overlap_ns = self.phases.preconnect_executor_overlap_ns,
            startup_timing_invalid_transition_count = self.invalid_transition_count,
            startup_timing_clock_regression_count = self.clock_regression_count,
            startup_timing_saturation_count = self.saturation_count,
            "startup timing snapshot frozen at first client transport attempt entry"
        );
    }
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
    prewarm_cancel_requested: bool,
    executor_status: &'static str,
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
                prewarm_cancel_requested: false,
                executor_status: "not_observed",
                invalid_transition_count: 0,
                clock_regression_count: 0,
                saturation_count: 0,
                completed_snapshot: None,
            }),
        })
    }

    pub(crate) fn finish_session_initialization(&self) {
        let mut inner = self.inner();
        let sample = self.clock.sample();
        inner.advance(sample.monotonic_ns);
        if inner.completed_snapshot.is_some() || !inner.session_initializing {
            inner.invalid_transition();
            return;
        }
        inner.session_initializing = false;
    }

    pub(crate) fn begin_phase(self: &Arc<Self>, phase: StartupPhase) -> StartupTimingGuard {
        let active = {
            let mut inner = self.inner();
            let sample = self.clock.sample();
            inner.begin_phase(sample.monotonic_ns, phase)
        };
        StartupTimingGuard {
            timing: Arc::clone(self),
            phase,
            active,
        }
    }

    pub(crate) fn record_prewarm_status(&self, status: impl Into<String>) {
        let mut inner = self.inner();
        if inner.completed_snapshot.is_none() && !inner.prewarm_cancel_requested {
            inner.prewarm_status = Some(status.into());
        }
    }

    /// Close speculative observation at handoff without awaiting task cleanup.
    /// Late task starts/drops are harmless; required phases remain strict.
    pub(crate) fn cutoff_speculative_prewarm(&self, status: &str) {
        let mut inner = self.inner();
        if inner.completed_snapshot.is_some() || inner.prewarm_cancel_requested {
            return;
        }
        inner.advance(self.clock.sample().monotonic_ns);
        inner.prewarm_cancel_requested = true;
        inner.prewarm_status = Some(status.to_string());
        inner.active.transport_preconnect = 0;
        inner.active.prewarm_preparation = 0;
        inner.active.prewarm_request = 0;
    }

    pub(crate) fn record_executor_status(&self, status: &'static str) {
        let mut inner = self.inner();
        if inner.completed_snapshot.is_none() {
            inner.executor_status = status;
        }
    }

    /// Freeze at the first real client transport-attempt entry (not wire send).
    /// Repeated calls return the exact same snapshot.
    #[cfg(test)]
    pub(crate) fn complete_snapshot(&self) -> StartupTimingSnapshot {
        let mut inner = self.inner();
        let sample = self.clock.sample();
        inner.complete(sample)
    }

    /// Client transport-attempt entry is not proof of successful send or provider receipt.
    /// Speculative generate=false traffic must not call this boundary.
    pub(crate) fn freeze_at_first_transport_attempt(&self) {
        let snapshot = {
            let mut inner = self.inner();
            if inner.completed_snapshot.is_some() {
                return;
            }
            inner.complete(self.clock.sample())
        };
        snapshot.trace_frozen_at_first_model_send();
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
        let mut inner = self.timing.inner();
        let sample = self.timing.clock.sample();
        inner.end_phase(sample.monotonic_ns, self.phase);
    }
}

impl StartupTimingInner {
    fn begin_phase(&mut self, now_ns: u128, phase: StartupPhase) -> bool {
        if self.completed_snapshot.is_some()
            || (self.prewarm_cancel_requested && phase.is_speculative())
        {
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
        if self.completed_snapshot.is_some()
            || (self.prewarm_cancel_requested && phase.is_speculative())
        {
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
        // Mandatory pre-turn compaction can enter the transport before the
        // regular-turn prewarm handoff. Observe unfinished speculative work
        // truthfully at this cutoff; only unfinished required work is invalid.
        let speculative_active = self.active.transport_preconnect > 0
            || self.active.prewarm_preparation > 0
            || self.active.prewarm_request > 0;
        if self.active.first_turn_prewarm_wait > 0 || self.active.executor_readiness > 0 {
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
            prewarm_status: if speculative_active {
                Some("in_progress_at_cutoff".to_string())
            } else {
                self.prewarm_status.clone()
            },
            prewarm_cancel_requested: self.prewarm_cancel_requested,
            executor_status: self.executor_status,
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

impl StartupPhase {
    fn is_speculative(self) -> bool {
        matches!(
            self,
            Self::TransportPreconnect | Self::PrewarmPreparation | Self::PrewarmRequest
        )
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
    use tracing::Level;
    use tracing_test::internal::MockWriter;

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
        assert_eq!(snapshot.inclusive_duration_ns, 36);
        assert_eq!(snapshot.phases.session_initialization_ns, 5);
        assert_eq!(snapshot.phases.transport_preconnect_ns, 31);
        assert_eq!(snapshot.phases.prewarm_preparation_ns, 11);
        assert_eq!(snapshot.phases.preconnect_preparation_overlap_ns, 11);
        assert_eq!(snapshot.prewarm_status.as_deref(), Some("ready"));

        clock.advance(100);
        timing.record_prewarm_status("late update");
        drop(timing.begin_phase(StartupPhase::ExecutorReadiness));
        assert_eq!(timing.complete_snapshot(), snapshot);
    }

    #[test]
    fn audit_observability_startup_samples_under_transition_lock() {
        #[derive(Default)]
        struct CheckingClock {
            owner: Mutex<std::sync::Weak<StartupTimingState>>,
            next: AtomicU64,
        }
        impl StartupClock for CheckingClock {
            fn sample(&self) -> ClockSample {
                if let Some(owner) = self.owner.lock().unwrap().upgrade() {
                    assert!(
                        matches!(
                            owner.inner.try_lock(),
                            Err(std::sync::TryLockError::WouldBlock)
                        ),
                        "clock sampling must be serialized with the transition"
                    );
                }
                ClockSample {
                    monotonic_ns: self.next.fetch_add(1, Ordering::SeqCst).into(),
                    wall_unix_ms: 0,
                }
            }
        }
        let clock = Arc::new(CheckingClock::default());
        let timing = StartupTimingState::with_clock("locked".into(), clock.clone());
        *clock.owner.lock().unwrap() = Arc::downgrade(&timing);
        timing.finish_session_initialization();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let timing = Arc::clone(&timing);
                scope.spawn(move || {
                    for _ in 0..32 {
                        drop(timing.begin_phase(StartupPhase::TransportPreconnect));
                    }
                });
            }
        });
        assert!(timing.complete_snapshot().profile_valid);
    }

    #[test]
    fn audit_observability_startup_preserves_real_clock_regressions() {
        let clock = Arc::new(ManualClock::default());
        let timing = StartupTimingState::with_clock("regression".into(), clock.clone());
        clock.advance(10);
        timing.finish_session_initialization();
        clock.monotonic_ns.store(5, Ordering::SeqCst);
        let snapshot = timing.complete_snapshot();
        assert_eq!(snapshot.clock_regression_count, 1);
        assert!(!snapshot.profile_valid);
    }

    #[test]
    fn startup_snapshot_trace_includes_every_snapshot_field() {
        let snapshot = StartupTimingSnapshot {
            schema_version: STARTUP_TIMING_SCHEMA_VERSION,
            correlation_id: "trace-thread".to_string(),
            started_at_unix_ms: 101,
            completed_at_unix_ms: 202,
            inclusive_duration_ns: 303,
            profile_valid: false,
            prewarm_status: Some("ready".to_string()),
            prewarm_cancel_requested: false,
            executor_status: "ready",
            phases: StartupPhaseTiming {
                session_initialization_ns: 401,
                transport_preconnect_ns: 402,
                prewarm_preparation_ns: 403,
                prewarm_request_ns: 404,
                first_turn_prewarm_wait_ns: 405,
                executor_readiness_ns: 406,
                preconnect_preparation_overlap_ns: 407,
                preconnect_executor_overlap_ns: 408,
            },
            invalid_transition_count: 501,
            clock_regression_count: 502,
            saturation_count: 503,
        };
        let buffer: &'static Mutex<Vec<u8>> = Box::leak(Box::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(Level::TRACE)
            .with_writer(MockWriter::new(buffer))
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        snapshot.trace_frozen_at_first_model_send();

        let logs = String::from_utf8(
            buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
        .expect("startup timing trace should be UTF-8");
        for expected in [
            "codex_core::session::turn",
            "startup_timing_schema_version=3",
            "startup_timing_boundary=\"first_client_transport_attempt_entry\"",
            "startup_timing_correlation_id=trace-thread",
            "startup_timing_started_at_unix_ms=101",
            "startup_timing_completed_at_unix_ms=202",
            "startup_timing_duration_ns=303",
            "startup_timing_profile_valid=false",
            "startup_prewarm_status=Some(\"ready\")",
            "startup_prewarm_cancel_requested=false",
            "startup_executor_status=\"ready\"",
            "startup_session_initialization_ns=401",
            "startup_transport_preconnect_ns=402",
            "startup_prewarm_preparation_ns=403",
            "startup_prewarm_request_ns=404",
            "startup_first_turn_wait_ns=405",
            "startup_executor_readiness_ns=406",
            "startup_preconnect_preparation_overlap_ns=407",
            "startup_preconnect_executor_overlap_ns=408",
            "startup_timing_invalid_transition_count=501",
            "startup_timing_clock_regression_count=502",
            "startup_timing_saturation_count=503",
            "startup timing snapshot frozen at first client transport attempt entry",
        ] {
            assert!(
                logs.contains(expected),
                "missing {expected:?} in trace: {logs}"
            );
        }
    }

    #[test]
    fn speculative_cutoff_closes_observation_before_late_cleanup() {
        let clock = Arc::new(ManualClock::default());
        let timing = StartupTimingState::with_clock("cutoff".into(), clock.clone());
        timing.finish_session_initialization();
        let preparation = timing.begin_phase(StartupPhase::PrewarmPreparation);
        let transport = timing.begin_phase(StartupPhase::TransportPreconnect);
        clock.advance(7);
        timing.cutoff_speculative_prewarm("not_ready");
        clock.advance(5);
        drop(timing.begin_phase(StartupPhase::PrewarmRequest));
        let snapshot = timing.complete_snapshot();
        assert!(snapshot.profile_valid);
        assert!(snapshot.prewarm_cancel_requested);
        assert_eq!(snapshot.phases.prewarm_preparation_ns, 7);
        assert_eq!(snapshot.phases.transport_preconnect_ns, 7);
        drop((preparation, transport));
        timing.record_prewarm_status("late ready");
        assert_eq!(timing.complete_snapshot(), snapshot);
    }

    #[test]
    fn speculative_cutoff_does_not_excuse_required_readiness() {
        let clock = Arc::new(ManualClock::default());
        let timing = StartupTimingState::with_clock("required".into(), clock);
        timing.finish_session_initialization();
        let _required = timing.begin_phase(StartupPhase::ExecutorReadiness);
        timing.cutoff_speculative_prewarm("not_ready");
        let snapshot = timing.complete_snapshot();
        assert!(!snapshot.profile_valid);
        assert_eq!(snapshot.invalid_transition_count, 1);
    }

    #[test]
    fn early_compaction_send_observes_unfinished_speculation_without_claiming_cancellation() {
        let clock = Arc::new(ManualClock::default());
        let timing = StartupTimingState::with_clock("early-compaction".into(), clock.clone());
        timing.finish_session_initialization();
        let preparation = timing.begin_phase(StartupPhase::PrewarmPreparation);
        clock.advance(9);
        let snapshot = timing.complete_snapshot();
        assert!(snapshot.profile_valid);
        assert_eq!(
            snapshot.prewarm_status.as_deref(),
            Some("in_progress_at_cutoff")
        );
        assert!(!snapshot.prewarm_cancel_requested);
        assert_eq!(snapshot.phases.prewarm_preparation_ns, 9);
        drop(preparation);
        assert_eq!(timing.complete_snapshot(), snapshot);
    }
}
