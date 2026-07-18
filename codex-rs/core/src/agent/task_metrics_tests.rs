use super::*;
use codex_agent_task_store::AgentRole;
use codex_agent_task_store::AgentStatusClaim;
use codex_agent_task_store::CapabilityProfile;

fn metric_input() -> TaskMetricInput {
    TaskMetricInput {
        task_id: OpaqueId::new(7),
        duration: Duration::from_secs(20),
        critical_path_idle_time: Duration::from_secs(5),
        role_usage: vec![
            RoleUsage {
                role: RoleLabel::Worker,
                capability: CapabilityLabel::ScopedWrite,
                tokens: 800,
                calls: 4,
            },
            RoleUsage {
                role: RoleLabel::Worker,
                capability: CapabilityLabel::ScopedWrite,
                tokens: 200,
                calls: 1,
            },
        ],
        concurrency: vec![
            ConcurrencySlice {
                duration: Duration::from_secs(10),
                active_turns: 2,
                capacity: 5,
            },
            ConcurrencySlice {
                duration: Duration::from_secs(10),
                active_turns: 4,
                capacity: 5,
            },
        ],
        first_pass_validation_succeeded: false,
        acceptance_total: 4,
        acceptance_first_pass_closed: 3,
        acceptance_final_closed: 4,
        duplicate_work: 2,
        conflicts: 1,
        drift: 3,
        reviewer_findings: FindingTotals {
            confirmed: 2,
            rejected: 1,
            unresolved: 1,
        },
        corrections: 1,
        waivers: 2,
        violations: 1,
        final_outcome: FinalOutcome::NeedsMain,
    }
}

fn terminal_input() -> TaskMetricTerminalInput {
    let input = metric_input();
    TaskMetricTerminalInput {
        first_pass_validation_succeeded: input.first_pass_validation_succeeded,
        acceptance_total: input.acceptance_total,
        acceptance_first_pass_closed: input.acceptance_first_pass_closed,
        acceptance_final_closed: input.acceptance_final_closed,
        duplicate_work: input.duplicate_work,
        conflicts: input.conflicts,
        drift: input.drift,
        reviewer_findings: input.reviewer_findings,
        corrections: input.corrections,
        waivers: input.waivers,
        violations: input.violations,
        final_outcome: input.final_outcome,
    }
}

#[test]
fn evaluates_complete_privacy_safe_metric_set() {
    let metrics = TaskMetrics::evaluate(metric_input()).expect("valid metrics");
    assert_eq!(metrics.critical_path_idle_basis_points, 2_500);
    assert_eq!(metrics.concurrency_utilization_basis_points, 6_000);
    assert_eq!(metrics.first_pass_acceptance_basis_points, 7_500);
    assert_eq!(metrics.final_acceptance_basis_points, 10_000);
    assert_eq!(
        metrics.total_usage,
        UsageTotals {
            tokens: 1_000,
            calls: 5,
        }
    );
    assert_eq!(
        metrics.usage_by_role[&RoleCapability {
            role: RoleLabel::Worker,
            capability: CapabilityLabel::ScopedWrite,
        }],
        UsageTotals {
            tokens: 1_000,
            calls: 5,
        }
    );
    assert_eq!(
        (
            metrics.duplicate_work,
            metrics.conflicts,
            metrics.drift,
            metrics.corrections,
            metrics.waivers,
            metrics.violations,
        ),
        (2, 1, 3, 1, 2, 1)
    );
    assert_eq!(metrics.reviewer_findings, metric_input().reviewer_findings);
    assert_eq!(metrics.final_outcome, FinalOutcome::NeedsMain);
}

