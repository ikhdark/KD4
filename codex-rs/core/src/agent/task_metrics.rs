//! Bounded task measurements and deterministic replay fixtures for MultiAgentV2.
//!
//! Caller-controlled measurements accepted here contain no strings, paths, byte buffers, or
//! arbitrary metadata. This module adds only static metric names and closed, low-cardinality tag
//! values, and never tags metrics with its opaque task identifier. This is a boundary on fields
//! supplied by this module, not a claim about the complete export envelope: `SessionTelemetry`
//! may attach its standard session metadata.

use codex_agent_task_store::AgentRole;
use codex_agent_task_store::AgentStatusClaim;
use codex_agent_task_store::AgentTask;
use codex_agent_task_store::Assignment;
use codex_agent_task_store::AttemptState;
use codex_agent_task_store::CONCURRENT_DRIFT_REASON;
use codex_agent_task_store::CapabilityProfile;
use codex_agent_task_store::CriterionStatus;
use codex_agent_task_store::GateKind;
use codex_agent_task_store::GateStatus;
use codex_agent_task_store::ValidationCallStatus;
use codex_otel::SessionTelemetry;
use std::collections::BTreeMap;
use std::time::Duration;
use std::time::Instant;

const MAX_METRIC_ROWS: usize = 256;
pub(super) const MAX_RECORDED_EVENTS: usize = 4_096;

const TASK_DURATION_METRIC: &str = "codex.multi_agent.task.duration_ms";
const TASK_CRITICAL_PATH_IDLE_DURATION_METRIC: &str =
    "codex.multi_agent.task.critical_path_idle.duration_ms";
const TASK_CRITICAL_PATH_IDLE_RATIO_METRIC: &str =
    "codex.multi_agent.task.critical_path_idle.basis_points";
const TASK_TOKEN_USAGE_METRIC: &str = "codex.multi_agent.task.token_usage";
const TASK_INFERENCE_CALLS_METRIC: &str = "codex.multi_agent.task.inference_calls";
const TASK_CONCURRENCY_UTILIZATION_METRIC: &str =
    "codex.multi_agent.task.concurrency_utilization.basis_points";
const TASK_FIRST_PASS_VALIDATION_METRIC: &str = "codex.multi_agent.task.first_pass_validation";
const TASK_ACCEPTANCE_CLOSURE_METRIC: &str =
    "codex.multi_agent.task.acceptance_closure.basis_points";
const TASK_DUPLICATE_WORK_METRIC: &str = "codex.multi_agent.task.duplicate_work";
const TASK_CONFLICT_METRIC: &str = "codex.multi_agent.task.conflict";
const TASK_DRIFT_METRIC: &str = "codex.multi_agent.task.drift";
const TASK_REVIEWER_FINDING_METRIC: &str = "codex.multi_agent.task.reviewer_finding";
const TASK_CORRECTION_METRIC: &str = "codex.multi_agent.task.correction";
const TASK_WAIVER_METRIC: &str = "codex.multi_agent.task.waiver";
const TASK_VIOLATION_METRIC: &str = "codex.multi_agent.task.violation";
const TASK_OUTCOME_METRIC: &str = "codex.multi_agent.task.outcome";

const ROLE_TAG: &str = "role";
const CAPABILITY_TAG: &str = "capability";
const OUTCOME_TAG: &str = "outcome";
const SUCCEEDED_TAG: &str = "succeeded";
const PHASE_TAG: &str = "phase";
const DISPOSITION_TAG: &str = "disposition";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct OpaqueId(u128);

impl OpaqueId {
    pub(crate) const fn new(value: u128) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum RoleLabel {
    #[cfg(test)]
    Root,
    Explorer,
    Worker,
    Reviewer,
    Verifier,
    Integrator,
    #[cfg(test)]
    Legacy,
}

impl RoleLabel {
    const fn as_str(self) -> &'static str {
        match self {
            #[cfg(test)]
            Self::Root => "root",
            Self::Explorer => "explorer",
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
            Self::Verifier => "verifier",
            Self::Integrator => "integrator",
            #[cfg(test)]
            Self::Legacy => "legacy",
        }
    }
}

impl From<AgentRole> for RoleLabel {
    fn from(role: AgentRole) -> Self {
        match role {
            AgentRole::Explorer => Self::Explorer,
            AgentRole::Worker => Self::Worker,
            AgentRole::Reviewer => Self::Reviewer,
            AgentRole::Verifier => Self::Verifier,
            AgentRole::Integrator => Self::Integrator,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum CapabilityLabel {
    ReadSearch,
    ReadSearchDiff,
    ReadSearchShell,
    ScopedWrite,
    CrossOwnerWrite,
    #[cfg(test)]
    Legacy,
}

impl CapabilityLabel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ReadSearch => "read_search",
            Self::ReadSearchDiff => "read_search_diff",
            Self::ReadSearchShell => "read_search_shell",
            Self::ScopedWrite => "scoped_write",
            Self::CrossOwnerWrite => "cross_owner_write",
            #[cfg(test)]
            Self::Legacy => "legacy",
        }
    }
}

