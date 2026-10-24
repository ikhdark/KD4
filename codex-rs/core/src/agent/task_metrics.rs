//! Bounded task measurements for MultiAgentV2.
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
use codex_otel::SessionTelemetry;
use std::collections::BTreeMap;
use std::time::Duration;
use std::time::Instant;

const MAX_METRIC_ROWS: usize = 256;
pub(super) const MAX_RECORDED_EVENTS: usize = 4_096;

const TASK_DURATION_METRIC: &str = "codex.multi_agent.task.duration_ms";
pub(super) const TASK_FIRST_MEANINGFUL_PROGRESS_DURATION_METRIC: &str =
    "codex.multi_agent.task.first_meaningful_progress.duration_ms";
const TASK_COORDINATOR_ZERO_ACTIVE_DURATION_METRIC: &str =
    "codex.multi_agent.task.coordinator_zero_active.duration_ms";
const TASK_COORDINATOR_ZERO_ACTIVE_RATIO_METRIC: &str =
    "codex.multi_agent.task.coordinator_zero_active.basis_points";
const TASK_TOKEN_USAGE_METRIC: &str = "codex.multi_agent.task.token_usage";
const TOKEN_CLASS_TAG: &str = "token_class";
const SUBTREE_TOKEN_USAGE_METRIC: &str = "codex.multi_agent.subtree.token_usage";
const SUBTREE_AGENT_COUNT_METRIC: &str = "codex.multi_agent.subtree.agents";
const SUBTREE_SCOPE_TAG: &str = "scope";
const TASK_INFERENCE_CALLS_METRIC: &str = "codex.multi_agent.task.inference_calls";
const TASK_CONCURRENCY_UTILIZATION_METRIC: &str =
    "codex.multi_agent.task.concurrency_utilization.basis_points";
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
const TASK_TERMINAL_STATE_METRIC: &str = "codex.multi_agent.task.terminal_state";

const ROLE_TAG: &str = "role";
const CAPABILITY_TAG: &str = "capability";
const OUTCOME_TAG: &str = "outcome";
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
    Architect,
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
            Self::Architect => "architect",
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
            AgentRole::Architect => Self::Architect,
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
    pub tokens: TokenClassTotals,
    pub calls: u64,
}

/// Per-class token accounting for one agent's model requests.
///
/// `total` is reported by the provider rather than derived, so a provider whose total
/// does not equal the sum of its classes is preserved as measured instead of silently
/// reconciled. Aggregate parent-plus-child comparisons need the classes separated:
/// cached input and reasoning output have materially different costs from fresh input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TokenClassTotals {
    pub coverage: Option<codex_protocol::protocol::TokenUsageCoverage>,
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub reasoning_output: u64,
    pub total: u64,
}

impl TokenClassTotals {
    pub(crate) fn from_usage(usage: &codex_protocol::protocol::TokenUsage) -> Self {
        Self {
            coverage: usage.coverage,
            input: usage.input_tokens.max(0) as u64,
            cached_input: usage.cached_input_tokens.max(0) as u64,
            output: usage.output_tokens.max(0) as u64,
            reasoning_output: usage.reasoning_output_tokens.max(0) as u64,
            total: usage.total_tokens.max(0) as u64,
        }
    }

    fn known(
        self,
        value: u64,
        field: impl FnOnce(&codex_protocol::protocol::TokenUsageCoverage) -> u64,
    ) -> Option<u64> {
        let coverage = self.coverage?;
        (!coverage.saturated && field(&coverage) == coverage.receipts).then_some(value)
    }