#[test]
fn rejects_inconsistent_or_unbounded_inputs() {
    let invalid_idle = TaskMetricInput {
        critical_path_idle_time: Duration::from_secs(21),
        ..metric_input()
    };
    assert_eq!(
        TaskMetrics::evaluate(invalid_idle),
        Err(MetricsError::InvalidCriticalPathIdleTime)
    );

    let invalid_closure = TaskMetricInput {
        acceptance_first_pass_closed: 4,
        acceptance_final_closed: 3,
        ..metric_input()
    };
    assert_eq!(
        TaskMetrics::evaluate(invalid_closure),
        Err(MetricsError::InvalidAcceptanceClosure)
    );

    let too_many_usage_rows = TaskMetricInput {
        role_usage: vec![
            RoleUsage {
                role: RoleLabel::Worker,
                capability: CapabilityLabel::ScopedWrite,
                tokens: 0,
                calls: 0,
            };
            MAX_METRIC_ROWS + 1
        ],
        ..metric_input()
    };
    assert_eq!(
        TaskMetrics::evaluate(too_many_usage_rows),
        Err(MetricsError::TooManyRows)
    );

    let too_many_concurrency_rows = TaskMetricInput {
        concurrency: vec![
            ConcurrencySlice {
                duration: Duration::ZERO,
                active_turns: 0,
                capacity: 5,
            };
            MAX_METRIC_ROWS + 1
        ],
        ..metric_input()
    };
    assert_eq!(
        TaskMetrics::evaluate(too_many_concurrency_rows),
        Err(MetricsError::TooManyRows)
    );
}

#[test]
fn recorder_aggregates_usage_and_weighted_concurrency_online() {
    let mut recorder =
        TaskMetricRecorder::new(OpaqueId::new(9), 0, 5).expect("valid initial state");
    recorder
        .record_store_role_usage(
            AgentRole::Worker,
            CapabilityProfile::ScopedSourceWrite,
            800,
            4,
        )
        .expect("first usage sample");
    recorder
        .record_store_role_usage(
            AgentRole::Worker,
            CapabilityProfile::ScopedSourceWrite,
            200,
            1,
        )
        .expect("second usage sample");
    recorder
        .transition_concurrency(Duration::from_secs(5), 2, 5)
        .expect("first concurrency transition");
    recorder
        .transition_concurrency(Duration::from_secs(15), 4, 5)
        .expect("second concurrency transition");

    let metrics = recorder
        .finish(Duration::from_secs(20), terminal_input())
        .expect("valid terminal sample")
        .expect("first terminal sample is retained");

    assert_eq!(metrics.task_id, OpaqueId::new(9));
    assert_eq!(metrics.critical_path_idle_time, Duration::from_secs(5));
    assert_eq!(metrics.critical_path_idle_basis_points, 2_500);
    assert_eq!(metrics.concurrency_utilization_basis_points, 4_000);
    assert_eq!(
        metrics.usage_by_role[&RoleCapability {
            role: RoleLabel::Worker,
            capability: CapabilityLabel::ScopedWrite,
        }],
        UsageTotals {
            tokens: 1_000,
            calls: 5,
        }
    );
    assert_eq!(metrics.total_usage.tokens, 1_000);
    assert_eq!(recorder.recorded_events(), 5);
    assert!(recorder.is_terminal());
}

#[test]
fn recorder_finishes_and_invokes_terminal_emitter_at_most_once() {
    let mut recorder =
        TaskMetricRecorder::new(OpaqueId::new(10), 1, 5).expect("valid initial state");
    let mut emissions = 0;
    assert!(
        recorder
            .finish_with(Duration::from_secs(1), terminal_input(), |_| emissions += 1)
            .expect("first terminal sample")
            .is_some()
    );
    let terminal_event_count = recorder.recorded_events();
    assert_eq!(
        recorder
            .finish_with(Duration::from_secs(2), terminal_input(), |_| emissions += 1)
            .expect("duplicate terminal samples are ignored"),
        None
    );
    assert_eq!(emissions, 1);
    assert_eq!(recorder.recorded_events(), terminal_event_count);
    assert_eq!(
        recorder.record_role_usage(RoleLabel::Worker, CapabilityLabel::ScopedWrite, 1, 1),
        Err(MetricsError::RecorderFinished)
    );
    assert_eq!(
        recorder.transition_concurrency(Duration::from_secs(2), 0, 5),
        Err(MetricsError::RecorderFinished)
    );
}

