use super::*;

fn plan(key: Option<std::path::PathBuf>, native_read: bool) -> WorkspaceAdmissionPlan {
    WorkspaceAdmissionPlan {
        native_additions: None,
        native_read_cohort: native_read,
        bypass_outer_gate: false,
        resource_key: key,
        shared_resource: native_read,
        supports_parallel: true,
        workspace_capable: true,
    }
}

#[derive(Default)]
struct Fixture {
    coordinator: WorkspaceExecutionCoordinator,
    timing: Arc<TurnTimingState>,
}

impl Fixture {
    async fn admit(&self, plan: WorkspaceAdmissionPlan) -> WorkspaceGateGuard {
        acquire_workspace_gate_with_additions(
            Arc::clone(&self.coordinator.parallel_execution),
            Arc::clone(&self.coordinator.workspace_execution),
            plan,
            &self.timing,
            None,
            &self.coordinator.global_gate_holders,
            &mut None,
        )
        .await
    }
}

#[tokio::test]
async fn rootless_reads_share_one_exclusive_barrier_and_cannot_bypass_known_writer() {
    let fixture = Fixture::default();
    let global = &fixture.coordinator.parallel_execution;
    // Independent work-count oracle: four readers reuse ONE barrier handle,
    // not four successive exclusive acquisitions. No timing threshold needed.
    let mut reads = Vec::new();
    for _ in 0..4 {
        reads.push(fixture.admit(plan(None, true)).await);
    }
    for read in &reads[1..] {
        let (
            WorkspaceGateGuard::ReadCohort { _guard: first },
            WorkspaceGateGuard::ReadCohort { _guard: next },
        ) = (&reads[0], read)
        else {
            panic!("typed rootless reader must join cohort")
        };
        assert!(Arc::ptr_eq(first, next));
    }
    assert!(
        Arc::clone(global).try_read_owned().is_err(),
        "even a writer in another Git root must be excluded"
    );
    let mut writer = Box::pin(fixture.admit(plan(Some("repository-B".into()), false)));
    assert!(futures::poll!(&mut writer).is_pending());
    let mut younger = Box::pin(fixture.admit(plan(None, true)));
    assert!(
        futures::poll!(&mut younger).is_pending(),
        "pending writer closes cohort"
    );
    reads.clear();
    let writer = writer.await;
    assert!(
        futures::poll!(&mut younger).is_pending(),
        "absolute read into B cannot race B writer"
    );
    drop(writer);
    drop(younger.await);
    assert!(Arc::clone(global).try_write_owned().is_ok());
}

#[tokio::test]
async fn rootless_cohorts_are_finite_and_cancelled_waiters_release_admission() {
    let global = Arc::new(RwLock::new(()));
    let holders = WorkspaceGlobalHolders::default();
    let timing = Arc::new(TurnTimingState::default());
    let mut reads = Vec::new();
    for _ in 0..16 {
        reads.push(acquire_read_cohort(Arc::clone(&global), &timing, &holders).await);
    }
    let mut seventeenth = Box::pin(acquire_read_cohort(Arc::clone(&global), &timing, &holders));
    assert!(futures::poll!(&mut seventeenth).is_pending());
    drop(seventeenth);
    reads.clear();
    assert!(
        Arc::clone(&global).try_write_owned().is_ok(),
        "cancelled waiter owns nothing"
    );
    let held = acquire_read_cohort(Arc::clone(&global), &timing, &holders).await;
    close_read_cohort(&holders);
    let mut rewritten_or_unknown = Box::pin(Arc::clone(&global).write_owned());
    assert!(futures::poll!(&mut rewritten_or_unknown).is_pending());
    drop(held);
    drop(rewritten_or_unknown.await);
    assert_eq!(timing.lifecycle_context().parallel_gate_waiter_count, 0);
}