    /// Non-cached input tokens, the portion billed at the full input rate.
    #[cfg(test)]
    pub(crate) fn uncached_input(self) -> u64 {
        self.input.saturating_sub(self.cached_input)
    }

    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            coverage: match (self.coverage, other.coverage) {
                (Some(mut left), Some(right)) => {
                    left.add_assign(&right);
                    Some(left)
                }
                _ => None,
            },
            input: self.input.checked_add(other.input)?,
            cached_input: self.cached_input.checked_add(other.cached_input)?,
            output: self.output.checked_add(other.output)?,
            reasoning_output: self.reasoning_output.checked_add(other.reasoning_output)?,
            total: self.total.checked_add(other.total)?,
        })
    }

    fn classes(self) -> [(&'static str, Option<u64>); 5] {
        let non_cached_input = (|| {
            let usage = codex_protocol::protocol::TokenUsage {
                input_tokens: i64::try_from(self.input).ok()?,
                cached_input_tokens: i64::try_from(self.cached_input).ok()?,
                coverage: self.coverage,
                ..Default::default()
            };
            u64::try_from(usage.known_non_cached_input()?).ok()
        })();
        [
            ("input", self.known(self.input, |c| c.input_tokens)),
            (
                "cached_input",
                self.known(self.cached_input, |c| c.cached_input_tokens),
            ),
            (
                "uncached_input",
                non_cached_input,
            ),
            ("output", self.known(self.output, |c| c.output_tokens)),
            (
                "reasoning_output",
                self.known(self.reasoning_output, |c| c.reasoning_output_tokens),
            ),
        ]
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct UsageTotals {
    pub tokens: TokenClassTotals,
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
    pub rejected: Option<u32>,
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
    /// Time with no active assignments anywhere in this coordinator, within this task's lifetime.
    pub coordinator_zero_active_time: Duration,
    pub role_usage: Vec<RoleUsage>,
    #[cfg(test)]
    pub concurrency: Vec<ConcurrencySlice>,
    pub acceptance_total: u32,
    pub acceptance_first_pass_closed: Option<u32>,
    pub acceptance_final_closed: u32,
    pub duplicate_work: Option<u32>,
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
    /// Coordinator occupancy only; this does not measure this task's required dependency path.
    pub coordinator_zero_active_time: Duration,
    pub coordinator_zero_active_basis_points: u16,
    pub total_usage: UsageTotals,
    pub usage_by_role: BTreeMap<RoleCapability, UsageTotals>,
    pub concurrency_utilization_basis_points: u16,
    pub acceptance_total: u32,
    pub acceptance_first_pass_closed: Option<u32>,
    pub acceptance_final_closed: u32,
    pub first_pass_acceptance_basis_points: Option<u16>,
    pub final_acceptance_basis_points: u16,
    pub duplicate_work: Option<u32>,
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
    pub acceptance_total: u32,
    pub acceptance_first_pass_closed: Option<u32>,
    pub acceptance_final_closed: u32,
    pub duplicate_work: Option<u32>,
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
    RecorderFinished,
    InvalidEventTime,
    InvalidCoordinatorZeroActiveTime,
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
        if input.coordinator_zero_active_time > input.duration {
            return Err(MetricsError::InvalidCoordinatorZeroActiveTime);
        }
        if input
            .acceptance_first_pass_closed
            .is_some_and(|closed| closed > input.acceptance_final_closed)
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
            coordinator_zero_active_time: input.coordinator_zero_active_time,
            coordinator_zero_active_basis_points: ratio(
                input.coordinator_zero_active_time.as_nanos(),
                input.duration.as_nanos(),
            ),
            total_usage,
            usage_by_role,
            concurrency_utilization_basis_points: ratio(
                active_time.as_nanos(),
                capacity_time.as_nanos(),
            ),
            acceptance_total: input.acceptance_total,
            acceptance_first_pass_closed: input.acceptance_first_pass_closed,
            acceptance_final_closed: input.acceptance_final_closed,
            first_pass_acceptance_basis_points: input
                .acceptance_first_pass_closed
                .map(|closed| ratio(u128::from(closed), u128::from(input.acceptance_total))),
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
            TASK_COORDINATOR_ZERO_ACTIVE_DURATION_METRIC,
            self.coordinator_zero_active_time,
            &outcome_tags,
        );
        session_telemetry.histogram(
            TASK_COORDINATOR_ZERO_ACTIVE_RATIO_METRIC,
            i64::from(self.coordinator_zero_active_basis_points),
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
            // Report the provider-reported total and each class separately. A single
            // collapsed scalar cannot distinguish a child that re-read the repository
            // from one that reused a cached prompt prefix.
            if let Some(total) = usage.tokens.known(usage.tokens.total, |c| c.total_tokens) {
                session_telemetry.histogram(
                    TASK_TOKEN_USAGE_METRIC,
                    saturating_metric_value(total),
                    &tags,
                );
            }
            for (token_class, value) in usage.tokens.classes() {
                let Some(value) = value else { continue };
                session_telemetry.histogram(
                    TASK_TOKEN_USAGE_METRIC,
                    saturating_metric_value(value),
                    &[
                        (ROLE_TAG, role_capability.role.as_str()),
                        (CAPABILITY_TAG, role_capability.capability.as_str()),
                        (TOKEN_CLASS_TAG, token_class),
                    ],
                );
            }
            session_telemetry.histogram(
                TASK_INFERENCE_CALLS_METRIC,
                saturating_metric_value(usage.calls),
                &tags,
            );
        }

        if let Some(closure) = self.first_pass_acceptance_basis_points {
            session_telemetry.histogram(
                TASK_ACCEPTANCE_CLOSURE_METRIC,
                i64::from(closure),
                &[(PHASE_TAG, "first_pass")],
            );
        }
        session_telemetry.histogram(
            TASK_ACCEPTANCE_CLOSURE_METRIC,
            i64::from(self.final_acceptance_basis_points),
            &[(PHASE_TAG, "final")],
        );
        if let Some(duplicate_work) = self.duplicate_work {
            session_telemetry.histogram(TASK_DUPLICATE_WORK_METRIC, i64::from(duplicate_work), &[]);
        }
        session_telemetry.histogram(TASK_CONFLICT_METRIC, i64::from(self.conflicts), &[]);
        session_telemetry.histogram(TASK_DRIFT_METRIC, i64::from(self.drift), &[]);
        for (disposition, count) in [
            ("confirmed", Some(self.reviewer_findings.confirmed)),
            ("rejected", self.reviewer_findings.rejected),
            ("unresolved", Some(self.reviewer_findings.unresolved)),
        ] {
            let Some(count) = count else {
                continue;
            };
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
/// from task start. Zero-active time uses coordinator-wide assignment occupancy, not task
/// dependencies or progress: unrelated active assignments can hide a stalled required task.
#[derive(Debug)]
pub(crate) struct TaskMetricRecorder {
    task_id: OpaqueId,
    last_elapsed: Duration,
    active_turns: u32,
    capacity: u32,
    weighted_active_time: Duration,
    weighted_capacity_time: Duration,
    coordinator_zero_active_time: Duration,
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
            coordinator_zero_active_time: Duration::ZERO,
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
        tokens: TokenClassTotals,
        calls: u64,
    ) -> Result<(), MetricsError> {
        self.ensure_recordable()?;
        let key = RoleCapability { role, capability };
        if !self.usage_by_role.contains_key(&key) && self.usage_by_role.len() >= MAX_METRIC_ROWS {
            return Err(MetricsError::TooManyRows);
        }
        let mut next = self.usage_by_role.get(&key).copied().unwrap_or_default();
        add_usage(&mut next, tokens, calls)?;
        self.recorded_events = self
            .recorded_events
            .saturating_add(1)
            .min(MAX_RECORDED_EVENTS);
        self.usage_by_role.insert(key, next);
        Ok(())
    }

    pub(crate) fn record_store_role_usage(
        &mut self,
        role: AgentRole,
        capability: CapabilityProfile,
        tokens: TokenClassTotals,
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
        let (weighted_active_time, weighted_capacity_time, coordinator_zero_active_time) =
            self.integrated_until(elapsed)?;
        self.recorded_events = self
            .recorded_events
            .saturating_add(1)
            .min(MAX_RECORDED_EVENTS);
        self.last_elapsed = elapsed;
        self.active_turns = active_turns;
        self.capacity = capacity;
        self.weighted_active_time = weighted_active_time;
        self.weighted_capacity_time = weighted_capacity_time;
        self.coordinator_zero_active_time = coordinator_zero_active_time;
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
        let (weighted_active_time, weighted_capacity_time, coordinator_zero_active_time) =
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
                coordinator_zero_active_time,
                role_usage,
                #[cfg(test)]
                concurrency: Vec::new(),
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
        self.recorded_events = self
            .recorded_events
            .saturating_add(1)
            .min(MAX_RECORDED_EVENTS);
        self.last_elapsed = elapsed;
        self.weighted_active_time = weighted_active_time;
        self.weighted_capacity_time = weighted_capacity_time;
        self.coordinator_zero_active_time = coordinator_zero_active_time;
        self.finished = true;
        Ok(Some(metrics))
    }

    #[cfg(test)]
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
        let coordinator_zero_active_time = if self.active_turns == 0 {
            self.coordinator_zero_active_time
                .checked_add(interval)
                .ok_or(MetricsError::ArithmeticOverflow)?
        } else {
            self.coordinator_zero_active_time
        };
        Ok((
            weighted_active_time,
            weighted_capacity_time,
            coordinator_zero_active_time,
        ))
    }
}

/// Process-local measurements for one durable typed task.
///
/// Persistence remains authoritative for task state. This runtime owns only bounded counters and
/// monotonic timestamps, so it can be dropped safely on restart without affecting task recovery.
#[derive(Debug)]
pub(crate) struct TaskMetricRuntime {
    pub(super) minimum_attempt_ordinal: u8,
    pub(super) usage_history_partial: bool,
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
            minimum_attempt_ordinal: 0,
            usage_history_partial: false,
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

    pub(crate) fn record_usage(
        &mut self,
        tokens: TokenClassTotals,
        calls: u64,
    ) -> Result<(), MetricsError> {
        self.recorder
            .record_store_role_usage(self.role, self.capability, tokens, calls)
    }

    pub(super) fn finish(
        &mut self,
        task: &AgentTask,
    ) -> Result<Option<TerminalTaskMetrics>, MetricsError> {
        self.recorder
            .finish(self.started_at.elapsed(), terminal_input(task))
            .map(|metrics| {
                metrics.map(|metrics| TerminalTaskMetrics {
                    metrics,
                    state: task.current_attempt.state,
                    usage_history_partial: self.usage_history_partial,
                })
            })
    }
}

/// Constructed only by successful recorder finalization and consumed once outside the index lock.
pub(super) struct TerminalTaskMetrics {
    metrics: TaskMetrics,
    state: AttemptState,
    usage_history_partial: bool,
}

impl TerminalTaskMetrics {
    pub(super) fn emit(self, session_telemetry: &SessionTelemetry) {
        self.metrics.emit(session_telemetry);
        session_telemetry.counter(
            TASK_TERMINAL_STATE_METRIC,
            1,
            &[
                ("state", attempt_state_metric_label(self.state)),
                (
                    "usage_scope",
                    if self.usage_history_partial {
                        "partial_process_history"
                    } else {
                        "observed_attempt_deltas"
                    },
                ),
            ],
        );
    }
}

const fn attempt_state_metric_label(state: AttemptState) -> &'static str {
    match state {
        AttemptState::Active => "active",
        AttemptState::Completed => "completed",
        AttemptState::NeedsMain => "needs_main",
        AttemptState::Violated => "violated",
        AttemptState::Abandoned => "abandoned",
    }
}