impl From<CapabilityProfile> for CapabilityLabel {
    fn from(capability: CapabilityProfile) -> Self {
        match capability {
            CapabilityProfile::ReadSearch => Self::ReadSearch,
            CapabilityProfile::ReadSearchDiff => Self::ReadSearchDiff,
            CapabilityProfile::ReadSearchShell => Self::ReadSearchShell,
            CapabilityProfile::ScopedSourceWrite => Self::ScopedWrite,
            CapabilityProfile::IntegratorSourceWrite => Self::CrossOwnerWrite,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RoleCapability {
    pub role: RoleLabel,
    pub capability: CapabilityLabel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoleUsage {
    pub role: RoleLabel,
    pub capability: CapabilityLabel,
    pub tokens: u64,
    pub calls: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct UsageTotals {
    pub tokens: u64,
    pub calls: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConcurrencySlice {
    pub duration: Duration,
    pub active_turns: u32,
    pub capacity: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FindingTotals {
    pub confirmed: u32,
    pub rejected: u32,
    pub unresolved: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinalOutcome {
    Completed,
    NeedsMain,
    Blocked,
    Failed,
    Violated,
    Abandoned,
}

impl FinalOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::NeedsMain => "needs_main",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Violated => "violated",
            Self::Abandoned => "abandoned",
        }
    }
}

impl From<AgentStatusClaim> for FinalOutcome {
    fn from(status: AgentStatusClaim) -> Self {
        match status {
            AgentStatusClaim::Completed => Self::Completed,
            AgentStatusClaim::NeedsMain => Self::NeedsMain,
            AgentStatusClaim::Blocked => Self::Blocked,
            AgentStatusClaim::Failed => Self::Failed,
            AgentStatusClaim::Violated => Self::Violated,
            AgentStatusClaim::Abandoned => Self::Abandoned,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskMetricInput {
    pub task_id: OpaqueId,
    pub duration: Duration,
    pub critical_path_idle_time: Duration,
    pub role_usage: Vec<RoleUsage>,
    #[cfg(test)]
    pub concurrency: Vec<ConcurrencySlice>,
    pub first_pass_validation_succeeded: bool,
    pub acceptance_total: u32,
    pub acceptance_first_pass_closed: u32,
    pub acceptance_final_closed: u32,
    pub duplicate_work: u32,
    pub conflicts: u32,
    pub drift: u32,
    pub reviewer_findings: FindingTotals,
    pub corrections: u32,
    pub waivers: u32,
    pub violations: u32,
    pub final_outcome: FinalOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskMetrics {
    pub task_id: OpaqueId,
    pub duration: Duration,
    pub critical_path_idle_time: Duration,
    pub critical_path_idle_basis_points: u16,
    pub total_usage: UsageTotals,
    pub usage_by_role: BTreeMap<RoleCapability, UsageTotals>,
    pub concurrency_utilization_basis_points: u16,
    pub first_pass_validation_succeeded: bool,
    pub acceptance_total: u32,
    pub acceptance_first_pass_closed: u32,
    pub acceptance_final_closed: u32,
    pub first_pass_acceptance_basis_points: u16,
    pub final_acceptance_basis_points: u16,
    pub duplicate_work: u32,
    pub conflicts: u32,
    pub drift: u32,
    pub reviewer_findings: FindingTotals,
    pub corrections: u32,
    pub waivers: u32,
    pub violations: u32,
    pub final_outcome: FinalOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskMetricTerminalInput {
    pub first_pass_validation_succeeded: bool,
    pub acceptance_total: u32,
    pub acceptance_first_pass_closed: u32,
    pub acceptance_final_closed: u32,
    pub duplicate_work: u32,
    pub conflicts: u32,
    pub drift: u32,
    pub reviewer_findings: FindingTotals,
    pub corrections: u32,
    pub waivers: u32,
    pub violations: u32,
    pub final_outcome: FinalOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetricsError {
    TooManyRows,
    TooManyEvents,
    RecorderFinished,
    InvalidEventTime,
    InvalidCriticalPathIdleTime,
    InvalidConcurrencySlice,
    InvalidAcceptanceClosure,
    ArithmeticOverflow,
}

impl TaskMetrics {
    #[cfg(test)]
    pub(crate) fn evaluate(input: TaskMetricInput) -> Result<Self, MetricsError> {
        if input.role_usage.len() > MAX_METRIC_ROWS || input.concurrency.len() > MAX_METRIC_ROWS {
            return Err(MetricsError::TooManyRows);
        }

        let mut active_time = Duration::ZERO;
        let mut capacity_time = Duration::ZERO;
        for slice in &input.concurrency {
            if slice.capacity == 0 || slice.active_turns > slice.capacity {
                return Err(MetricsError::InvalidConcurrencySlice);
            }
            active_time = weighted_add(active_time, slice.duration, slice.active_turns)?;
            capacity_time = weighted_add(capacity_time, slice.duration, slice.capacity)?;
        }

        Self::evaluate_with_concurrency_totals(input, active_time, capacity_time)
    }

    fn evaluate_with_concurrency_totals(
        input: TaskMetricInput,
        active_time: Duration,
        capacity_time: Duration,
    ) -> Result<Self, MetricsError> {
        if input.role_usage.len() > MAX_METRIC_ROWS {
            return Err(MetricsError::TooManyRows);
        }
        if input.critical_path_idle_time > input.duration {
            return Err(MetricsError::InvalidCriticalPathIdleTime);
        }
        if input.acceptance_first_pass_closed > input.acceptance_final_closed
            || input.acceptance_final_closed > input.acceptance_total
        {
            return Err(MetricsError::InvalidAcceptanceClosure);
        }

        let mut total_usage = UsageTotals::default();
        let mut usage_by_role = BTreeMap::<RoleCapability, UsageTotals>::new();
        for usage in input.role_usage {
            let totals = usage_by_role
                .entry(RoleCapability {
                    role: usage.role,
                    capability: usage.capability,
                })
                .or_default();
            add_usage(totals, usage.tokens, usage.calls)?;
            add_usage(&mut total_usage, usage.tokens, usage.calls)?;
        }

        Ok(Self {
            task_id: input.task_id,
            duration: input.duration,
            critical_path_idle_time: input.critical_path_idle_time,
            critical_path_idle_basis_points: ratio(
                input.critical_path_idle_time.as_nanos(),
                input.duration.as_nanos(),
            ),
            total_usage,
            usage_by_role,
            concurrency_utilization_basis_points: ratio(
                active_time.as_nanos(),
                capacity_time.as_nanos(),
            ),
            first_pass_validation_succeeded: input.first_pass_validation_succeeded,
            acceptance_total: input.acceptance_total,
            acceptance_first_pass_closed: input.acceptance_first_pass_closed,
            acceptance_final_closed: input.acceptance_final_closed,
            first_pass_acceptance_basis_points: ratio(
                u128::from(input.acceptance_first_pass_closed),
                u128::from(input.acceptance_total),
            ),
            final_acceptance_basis_points: ratio(
                u128::from(input.acceptance_final_closed),
                u128::from(input.acceptance_total),
            ),
            duplicate_work: input.duplicate_work,
            conflicts: input.conflicts,
            drift: input.drift,
            reviewer_findings: input.reviewer_findings,
            corrections: input.corrections,
            waivers: input.waivers,
            violations: input.violations,
            final_outcome: input.final_outcome,
        })
    }

    /// Emit a terminal metric set. This is private so callers must pass through the recorder's
    /// emit-once terminal transition.
    fn emit(&self, session_telemetry: &SessionTelemetry) {
        let outcome = self.final_outcome.as_str();
        let outcome_tags = [(OUTCOME_TAG, outcome)];
        session_telemetry.record_duration(TASK_DURATION_METRIC, self.duration, &outcome_tags);
        session_telemetry.record_duration(
            TASK_CRITICAL_PATH_IDLE_DURATION_METRIC,
            self.critical_path_idle_time,
            &outcome_tags,
        );
        session_telemetry.histogram(
            TASK_CRITICAL_PATH_IDLE_RATIO_METRIC,
            i64::from(self.critical_path_idle_basis_points),
            &outcome_tags,
        );
        session_telemetry.histogram(
            TASK_CONCURRENCY_UTILIZATION_METRIC,
            i64::from(self.concurrency_utilization_basis_points),
            &[],
        );

        for (role_capability, usage) in &self.usage_by_role {
            let tags = [
                (ROLE_TAG, role_capability.role.as_str()),
                (CAPABILITY_TAG, role_capability.capability.as_str()),
            ];
            session_telemetry.histogram(
                TASK_TOKEN_USAGE_METRIC,
                saturating_metric_value(usage.tokens),
                &tags,
            );
            session_telemetry.histogram(
                TASK_INFERENCE_CALLS_METRIC,
                saturating_metric_value(usage.calls),
                &tags,
            );
        }

        session_telemetry.counter(
            TASK_FIRST_PASS_VALIDATION_METRIC,
            1,
            &[(
                SUCCEEDED_TAG,
                bool_tag(self.first_pass_validation_succeeded),
            )],
        );
        session_telemetry.histogram(
            TASK_ACCEPTANCE_CLOSURE_METRIC,
            i64::from(self.first_pass_acceptance_basis_points),
            &[(PHASE_TAG, "first_pass")],
        );
        session_telemetry.histogram(
            TASK_ACCEPTANCE_CLOSURE_METRIC,
            i64::from(self.final_acceptance_basis_points),
            &[(PHASE_TAG, "final")],
        );
        session_telemetry.histogram(
            TASK_DUPLICATE_WORK_METRIC,
            i64::from(self.duplicate_work),
            &[],
        );
        session_telemetry.histogram(TASK_CONFLICT_METRIC, i64::from(self.conflicts), &[]);
        session_telemetry.histogram(TASK_DRIFT_METRIC, i64::from(self.drift), &[]);
        for (disposition, count) in [
            ("confirmed", self.reviewer_findings.confirmed),
            ("rejected", self.reviewer_findings.rejected),
            ("unresolved", self.reviewer_findings.unresolved),
        ] {
            session_telemetry.histogram(
                TASK_REVIEWER_FINDING_METRIC,
                i64::from(count),
                &[(DISPOSITION_TAG, disposition)],
            );
        }
        session_telemetry.histogram(TASK_CORRECTION_METRIC, i64::from(self.corrections), &[]);
        session_telemetry.histogram(TASK_WAIVER_METRIC, i64::from(self.waivers), &[]);
        session_telemetry.histogram(TASK_VIOLATION_METRIC, i64::from(self.violations), &[]);
        session_telemetry.counter(TASK_OUTCOME_METRIC, 1, &outcome_tags);
    }
}

/// Bounded, online recorder for one task. Elapsed timestamps are monotonic durations measured
/// from task start. An interval is critical-path idle when its reported active-turn count is zero.
#[derive(Debug)]
pub(crate) struct TaskMetricRecorder {
    task_id: OpaqueId,
    last_elapsed: Duration,
    active_turns: u32,
    capacity: u32,
    weighted_active_time: Duration,
    weighted_capacity_time: Duration,
    critical_path_idle_time: Duration,
    usage_by_role: BTreeMap<RoleCapability, UsageTotals>,
    recorded_events: usize,
    finished: bool,
}

impl TaskMetricRecorder {
    pub(crate) fn new(
        task_id: OpaqueId,
        active_turns: u32,
        capacity: u32,
    ) -> Result<Self, MetricsError> {
        validate_concurrency_state(active_turns, capacity)?;
        Ok(Self {
            task_id,
            last_elapsed: Duration::ZERO,
            active_turns,
            capacity,
            weighted_active_time: Duration::ZERO,
            weighted_capacity_time: Duration::ZERO,
            critical_path_idle_time: Duration::ZERO,
            usage_by_role: BTreeMap::new(),
            recorded_events: 0,
            finished: false,
        })
    }

    #[cfg(test)]
    fn recorded_events(&self) -> usize {
        self.recorded_events
    }

    #[cfg(test)]
    pub(crate) fn is_terminal(&self) -> bool {
        self.finished
    }

    pub(crate) fn record_role_usage(
        &mut self,
        role: RoleLabel,
        capability: CapabilityLabel,
        tokens: u64,
        calls: u64,
    ) -> Result<(), MetricsError> {
        self.ensure_recordable()?;
        let key = RoleCapability { role, capability };
        if !self.usage_by_role.contains_key(&key) && self.usage_by_role.len() >= MAX_METRIC_ROWS {
            return Err(MetricsError::TooManyRows);
        }
        let mut next = self.usage_by_role.get(&key).copied().unwrap_or_default();
        add_usage(&mut next, tokens, calls)?;
        self.reserve_non_terminal_event()?;
        self.usage_by_role.insert(key, next);
        Ok(())
    }

    pub(crate) fn record_store_role_usage(
        &mut self,
        role: AgentRole,
        capability: CapabilityProfile,
        tokens: u64,
        calls: u64,
    ) -> Result<(), MetricsError> {
        self.record_role_usage(role.into(), capability.into(), tokens, calls)
    }

    pub(crate) fn transition_concurrency(
        &mut self,
        elapsed: Duration,
        active_turns: u32,
        capacity: u32,
    ) -> Result<(), MetricsError> {
        self.ensure_recordable()?;
        validate_concurrency_state(active_turns, capacity)?;
        let (weighted_active_time, weighted_capacity_time, critical_path_idle_time) =
            self.integrated_until(elapsed)?;
        self.reserve_non_terminal_event()?;
        self.last_elapsed = elapsed;
        self.active_turns = active_turns;
        self.capacity = capacity;
        self.weighted_active_time = weighted_active_time;
        self.weighted_capacity_time = weighted_capacity_time;
        self.critical_path_idle_time = critical_path_idle_time;
        Ok(())
    }

    /// Finish exactly once. A repeated terminal signal is ignored and returns `Ok(None)`.
    fn finish(
        &mut self,
        elapsed: Duration,
        terminal: TaskMetricTerminalInput,
    ) -> Result<Option<TaskMetrics>, MetricsError> {
        if self.finished {
            return Ok(None);
        }
        let (weighted_active_time, weighted_capacity_time, critical_path_idle_time) =
            self.integrated_until(elapsed)?;
        let role_usage = self
            .usage_by_role
            .iter()
            .map(|(key, totals)| RoleUsage {
                role: key.role,
                capability: key.capability,
                tokens: totals.tokens,
                calls: totals.calls,
            })
            .collect();
        let metrics = TaskMetrics::evaluate_with_concurrency_totals(
            TaskMetricInput {
                task_id: self.task_id,
                duration: elapsed,
                critical_path_idle_time,
                role_usage,
                #[cfg(test)]
                concurrency: Vec::new(),
                first_pass_validation_succeeded: terminal.first_pass_validation_succeeded,
                acceptance_total: terminal.acceptance_total,
                acceptance_first_pass_closed: terminal.acceptance_first_pass_closed,
                acceptance_final_closed: terminal.acceptance_final_closed,
                duplicate_work: terminal.duplicate_work,
                conflicts: terminal.conflicts,
                drift: terminal.drift,
                reviewer_findings: terminal.reviewer_findings,
                corrections: terminal.corrections,
                waivers: terminal.waivers,
                violations: terminal.violations,
                final_outcome: terminal.final_outcome,
            },
            weighted_active_time,
            weighted_capacity_time,
        )?;
        self.reserve_terminal_event()?;
        self.last_elapsed = elapsed;
        self.weighted_active_time = weighted_active_time;
        self.weighted_capacity_time = weighted_capacity_time;
        self.critical_path_idle_time = critical_path_idle_time;
        self.finished = true;
        Ok(Some(metrics))
    }

    pub(crate) fn finish_and_emit(
        &mut self,
        elapsed: Duration,
        terminal: TaskMetricTerminalInput,
        session_telemetry: &SessionTelemetry,
    ) -> Result<Option<TaskMetrics>, MetricsError> {
        self.finish_with(elapsed, terminal, |metrics| {
            metrics.emit(session_telemetry);
        })
    }

    fn finish_with(
        &mut self,
        elapsed: Duration,
        terminal: TaskMetricTerminalInput,
        emit: impl FnOnce(&TaskMetrics),
    ) -> Result<Option<TaskMetrics>, MetricsError> {
        let metrics = self.finish(elapsed, terminal)?;
        if let Some(metrics) = metrics.as_ref() {
            emit(metrics);
        }
        Ok(metrics)
    }

    fn ensure_recordable(&self) -> Result<(), MetricsError> {
        if self.finished {
            Err(MetricsError::RecorderFinished)
        } else {
            Ok(())
        }
    }

    fn reserve_non_terminal_event(&mut self) -> Result<(), MetricsError> {
        self.reserve_event(MAX_RECORDED_EVENTS.saturating_sub(1))
    }

    fn reserve_terminal_event(&mut self) -> Result<(), MetricsError> {
        self.reserve_event(MAX_RECORDED_EVENTS)
    }

    fn reserve_event(&mut self, limit: usize) -> Result<(), MetricsError> {
        self.ensure_recordable()?;
        if self.recorded_events >= limit {
            return Err(MetricsError::TooManyEvents);
        }
        self.recorded_events += 1;
        Ok(())
    }

    fn integrated_until(
        &self,
        elapsed: Duration,
    ) -> Result<(Duration, Duration, Duration), MetricsError> {
        let interval = elapsed
            .checked_sub(self.last_elapsed)
            .ok_or(MetricsError::InvalidEventTime)?;
        let weighted_active_time =
            weighted_add(self.weighted_active_time, interval, self.active_turns)?;
        let weighted_capacity_time =
            weighted_add(self.weighted_capacity_time, interval, self.capacity)?;
        let critical_path_idle_time = if self.active_turns == 0 {
            self.critical_path_idle_time
                .checked_add(interval)
                .ok_or(MetricsError::ArithmeticOverflow)?
        } else {
            self.critical_path_idle_time
        };
        Ok((
            weighted_active_time,
            weighted_capacity_time,
            critical_path_idle_time,
        ))
    }
}

/// Process-local measurements for one durable typed task.
///
/// Persistence remains authoritative for task state. This runtime owns only bounded counters and
/// monotonic timestamps, so it can be dropped safely on restart without affecting task recovery.
#[derive(Debug)]
pub(crate) struct TaskMetricRuntime {
    started_at: Instant,
    role: AgentRole,
    capability: CapabilityProfile,
    recorder: TaskMetricRecorder,
}

impl TaskMetricRuntime {
    pub(crate) fn new(
        assignment: &Assignment,
        active_turns: u32,
        capacity: u32,
    ) -> Result<Self, MetricsError> {
        Ok(Self {
            started_at: Instant::now(),
            role: assignment.role,
            capability: assignment.capability_profile,
            recorder: TaskMetricRecorder::new(
                OpaqueId::new(assignment.assignment_id.as_uuid().as_u128()),
                active_turns,
                capacity,
            )?,
        })
    }

    pub(crate) fn transition_concurrency(
        &mut self,
        active_turns: u32,
        capacity: u32,
    ) -> Result<(), MetricsError> {
        self.recorder
            .transition_concurrency(self.started_at.elapsed(), active_turns, capacity)
    }

    pub(crate) fn record_usage(&mut self, tokens: u64, calls: u64) -> Result<(), MetricsError> {
        self.recorder
            .record_store_role_usage(self.role, self.capability, tokens, calls)
    }

    pub(crate) fn finish_and_emit(
        &mut self,
        task: &AgentTask,
        session_telemetry: &SessionTelemetry,
    ) -> Result<bool, MetricsError> {
        self.recorder
            .finish_and_emit(
                self.started_at.elapsed(),
                terminal_input(task),
                session_telemetry,
            )
            .map(|metrics| metrics.is_some())
    }
}

pub(crate) fn terminal_metrics_ready(task: &AgentTask) -> bool {
    if !task.current_attempt.state.is_terminal()
        || task.gates.iter().any(|gate| !gate.status.is_sealed())
    {
        return false;
    }

    // The first review rejection is an explicit correction opportunity, not a final outcome.
    !(task.current_attempt.ordinal == 0
        && task.gates.iter().any(|gate| {
            gate.kind == GateKind::Review && gate.status == GateStatus::ChangesRequested
        }))
}

fn terminal_input(task: &AgentTask) -> TaskMetricTerminalInput {
    let receipt = task.receipt.as_ref();
    let acceptance_total = saturating_u32(task.assignment.acceptance_criteria.len());
    let acceptance_final_closed = receipt
        .map(|receipt| {
            saturating_u32(
                receipt
                    .criterion_results
                    .iter()
                    .filter(|criterion| criterion.status == CriterionStatus::Passed)
                    .count(),
            )
        })
        .unwrap_or(0);
    let acceptance_first_pass_closed = if task.current_attempt.ordinal == 0 {
        acceptance_final_closed
    } else {
        0
    };
    let first_pass_validation_succeeded = task.current_attempt.ordinal == 0
        && receipt.is_some_and(|receipt| {
            receipt.status == AgentStatusClaim::Completed
                && !receipt.validation_call_ids.is_empty()
                && receipt.validation_call_ids.iter().all(|call_id| {
                    task.validation_calls.iter().any(|call| {
                        call.call_id == *call_id && call.status == ValidationCallStatus::Succeeded
                    })
                })
        });

    let mut reviewer_findings = FindingTotals::default();
    for gate in task
        .gates
        .iter()
        .filter(|gate| gate.kind == GateKind::Review)
    {
        match gate.status {
            GateStatus::ChangesRequested | GateStatus::Failed | GateStatus::Violated => {
                reviewer_findings.confirmed = reviewer_findings.confirmed.saturating_add(1);
            }
            GateStatus::Waived | GateStatus::Pending => {
                reviewer_findings.unresolved = reviewer_findings.unresolved.saturating_add(1);
            }
            GateStatus::Passed => {}
        }
    }

    let conflicts = saturating_u32(
        task.gates
            .iter()
            .filter(|gate| {
                gate.kind == GateKind::Ownership
                    && matches!(gate.status, GateStatus::Failed | GateStatus::Violated)
            })
            .count(),
    );
    let drift = u32::from(task.gates.iter().any(|gate| {
        if gate.kind != GateKind::Risk || gate.status != GateStatus::Passed {
            return false;
        }
        let reasons = gate
            .reason
            .strip_prefix("cold review required: ")
            .unwrap_or(&gate.reason);
        reasons
            .split("; ")
            .any(|reason| reason == CONCURRENT_DRIFT_REASON)
    }));
    let waivers = saturating_u32(
        task.gates
            .iter()
            .filter(|gate| gate.status == GateStatus::Waived)
            .count(),
    );
    let gate_violations = task
        .gates
        .iter()
        .filter(|gate| gate.status == GateStatus::Violated)
        .count();
    let violations = saturating_u32(gate_violations).saturating_add(u32::from(
        task.current_attempt.state == AttemptState::Violated && gate_violations == 0,
    ));

    TaskMetricTerminalInput {
        first_pass_validation_succeeded,
        acceptance_total,
        acceptance_first_pass_closed,
        acceptance_final_closed,
        duplicate_work: 0,
        conflicts,
        drift,
        reviewer_findings,
        corrections: u32::from(task.current_attempt.ordinal),
        waivers,
        violations,
        final_outcome: final_outcome(task),
    }
}

fn final_outcome(task: &AgentTask) -> FinalOutcome {
    match task.current_attempt.state {
        AttemptState::Completed => task
            .receipt
            .as_ref()
            .map(|receipt| receipt.status.into())
            .unwrap_or(FinalOutcome::Completed),
        AttemptState::NeedsMain => match task.receipt.as_ref().map(|receipt| receipt.status) {
            Some(AgentStatusClaim::Blocked) => FinalOutcome::Blocked,
            Some(AgentStatusClaim::Failed) => FinalOutcome::Failed,
            _ => FinalOutcome::NeedsMain,
        },
        AttemptState::Violated => FinalOutcome::Violated,
        AttemptState::Abandoned => FinalOutcome::Abandoned,
        AttemptState::Active => FinalOutcome::NeedsMain,
    }
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn validate_concurrency_state(active_turns: u32, capacity: u32) -> Result<(), MetricsError> {
    if capacity == 0 || active_turns > capacity {
        Err(MetricsError::InvalidConcurrencySlice)
    } else {
        Ok(())
    }
}

fn bool_tag(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn saturating_metric_value(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn add_usage(totals: &mut UsageTotals, tokens: u64, calls: u64) -> Result<(), MetricsError> {
    totals.tokens = totals
        .tokens
        .checked_add(tokens)
        .ok_or(MetricsError::ArithmeticOverflow)?;
    totals.calls = totals
        .calls
        .checked_add(calls)
        .ok_or(MetricsError::ArithmeticOverflow)?;
    Ok(())
}

fn weighted_add(
    total: Duration,
    duration: Duration,
    weight: u32,
) -> Result<Duration, MetricsError> {
    duration
        .checked_mul(weight)
        .and_then(|weighted| total.checked_add(weighted))
        .ok_or(MetricsError::ArithmeticOverflow)
}

fn ratio(numerator: u128, denominator: u128) -> u16 {
    if denominator == 0 {
        0
    } else {
        numerator
            .saturating_mul(10_000)
            .div_euclid(denominator)
            .min(10_000) as u16
    }
}

#[cfg(test)]
mod replay {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum MutationDisposition {
        NotAttempted,
        Blocked,
        DetectionOnlyViolation,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum SpawnDisposition {
        Accepted,
        RejectedDependency,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum GateDisposition {
        DirectCompletion,
        ReviewThenVerification,
        CorrectionThenVerification,
        NeedsMain,
        NotEntered,
        LegacyUnchanged,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum DiffEvidence {
        NotRequired,
        AttemptSnapshot,
        Missing,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum WatermarkEvidence {
        NotRequired,
        Reconstructed,
        Missing,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum DependencyState {
        Successful,
        Incomplete,
        Blocked,
        Failed,
        Violated,
        Abandoned,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct ReplayInput {
        pub typed: bool,
        pub legacy: bool,
        pub cross_owner: bool,
        pub cold_review_required: bool,
        pub review_defect_found: bool,
        pub unauthorized_patch: bool,
        pub unauthorized_shell: bool,
        pub shell_enforcement_supported: bool,
        pub file_was_dirty: bool,
        pub private_snapshot_present: bool,
        pub concurrent_drift: bool,
        pub restarted: bool,
        pub watermark_rebuilt: bool,
        pub dependency_state: DependencyState,
        pub correction_requests: u32,
        pub focused_validation_succeeded: bool,
        pub verification_succeeded: bool,
    }

    impl Default for ReplayInput {
        fn default() -> Self {
            Self {
                typed: true,
                legacy: false,
                cross_owner: false,
                cold_review_required: false,
                review_defect_found: false,
                unauthorized_patch: false,
                unauthorized_shell: false,
                shell_enforcement_supported: true,
                file_was_dirty: false,
                private_snapshot_present: false,
                concurrent_drift: false,
                restarted: false,
                watermark_rebuilt: false,
                dependency_state: DependencyState::Successful,
                correction_requests: 0,
                focused_validation_succeeded: true,
                verification_succeeded: true,
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct ReplayResult {
        pub spawn: SpawnDisposition,
        pub patch: MutationDisposition,
        pub shell: MutationDisposition,
        pub gate: GateDisposition,
        pub diff: DiffEvidence,
        pub watermark: WatermarkEvidence,
        pub correction_attempts_allowed: u32,
        pub outcome: FinalOutcome,
    }

    pub(crate) fn evaluate_replay(input: ReplayInput) -> ReplayResult {
        let spawn = if input.typed && input.dependency_state != DependencyState::Successful {
            SpawnDisposition::RejectedDependency
        } else {
            SpawnDisposition::Accepted
        };
        let patch = if input.unauthorized_patch {
            MutationDisposition::Blocked
        } else {
            MutationDisposition::NotAttempted
        };
        let shell = if input.unauthorized_shell && input.shell_enforcement_supported {
            MutationDisposition::Blocked
        } else if input.unauthorized_shell {
            MutationDisposition::DetectionOnlyViolation
        } else {
            MutationDisposition::NotAttempted
        };
        let diff = if input.file_was_dirty && input.private_snapshot_present {
            DiffEvidence::AttemptSnapshot
        } else if input.file_was_dirty {
            DiffEvidence::Missing
        } else {
            DiffEvidence::NotRequired
        };
        let watermark = if input.restarted && input.watermark_rebuilt {
            WatermarkEvidence::Reconstructed
        } else if input.restarted {
            WatermarkEvidence::Missing
        } else {
            WatermarkEvidence::NotRequired
        };
        let violation = input.unauthorized_patch || input.unauthorized_shell;
        let needs_main = input.concurrent_drift
            || input.correction_requests > 1
            || diff == DiffEvidence::Missing
            || watermark == WatermarkEvidence::Missing;
        let gate = if input.legacy {
            GateDisposition::LegacyUnchanged
        } else if spawn == SpawnDisposition::RejectedDependency {
            GateDisposition::NotEntered
        } else if violation || needs_main {
            GateDisposition::NeedsMain
        } else if input.review_defect_found {
            GateDisposition::CorrectionThenVerification
        } else if input.cross_owner || input.cold_review_required {
            GateDisposition::ReviewThenVerification
        } else {
            GateDisposition::DirectCompletion
        };
        let outcome = if input.legacy {
            FinalOutcome::Completed
        } else if spawn == SpawnDisposition::RejectedDependency {
            FinalOutcome::Blocked
        } else if violation {
            FinalOutcome::Violated
        } else if needs_main
            || !input.focused_validation_succeeded
            || ((input.cross_owner || input.cold_review_required || input.review_defect_found)
                && !input.verification_succeeded)
        {
            FinalOutcome::NeedsMain
        } else {
            FinalOutcome::Completed
        };
        ReplayResult {
            spawn,
            patch,
            shell,
            gate,
            diff,
            watermark,
            correction_attempts_allowed: u32::from(input.typed && input.correction_requests > 0),
            outcome,
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ReplayScenarioKind {
        BoundedFix,
        CrossOwner,
        ColdReviewDetection,
        UnauthorizedPatchAndShell,
        DirtyFileDiff,
        ConcurrentDrift,
        RestartWatermark,
        DependencyRejection,
        CorrectionBounds,
        LegacyCompatibility,
    }

    impl ReplayScenarioKind {
        const ALL: [Self; 10] = [
            Self::BoundedFix,
            Self::CrossOwner,
            Self::ColdReviewDetection,
            Self::UnauthorizedPatchAndShell,
            Self::DirtyFileDiff,
            Self::ConcurrentDrift,
            Self::RestartWatermark,
            Self::DependencyRejection,
            Self::CorrectionBounds,
            Self::LegacyCompatibility,
        ];

        fn input(self) -> ReplayInput {
            let mut input = ReplayInput::default();
            match self {
                Self::BoundedFix => {}
                Self::CrossOwner => input.cross_owner = true,
                Self::ColdReviewDetection => {
                    input.cold_review_required = true;
                    input.review_defect_found = true;
                    input.correction_requests = 1;
                }
                Self::UnauthorizedPatchAndShell => {
                    input.unauthorized_patch = true;
                    input.unauthorized_shell = true;
                    input.shell_enforcement_supported = false;
                }
                Self::DirtyFileDiff => {
                    input.file_was_dirty = true;
                    input.private_snapshot_present = true;
                }
                Self::ConcurrentDrift => input.concurrent_drift = true,
                Self::RestartWatermark => {
                    input.restarted = true;
                    input.watermark_rebuilt = true;
                }
                Self::DependencyRejection => input.dependency_state = DependencyState::Incomplete,
                Self::CorrectionBounds => {
                    input.review_defect_found = true;
                    input.correction_requests = 2;
                }
                Self::LegacyCompatibility => {
                    input.typed = false;
                    input.legacy = true;
                    input.focused_validation_succeeded = false;
                }
            }
            input
        }

        fn expected(self) -> ReplayResult {
            let mut expected = ReplayResult {
                spawn: SpawnDisposition::Accepted,
                patch: MutationDisposition::NotAttempted,
                shell: MutationDisposition::NotAttempted,
                gate: GateDisposition::DirectCompletion,
                diff: DiffEvidence::NotRequired,
                watermark: WatermarkEvidence::NotRequired,
                correction_attempts_allowed: 0,
                outcome: FinalOutcome::Completed,
            };
            match self {
                Self::BoundedFix => {}
                Self::CrossOwner => expected.gate = GateDisposition::ReviewThenVerification,
                Self::ColdReviewDetection => {
                    expected.gate = GateDisposition::CorrectionThenVerification;
                    expected.correction_attempts_allowed = 1;
                }
                Self::UnauthorizedPatchAndShell => {
                    expected.patch = MutationDisposition::Blocked;
                    expected.shell = MutationDisposition::DetectionOnlyViolation;
                    expected.gate = GateDisposition::NeedsMain;
                    expected.outcome = FinalOutcome::Violated;
                }
                Self::DirtyFileDiff => expected.diff = DiffEvidence::AttemptSnapshot,
                Self::ConcurrentDrift => {
                    expected.gate = GateDisposition::NeedsMain;
                    expected.outcome = FinalOutcome::NeedsMain;
                }
                Self::RestartWatermark => expected.watermark = WatermarkEvidence::Reconstructed,
                Self::DependencyRejection => {
                    expected.spawn = SpawnDisposition::RejectedDependency;
                    expected.gate = GateDisposition::NotEntered;
                    expected.outcome = FinalOutcome::Blocked;
                }
                Self::CorrectionBounds => {
                    expected.gate = GateDisposition::NeedsMain;
                    expected.correction_attempts_allowed = 1;
                    expected.outcome = FinalOutcome::NeedsMain;
                }
                Self::LegacyCompatibility => expected.gate = GateDisposition::LegacyUnchanged,
            }
            expected
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct ReplayScenario {
        pub kind: ReplayScenarioKind,
        pub input: ReplayInput,
        pub expected: ReplayResult,
    }

    pub(crate) fn replay_scenarios() -> Vec<ReplayScenario> {
        ReplayScenarioKind::ALL
            .into_iter()
            .map(|kind| {
                let input = kind.input();
                ReplayScenario {
                    kind,
                    input,
                    expected: kind.expected(),
                }
            })
            .collect()
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct RunCost {
        pub correct: bool,
        pub wall_time: Duration,
        pub tokens: u64,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Comparison {
        Better,
        Equal,
        Worse,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct RunComparison {
        pub correctness: Comparison,
        pub wall_time: Comparison,
        pub token_cost: Comparison,
    }

    pub(crate) fn compare_multi_agent_to_single_agent(
        single: RunCost,
        multi: RunCost,
    ) -> RunComparison {
        RunComparison {
            correctness: compare(multi.correct, single.correct, false),
            wall_time: compare(multi.wall_time, single.wall_time, true),
            token_cost: compare(multi.tokens, single.tokens, true),
        }
    }

    fn compare<T: Ord>(candidate: T, baseline: T, lower_is_better: bool) -> Comparison {
        match (candidate.cmp(&baseline), lower_is_better) {
            (std::cmp::Ordering::Equal, _) => Comparison::Equal,
            (std::cmp::Ordering::Less, true) | (std::cmp::Ordering::Greater, false) => {
                Comparison::Better
            }
            _ => Comparison::Worse,
        }
    }
}

#[cfg(test)]
use replay::*;

#[cfg(test)]
#[path = "task_metrics_tests.rs"]
mod tests;