#[test]
fn recorder_rejects_invalid_transitions_and_enforces_event_bound() {
    assert_eq!(
        TaskMetricRecorder::new(OpaqueId::new(11), 6, 5).expect_err("invalid initial state"),
        MetricsError::InvalidConcurrencySlice
    );

    let mut non_monotonic =
        TaskMetricRecorder::new(OpaqueId::new(12), 1, 5).expect("valid initial state");
    non_monotonic
        .transition_concurrency(Duration::from_secs(5), 2, 5)
        .expect("valid transition");
    assert_eq!(
        non_monotonic.transition_concurrency(Duration::from_secs(4), 2, 5),
        Err(MetricsError::InvalidEventTime)
    );
    assert_eq!(non_monotonic.recorded_events(), 1);
    assert_eq!(
        non_monotonic.transition_concurrency(Duration::from_secs(6), 0, 0),
        Err(MetricsError::InvalidConcurrencySlice)
    );
    assert_eq!(
        non_monotonic.transition_concurrency(Duration::from_secs(6), 6, 5),
        Err(MetricsError::InvalidConcurrencySlice)
    );
    assert_eq!(non_monotonic.recorded_events(), 1);

    let mut bounded =
        TaskMetricRecorder::new(OpaqueId::new(13), 1, 5).expect("valid initial state");
    for _ in 0..MAX_RECORDED_EVENTS - 1 {
        bounded
            .record_role_usage(RoleLabel::Worker, CapabilityLabel::ScopedWrite, 0, 0)
            .expect("event within the bound");
    }
    assert_eq!(bounded.recorded_events(), MAX_RECORDED_EVENTS - 1);
    assert_eq!(
        bounded.record_role_usage(RoleLabel::Worker, CapabilityLabel::ScopedWrite, 0, 0),
        Err(MetricsError::TooManyEvents)
    );
    assert!(
        bounded
            .finish(Duration::from_secs(1), terminal_input())
            .expect("the bound reserves room for a terminal event")
            .is_some()
    );
    assert_eq!(bounded.recorded_events(), MAX_RECORDED_EVENTS);
}

#[test]
fn recorder_checks_usage_overflow_before_committing_an_event() {
    let mut recorder =
        TaskMetricRecorder::new(OpaqueId::new(14), 1, 5).expect("valid initial state");
    recorder
        .record_role_usage(
            RoleLabel::Worker,
            CapabilityLabel::ScopedWrite,
            u64::MAX,
            u64::MAX,
        )
        .expect("maximum initial totals");
    assert_eq!(
        recorder.record_role_usage(RoleLabel::Worker, CapabilityLabel::ScopedWrite, 1, 0),
        Err(MetricsError::ArithmeticOverflow)
    );
    assert_eq!(recorder.recorded_events(), 1);
}

#[test]
fn store_enums_convert_to_closed_metric_labels() {
    for (role, expected) in [
        (AgentRole::Explorer, RoleLabel::Explorer),
        (AgentRole::Worker, RoleLabel::Worker),
        (AgentRole::Reviewer, RoleLabel::Reviewer),
        (AgentRole::Verifier, RoleLabel::Verifier),
        (AgentRole::Integrator, RoleLabel::Integrator),
    ] {
        assert_eq!(RoleLabel::from(role), expected);
    }
    for (capability, expected) in [
        (CapabilityProfile::ReadSearch, CapabilityLabel::ReadSearch),
        (
            CapabilityProfile::ReadSearchDiff,
            CapabilityLabel::ReadSearchDiff,
        ),
        (
            CapabilityProfile::ReadSearchShell,
            CapabilityLabel::ReadSearchShell,
        ),
        (
            CapabilityProfile::ScopedSourceWrite,
            CapabilityLabel::ScopedWrite,
        ),
        (
            CapabilityProfile::IntegratorSourceWrite,
            CapabilityLabel::CrossOwnerWrite,
        ),
    ] {
        assert_eq!(CapabilityLabel::from(capability), expected);
    }
    for (status, expected) in [
        (AgentStatusClaim::Completed, FinalOutcome::Completed),
        (AgentStatusClaim::NeedsMain, FinalOutcome::NeedsMain),
        (AgentStatusClaim::Blocked, FinalOutcome::Blocked),
        (AgentStatusClaim::Failed, FinalOutcome::Failed),
        (AgentStatusClaim::Violated, FinalOutcome::Violated),
        (AgentStatusClaim::Abandoned, FinalOutcome::Abandoned),
    ] {
        assert_eq!(FinalOutcome::from(status), expected);
    }
}