/// Emits aggregate parent-plus-child usage for one completed delegation subtree.
///
/// Root and descendant scopes are reported separately, plus their combined total, so a
/// delegation that merely moves tokens into children is distinguishable from one that
/// reduces overall work. `complete` is false when a subtree member's usage could not be
/// read, which makes the combined figure a lower bound.
pub(crate) fn emit_subtree_usage(
    session_telemetry: &SessionTelemetry,
    usage: &crate::agent::control::SubtreeUsage,
) {
    let combined = usage.combined();
    for (scope, totals) in [
        ("root", usage.root),
        ("descendants", usage.descendants),
        ("combined", combined),
    ] {
        for (token_class, value) in totals.known_classes() {
            // Numeric compatibility sums do not certify absent components.
            let Some(value) = value else { continue };
            session_telemetry.histogram(
                SUBTREE_TOKEN_USAGE_METRIC,
                saturating_metric_value(value),
                &[
                    (SUBTREE_SCOPE_TAG, scope),
                    (TOKEN_CLASS_TAG, token_class),
                    ("complete", bool_tag(usage.is_complete())),
                ],
            );
        }
    }
    session_telemetry.histogram(
        SUBTREE_AGENT_COUNT_METRIC,
        i64::from(usage.descendant_threads),
        &[("complete", bool_tag(usage.is_complete()))],
    );
}

