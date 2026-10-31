//! Live prerequisite authority, using the store's existing root revision owner.
use super::*;
use serde::Deserialize;

/// Explicit, attempt-fenced selection; never shorthand for whole-task acceptance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceTarget {
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub criterion_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AcceptanceObservation {
    pub target: AcceptanceTarget,
    pub criteria: Vec<crate::CriterionAssessment>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AcceptanceReadiness {
    pub cursor: String,
    pub satisfied: bool,
    pub observations: Vec<AcceptanceObservation>,
}

/// Reuses the root-state poller, with no task lifetime beyond the caller's wait.
pub struct AcceptanceSubscription {
    store: LocalAgentTaskStore,
    root: String,
    caller: Option<(AssignmentId, AttemptId)>,
    targets: Vec<AcceptanceTarget>,
    recheck_source_freshness: bool,
    waiter: DurableWakeWaiter,
    shutdown: watch::Receiver<bool>,
}

impl LocalAgentTaskStore {
    pub async fn subscribe_acceptance(
        &self,
        root: String,
        caller: Option<(AssignmentId, AttemptId)>,
        mut targets: Vec<AcceptanceTarget>,
    ) -> StoreResult<AcceptanceSubscription> {
        if targets.is_empty() {
            return Err(StoreError::InvalidAssignment("acceptance targets cannot be empty".into()));
        }
        targets.sort_by_key(|target| target.assignment_id.to_string());
        let mut seen = HashSet::new();
        for target in &mut targets {
            if !seen.insert(target.assignment_id) || target.criterion_ids.is_empty() {
                return Err(StoreError::InvalidAssignment("acceptance requires unique targets and nonempty criterion selections".into()));
            }
            target.criterion_ids.sort();
            target.criterion_ids.dedup();
        }
        // Registration precedes every authority read. A commit is either in the
        // read snapshot or remains pending on this receiver, including cross-process writes.
        let waiter = self.durable_wake_poller.register(&root, true);
        Ok(AcceptanceSubscription {
            store: self.clone(), root, caller, targets, waiter, recheck_source_freshness: false,
            shutdown: self.durable_wake_poller.shutdown.subscribe(),
        })
    }
}

impl AcceptanceSubscription {
    pub async fn observe(&mut self) -> StoreResult<AcceptanceReadiness> {
        self.waiter.wake_rx.borrow_and_update();
        if *self.shutdown.borrow() {
            return Err(sqlx::Error::PoolClosed.into());
        }
        let mut transaction = self.store.pool.begin().await?;
        if let Some((assignment_id, attempt_id)) = self.caller {
            let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
            let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
            if assignment.root_session_id != self.root || attempt.attempt_id != attempt_id
                || attempt.state != AttemptState::Active {
                return Err(StoreError::AttemptNotActive(attempt_id));
            }
        }
        let mut observations = Vec::new();
        for target in &self.targets {
            let assignment = load_assignment_tx(&mut transaction, target.assignment_id).await?;
            if assignment.root_session_id != self.root {
                return Err(StoreError::RootSessionMismatch {
                    expected: self.root.clone(), requested: assignment.root_session_id,
                });
            }
            let attempt = load_current_attempt_tx(&mut transaction, target.assignment_id).await?;
            if attempt.attempt_id != target.attempt_id {
                return Err(StoreError::AttemptNotActive(target.attempt_id));
            }
            let criteria = effective_criteria(&assignment, attempt.amendment.as_ref());
            self.recheck_source_freshness |= criteria.iter().any(|criterion|
                target.criterion_ids.contains(&criterion.id)
                    && criterion.evidence_policy.as_ref().is_some_and(|policy|
                        policy.kind == crate::CriterionEvidenceRequirement::SourceInspection
                            && policy.freshness == crate::EvidenceFreshness::Current));
            for id in &target.criterion_ids {
                if !criteria.iter().any(|criterion| criterion.id == *id && criterion.evidence_policy.is_some()) {
                    return Err(StoreError::InvalidAssignment(format!(
                        "acceptance criterion {id:?} must exist and have an explicit evidence policy"
                    )));
                }
            }
            let receipt = load_receipt_tx(&mut transaction, &attempt).await?.map(|(receipt, _)| receipt);
            let assessed = source_inspection::assess_tx(
                &mut transaction, &assignment, &attempt,
                receipt.as_ref().map_or(&[], |receipt| receipt.criterion_results.as_slice()),
                receipt.as_ref().map_or(&[], |receipt| receipt.validation_call_ids.as_slice()),
                receipt.is_none() && attempt.state == AttemptState::Active,
            ).await?;
            let selected = target.criterion_ids.iter().map(|id| {
                assessed.iter().find(|assessment| assessment.criterion_id == *id).cloned()
                    .unwrap_or_else(|| crate::CriterionAssessment {
                        criterion_id: id.clone(),
                        satisfied: receipt.as_ref().is_some_and(|receipt|
                            receipt.criterion_results.iter().any(|result|
                                result.criterion_id == *id && result.status == CriterionStatus::Passed)),
                        reason: "explicit report_only policy; sealed claim, not independent verification".into(),
                    })
            }).collect();
            observations.push(AcceptanceObservation { target: target.clone(), criteria: selected });
        }
        transaction.rollback().await?;
        let cursor = hash_bytes(&serde_json::to_vec(&observations)?);
        Ok(AcceptanceReadiness {
            cursor,
            satisfied: observations.iter().all(|observation| observation.criteria.iter().all(|criterion| criterion.satisfied)),
            observations,
        })
    }

    /// Initial status, then one notification per distinct satisfied selection.
    /// No receipt, gate, sibling operation or task state is changed by a wake.
    pub async fn next(&mut self, after_cursor: Option<&str>) -> StoreResult<AcceptanceReadiness> {
        loop {
            let state = self.observe().await?;
            if after_cursor.is_none() || state.satisfied && after_cursor != Some(state.cursor.as_str()) {
                return Ok(state);
            }
            tokio::select! {
                changed = self.waiter.wake_rx.changed() => {
                    changed.map_err(|_| StoreError::from(sqlx::Error::PoolClosed))?;
                }
                _ = self.shutdown.changed() => return Err(sqlx::Error::PoolClosed.into()),
                // External file restoration need not commit a task-store event.
                _ = tokio::time::sleep(EXTERNAL_WAKE_RECHECK_INTERVAL), if self.recheck_source_freshness => {},
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DependencyReadiness {
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    /// Observation revision, not permission to reuse acceptance after a mutation.
    pub revision: i64,
    /// Semantic identity: unrelated root updates do not redeliver readiness.
    pub cursor: String,
    pub ready: bool,
    pub blockers: Vec<DependencyBlocker>,
}

/// Dropping the subscription releases its poller interest; no detached worker.
pub struct DependencySubscription {
    store: LocalAgentTaskStore,
    assignment_id: AssignmentId,
    attempt_id: AttemptId,
    waiter: DurableWakeWaiter,
    shutdown: watch::Receiver<bool>,
}

impl LocalAgentTaskStore {
    pub async fn subscribe_dependency_readiness(
        &self,
        assignment_id: AssignmentId,
        attempt_id: AttemptId,
    ) -> StoreResult<DependencySubscription> {
        let mut transaction = self.pool.begin().await?;
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        transaction.rollback().await?;
        // Subscribe before observing authority. A commit between these steps is
        // either in that read snapshot or retained by the registered receiver.
        let waiter = self.durable_wake_poller.register(&assignment.root_session_id, true);
        Ok(DependencySubscription {
            store: self.clone(), assignment_id, attempt_id, waiter,
            shutdown: self.durable_wake_poller.shutdown.subscribe(),
        })
    }
}

impl DependencySubscription {
    pub async fn observe(&mut self) -> StoreResult<DependencyReadiness> {
        // Mark the wake BEFORE the read: never acknowledge a commit absent from
        // the observation. The transaction owns all transitive acceptance reads.
        self.waiter.wake_rx.borrow_and_update();
        if *self.shutdown.borrow() {
            return Err(sqlx::Error::PoolClosed.into());
        }
        let mut transaction = self.store.pool.begin().await?;
        let assignment = load_assignment_tx(&mut transaction, self.assignment_id).await?;
        let attempt = load_current_attempt_tx(&mut transaction, self.assignment_id).await?;
        if attempt.attempt_id != self.attempt_id || attempt.state != AttemptState::Active {
            return Err(StoreError::AttemptNotActive(self.attempt_id));
        }
        let revision = sqlx::query_scalar::<_, i64>(
            "SELECT revision FROM root_task_state_revision WHERE root_session_id = ?",
        ).bind(&assignment.root_session_id).fetch_one(&mut *transaction).await?;
        let captured = load_dependency_admissions_tx(&mut transaction, self.assignment_id).await?;
        let blockers = dependency_authority_blockers_tx(&mut transaction, &assignment, &captured).await?;
        transaction.rollback().await?;
        let cursor = hash_bytes(serde_json::to_vec(&(self.assignment_id, self.attempt_id, &blockers))?.as_slice());
        Ok(DependencyReadiness {
            assignment_id: self.assignment_id, attempt_id: self.attempt_id,
            revision, cursor, ready: blockers.is_empty(), blockers,
        })
    }

    /// Initial observation, or the next semantic change after a delivered cursor.
    /// Cancellation is owned by the caller dropping this future/subscription.
    pub async fn next(&mut self, after_cursor: Option<&str>) -> StoreResult<DependencyReadiness> {
        loop {
            let state = self.observe().await?;
            if after_cursor != Some(state.cursor.as_str()) {
                return Ok(state);
            }
            tokio::select! {
                changed = self.waiter.wake_rx.changed() => {
                    changed.map_err(|_| StoreError::from(sqlx::Error::PoolClosed))?;
                }
                _ = self.shutdown.changed() => return Err(sqlx::Error::PoolClosed.into()),
            }
        }
    }
}