#[test]
fn emitter_vocabulary_is_static_and_low_cardinality() {
    assert_eq!(
        [
            TASK_DURATION_METRIC,
            TASK_CRITICAL_PATH_IDLE_DURATION_METRIC,
            TASK_CRITICAL_PATH_IDLE_RATIO_METRIC,
            TASK_TOKEN_USAGE_METRIC,
            TASK_INFERENCE_CALLS_METRIC,
            TASK_CONCURRENCY_UTILIZATION_METRIC,
            TASK_FIRST_PASS_VALIDATION_METRIC,
            TASK_ACCEPTANCE_CLOSURE_METRIC,
            TASK_DUPLICATE_WORK_METRIC,
            TASK_CONFLICT_METRIC,
            TASK_DRIFT_METRIC,
            TASK_REVIEWER_FINDING_METRIC,
            TASK_CORRECTION_METRIC,
            TASK_WAIVER_METRIC,
            TASK_VIOLATION_METRIC,
            TASK_OUTCOME_METRIC,
        ],
        [
            "codex.multi_agent.task.duration_ms",
            "codex.multi_agent.task.critical_path_idle.duration_ms",
            "codex.multi_agent.task.critical_path_idle.basis_points",
            "codex.multi_agent.task.token_usage",
            "codex.multi_agent.task.inference_calls",
            "codex.multi_agent.task.concurrency_utilization.basis_points",
            "codex.multi_agent.task.first_pass_validation",
            "codex.multi_agent.task.acceptance_closure.basis_points",
            "codex.multi_agent.task.duplicate_work",
            "codex.multi_agent.task.conflict",
            "codex.multi_agent.task.drift",
            "codex.multi_agent.task.reviewer_finding",
            "codex.multi_agent.task.correction",
            "codex.multi_agent.task.waiver",
            "codex.multi_agent.task.violation",
            "codex.multi_agent.task.outcome",
        ]
    );
    assert_eq!(
        [
            ROLE_TAG,
            CAPABILITY_TAG,
            OUTCOME_TAG,
            SUCCEEDED_TAG,
            PHASE_TAG,
            DISPOSITION_TAG,
        ],
        [
            "role",
            "capability",
            "outcome",
            "succeeded",
            "phase",
            "disposition",
        ]
    );
    assert_eq!(
        [
            RoleLabel::Root.as_str(),
            RoleLabel::Explorer.as_str(),
            RoleLabel::Worker.as_str(),
            RoleLabel::Reviewer.as_str(),
            RoleLabel::Verifier.as_str(),
            RoleLabel::Integrator.as_str(),
            RoleLabel::Legacy.as_str(),
        ],
        [
            "root",
            "explorer",
            "worker",
            "reviewer",
            "verifier",
            "integrator",
            "legacy",
        ]
    );
    assert_eq!(
        [
            CapabilityLabel::ReadSearch.as_str(),
            CapabilityLabel::ReadSearchDiff.as_str(),
            CapabilityLabel::ReadSearchShell.as_str(),
            CapabilityLabel::ScopedWrite.as_str(),
            CapabilityLabel::CrossOwnerWrite.as_str(),
            CapabilityLabel::Legacy.as_str(),
        ],
        [
            "read_search",
            "read_search_diff",
            "read_search_shell",
            "scoped_write",
            "cross_owner_write",
            "legacy",
        ]
    );
    assert_eq!(
        [
            FinalOutcome::Completed.as_str(),
            FinalOutcome::NeedsMain.as_str(),
            FinalOutcome::Blocked.as_str(),
            FinalOutcome::Failed.as_str(),
            FinalOutcome::Violated.as_str(),
            FinalOutcome::Abandoned.as_str(),
        ],
        [
            "completed",
            "needs_main",
            "blocked",
            "failed",
            "violated",
            "abandoned",
        ]
    );
    assert_eq!(bool_tag(true), "true");
    assert_eq!(bool_tag(false), "false");
    assert_eq!(saturating_metric_value(i64::MAX as u64), i64::MAX);
    assert_eq!(
        saturating_metric_value((i64::MAX as u64).saturating_add(1)),
        i64::MAX
    );
    assert_eq!(saturating_metric_value(u64::MAX), i64::MAX);
}