pub(crate) fn terminal_metrics_ready(task: &AgentTask) -> bool {
    if !task.current_attempt.state.is_terminal()
        || (task.current_attempt.state == codex_agent_task_store::AttemptState::Completed
            && task.gates.iter().any(|gate| !gate.status.is_sealed()))
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
    let acceptance_total = saturating_u32(
        task.assignment
            .effective_criteria(task.current_attempt.amendment.as_ref())
            .len(),
    );
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
    // Earlier attempt criterion results are not present in this snapshot.
    let acceptance_first_pass_closed =
        (task.current_attempt.ordinal == 0).then_some(acceptance_final_closed);

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
        acceptance_total,
        acceptance_first_pass_closed,
        acceptance_final_closed,
        // Concurrently active assignments whose read scope overlapped this one at
        // admission: the measurable form of duplicated repository discovery.
        duplicate_work: Some(task.assignment.benign_read_overlap_count),
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

fn add_usage(
    totals: &mut UsageTotals,
    tokens: TokenClassTotals,
    calls: u64,
) -> Result<(), MetricsError> {
    if totals.calls == 0 && totals.tokens == TokenClassTotals::default() {
        totals.tokens.coverage = Some(Default::default());
    }
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
#[path = "task_metrics_tests.rs"]
mod tests;