#[test]
fn deterministic_replay_scenarios_cover_required_rules() {
    let scenarios = replay_scenarios();
    assert_eq!(scenarios.len(), 10);
    for scenario in &scenarios {
        assert_eq!(evaluate_replay(scenario.input), scenario.expected);
    }
    let outcomes: Vec<_> = scenarios
        .iter()
        .map(|scenario| {
            (
                scenario.kind,
                scenario.expected.gate,
                scenario.expected.outcome,
            )
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![
            (
                ReplayScenarioKind::BoundedFix,
                GateDisposition::DirectCompletion,
                FinalOutcome::Completed
            ),
            (
                ReplayScenarioKind::CrossOwner,
                GateDisposition::ReviewThenVerification,
                FinalOutcome::Completed
            ),
            (
                ReplayScenarioKind::ColdReviewDetection,
                GateDisposition::CorrectionThenVerification,
                FinalOutcome::Completed
            ),
            (
                ReplayScenarioKind::UnauthorizedPatchAndShell,
                GateDisposition::NeedsMain,
                FinalOutcome::Violated
            ),
            (
                ReplayScenarioKind::DirtyFileDiff,
                GateDisposition::DirectCompletion,
                FinalOutcome::Completed
            ),
            (
                ReplayScenarioKind::ConcurrentDrift,
                GateDisposition::NeedsMain,
                FinalOutcome::NeedsMain
            ),
            (
                ReplayScenarioKind::RestartWatermark,
                GateDisposition::DirectCompletion,
                FinalOutcome::Completed
            ),
            (
                ReplayScenarioKind::DependencyRejection,
                GateDisposition::NotEntered,
                FinalOutcome::Blocked
            ),
            (
                ReplayScenarioKind::CorrectionBounds,
                GateDisposition::NeedsMain,
                FinalOutcome::NeedsMain
            ),
            (
                ReplayScenarioKind::LegacyCompatibility,
                GateDisposition::LegacyUnchanged,
                FinalOutcome::Completed
            ),
        ]
    );
}

#[test]
fn unauthorized_shell_models_enforcement_and_detection_only_platforms() {
    let supported = evaluate_replay(ReplayInput {
        unauthorized_shell: true,
        ..ReplayInput::default()
    });
    let detection_only = evaluate_replay(ReplayInput {
        unauthorized_shell: true,
        shell_enforcement_supported: false,
        ..ReplayInput::default()
    });
    assert_eq!(supported.shell, MutationDisposition::Blocked);
    assert_eq!(
        detection_only.shell,
        MutationDisposition::DetectionOnlyViolation
    );
    assert_eq!(detection_only.outcome, FinalOutcome::Violated);
}

#[test]
fn compares_correctness_wall_time_and_token_cost() {
    let comparison = compare_multi_agent_to_single_agent(
        RunCost {
            correct: false,
            wall_time: Duration::from_secs(30),
            tokens: 1_000,
        },
        RunCost {
            correct: true,
            wall_time: Duration::from_secs(20),
            tokens: 1_500,
        },
    );
    assert_eq!(
        comparison,
        RunComparison {
            correctness: Comparison::Better,
            wall_time: Comparison::Better,
            token_cost: Comparison::Worse,
        }
    );
}
