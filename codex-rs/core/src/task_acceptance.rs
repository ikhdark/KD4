//! Finite acceptance of declared obligations, separate from checklist progress.
//! Scope interpretation remains a model judgment: absence of a contract is
//! unknown, never a host assertion that the full user request was fulfilled.
use super::PlanInputReference;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::AttemptId;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptanceObligation {
    pub(crate) text: String,
    pub(crate) origin: PlanInputReference,
    pub(crate) requirement: EvidenceRequirement,
    /// Declared checklist scope, not proof or permission to complete its steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) plan_requirement_ids: Vec<String>,
    /// Pointer only. Its existence, kind and applicability are host-resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) evidence_ref: Option<String>,
    /// Concrete external dependency; interpretation is recorded, not certified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) blocker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) retired_by: Option<PlanInputReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) retirement_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum EvidenceRequirement {
    Inspection {
        environment_id: String,
        path: String,
        start: u64,
        end: u64,
    },
    Validation {
        environment_id: String,
        path: String,
        test_execution: bool,
        /// Exact runner identities, not package attribution or a test count.
        #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
        required_tests: std::collections::BTreeSet<crate::validation::RunnerTestIdentity>,
    },
    Assignment {
        assignment_id: AssignmentId,
        attempt_id: AttemptId,
    },
}

impl EvidenceRequirement {
    pub(crate) fn same_contract(&self, other: &Self) -> bool {
        match (self, other) {
            // A coordinator retry changes the evidence generation, not the
            // parent's obligation. The terminal query still requires the exact
            // current attempt; rebinding cannot promote an old receipt.
            (
                Self::Assignment {
                    assignment_id: a, ..
                },
                Self::Assignment {
                    assignment_id: b, ..
                },
            ) => a == b,
            _ => self == other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum AcceptedEvidence {
    Inspection {
        environment_id: String,
        path: String,
        ranges: Vec<(u64, u64)>,
    },
    Validation {
        environment_id: String,
        paths: std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
        test_execution: bool,
        passed_tests: std::collections::BTreeSet<crate::validation::RunnerTestIdentity>,
    },
    Assignment {
        assignment_id: AssignmentId,
        attempt_id: AttemptId,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct AcceptanceEvidence {
    pub(crate) turn_id: String,
    pub(crate) mutation_revision: u64,
    pub(crate) observations: BTreeMap<String, AcceptedEvidence>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequestState {
    Complete,
    Partial,
    Blocked,
    Unknown,
    Invalidated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactState {
    ScopedEvidenceSatisfied,
    Unverified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalAction {
    Deliver,
    Continue,
    ReportBlocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TaskClosureDecision {
    pub(crate) plan_revision: Option<String>,
    pub(crate) input_revision: Option<String>,
    pub(crate) mutation_revision: u64,
    pub(crate) request_state: RequestState,
    pub(crate) artifact_state: ArtifactState,
    pub(crate) terminal_action: TerminalAction,
    pub(crate) unmet_obligations: Vec<String>,
    pub(crate) evidence_refs: Vec<String>,
    /// A finite contract cannot certify omitted requirements or semantic intent.
    pub(crate) authority: &'static str,
}

impl TaskClosureDecision {
    pub(crate) fn disposition(&self) -> codex_protocol::protocol::TurnTaskDisposition {
        // The public record retains every computed state without warning text.
        let mut value = serde_json::json!(self);
        value["schema_version"] = serde_json::json!(1);
        serde_json::from_value(value).expect("task disposition shares the closure decision contract")
    }
}

impl EvidenceRequirement {
    pub(crate) fn is_satisfied_by(&self, observed: Option<&AcceptedEvidence>) -> bool {
        match (self, observed) {
            (
                EvidenceRequirement::Inspection {
                    environment_id,
                    path,
                    start,
                    end,
                },
                Some(AcceptedEvidence::Inspection {
                    environment_id: owner,
                    path: actual,
                    ranges,
                }),
            ) => {
                environment_id == owner
                    && crate::tool_history::source_locator_key(&normalized_path(path))
                        == crate::tool_history::source_locator_key(actual)
                    && start < end
                    && ranges.iter().any(|(a, b)| a <= start && b >= end)
            }
            (
                EvidenceRequirement::Validation {
                    environment_id,
                    path,
                    test_execution,
                    required_tests,
                },
                Some(AcceptedEvidence::Validation {
                    environment_id: owner,
                    paths,
                    test_execution: executed,
                    passed_tests,
                }),
            ) => {
                environment_id == owner
                    && (!test_execution || *executed)
                    && required_tests.is_subset(passed_tests)
                    && paths.iter().any(|scope| {
                        // Locators keep their case; comparisons use the platform key.
                        let path = normalized_path(path);
                        let path_key = crate::tool_history::source_locator_key(&path);
                        let path: &str = &path_key;
                        let scope_key = crate::tool_history::source_locator_key(&scope.path);
                        let scope_path: &str = &scope_key;
                        path == scope_path
                            || scope.recursive
                                && path.strip_prefix(scope_path).is_some_and(|suffix| {
                                    scope_path.ends_with('/') || suffix.starts_with('/')
                                })
                    })
            }
            (
                EvidenceRequirement::Assignment {
                    assignment_id,
                    attempt_id,
                },
                Some(AcceptedEvidence::Assignment {
                    assignment_id: actual,
                    attempt_id: attempt,
                }),
            ) => assignment_id == actual && attempt_id == attempt,
            _ => false,
        }
    }
}

impl AcceptanceEvidence {
    /// Model view only; the complete owner map remains queryable when criteria change.
    pub(crate) fn for_obligations(&self, obligations: &BTreeMap<String, AcceptanceObligation>) -> Self {
        let mut projected = self.clone();
        projected.observations.retain(|reference, observed| {
            obligations.values().any(|obligation| {
                obligation.retired_by.is_none()
                    && (obligation.evidence_ref.as_ref() == Some(reference)
                        || obligation.requirement.is_satisfied_by(Some(observed)))
            })
        });
        projected
    }
}

pub(crate) fn evaluate(
    plan_revision: Option<String>,
    input_revision: Option<String>,
    reviewed_revision: Option<&str>,
    mutation_revision: u64,
    obligations: &BTreeMap<String, AcceptanceObligation>,
    evidence: &AcceptanceEvidence,
) -> TaskClosureDecision {
    let mut decision = TaskClosureDecision {
        plan_revision,
        input_revision,
        mutation_revision,
        request_state: RequestState::Unknown,
        artifact_state: ArtifactState::Unverified,
        terminal_action: TerminalAction::Deliver,
        unmet_obligations: Vec::new(),
        evidence_refs: Vec::new(),
        authority: "declared_scope_only; intent, omitted consumers and external workspace completeness are not certified",
    };
    // Legacy/no-plan/short reporting tasks are deliverable without a fabricated
    // success, compulsory bookkeeping, edits or test execution.
    if obligations
        .values()
        .all(|obligation| obligation.retired_by.is_some())
    {
        return decision;
    }
    if decision.input_revision.is_none() || reviewed_revision != decision.input_revision.as_deref()
    {
        decision.request_state = RequestState::Invalidated;
        decision.terminal_action = TerminalAction::Continue;
        // Retired scope is history, not open work to reconcile or report.
        decision.unmet_obligations = obligations
            .iter()
            .filter(|(_, obligation)| obligation.retired_by.is_none())
            .map(|(id, _)| id.clone())
            .collect();
        return decision;
    }
    let mut all_blocked = true;
    for (id, obligation) in obligations {
        if obligation.retired_by.is_some() {
            continue;
        }
        let observed = obligation.evidence_ref.as_ref().and_then(|reference| {
            (evidence.mutation_revision == mutation_revision)
                .then(|| evidence.observations.get(reference))
                .flatten()
        });
        let satisfied = obligation.requirement.is_satisfied_by(observed);
        if satisfied {
            if let Some(reference) = &obligation.evidence_ref {
                decision.evidence_refs.push(reference.clone());
            }
        } else {
            decision.unmet_obligations.push(id.clone());
            all_blocked &= obligation
                .blocker
                .as_ref()
                .is_some_and(|reason| !reason.trim().is_empty());
        }
    }
    if decision.unmet_obligations.is_empty() {
        decision.request_state = RequestState::Complete;
        decision.artifact_state = ArtifactState::ScopedEvidenceSatisfied;
    } else if all_blocked {
        decision.request_state = RequestState::Blocked;
        decision.terminal_action = TerminalAction::ReportBlocked;
    } else {
        decision.request_state = RequestState::Partial;
        decision.terminal_action = TerminalAction::Continue;
    }
    decision
}

fn normalized_path(path: &str) -> String {
    crate::tool_history::SourceDependencyV1::new(std::path::Path::new(path), false).path
}

impl super::PlanStore {
    /// Archive only an actually admitted final, never an intermediate Complete
    /// predicate. Persist before publication, with the normal owner lock order.
    /// Returns the checklist to publish when archival changed its presentation.
    pub(crate) async fn archive_completed_scope<F, Fut>(
        &self,
        decision: &TaskClosureDecision,
        persist: F,
    ) -> std::io::Result<Option<codex_protocol::plan_tool::UpdatePlanArgs>>
    where
        F: FnOnce(codex_protocol::models::ResponseItem) -> Fut,
        Fut: std::future::Future<Output = std::io::Result<()>>,
    {
        if decision.request_state != RequestState::Complete { return Ok(None); }
        let _publication = self.publication_guard().await;
        let mut current = self.current.lock().await;
        let input = self.accepted_input.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref().map(super::AcceptedPlanInput::review_revision);
        if current.plan.as_ref().map(|_| current.revision()) != decision.plan_revision.as_deref()
            || input != decision.input_revision
        {
            return Err(std::io::Error::other("task scope changed before archival"));
        }
        let Some(mut plan) = current.plan.clone() else { return Ok(None) };
        let mut next = current.clone();
        let mut changed = false;
        // Mapped stale markers are presentation lag, not unfinished old work.
        // Unmapped rows are never completed by the finite acceptance contract.
        for step in &mut plan.plan {
            let id = next.lineage.step_id(&step.step);
            let requirements = next.lineage.step_requirements.get(&id)
                .cloned().unwrap_or_else(|| vec![id]);
            if requirements.iter().all(|id| next.lineage.acceptance.values().any(|obligation|
                obligation.retired_by.is_none() && obligation.plan_requirement_ids.contains(id))) {
                changed |= step.status != codex_protocol::plan_tool::StepStatus::Completed;
                step.status = codex_protocol::plan_tool::StepStatus::Completed;
                // A renamed row completes its original requirement only through
                // explicit resolution; the satisfied mapping is that accounting.
                for id in requirements {
                    if next.lineage.requirements.contains_key(&id) {
                        next.lineage.resolved_requirements.insert(id);
                    }
                }
            }
        }
        next.lineage.update_statuses(&plan);
        next.plan = Some(plan.clone());
        for (id, obligation) in std::mem::take(&mut next.lineage.acceptance) {
            next.lineage.acceptance_history.entry(id).or_insert(obligation);
        }
        next.lineage.acceptance_reviewed_revision = None;
        next.derived = Default::default();
        next.durably_published = true;
        let published = changed.then(|| plan.clone());
        let response = super::PlanToolResponse {
            obligations: next.lineage.obligation_summary(&plan),
            completion_authority: super::checklist_completion_authority(),
            lineage: next.lineage.compact_for_plan(&plan),
            lineage_complete: true,
            revision: next.revision().to_owned(),
            step_ids: plan.plan.iter().map(|step| next.lineage.step_id(&step.step)).collect(),
            message: "Completed declared task scope archived; no passing proof is carried into a later task".into(),
            effect: "structural_revision".into(),
            no_progress: false,
            current_plan: plan,
        };
        persist(super::plan_snapshot_item(&serde_json::json!(response))).await?;
        *current = next;
        Ok(published)
    }

    pub(crate) fn publish_acceptance_evidence(&self, evidence: AcceptanceEvidence) {
        *self
            .acceptance_evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = evidence;
    }

    /// Advisory over already-owned state, never a child/source/lease query or
    /// permission to finalize. Final admission still reacquires authoritative
    /// evidence through task_closure.
    pub(crate) async fn completion_advisory(
        &self, turn_id: &str, mutation_revision: u64,
    ) -> (TaskClosureDecision, Vec<String>) {
        let current = self.current.lock().await;
        let input = self.accepted_input.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let evidence = self.acceptance_evidence.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let empty = AcceptanceEvidence::default();
        let evidence = if evidence.turn_id == turn_id && evidence.mutation_revision == mutation_revision {
            &*evidence
        } else { &empty };
        let decision = evaluate(
            current.plan.as_ref().map(|_| current.revision().to_owned()),
            input.as_ref().map(super::AcceptedPlanInput::review_revision),
            current.lineage.acceptance_reviewed_revision.as_deref(),
            mutation_revision, &current.lineage.acceptance, evidence,
        );
        let blockers = decision.unmet_obligations.iter().filter_map(|id| {
            current.lineage.acceptance.get(id)?.blocker.as_ref()
                .filter(|reason| !reason.trim().is_empty()).map(|reason| format!("{id}: {reason}"))
        }).collect();
        (decision, blockers)
    }

    pub(crate) async fn task_closure(
        &self,
        turn_id: &str,
        mutation_revision: u64,
        coordinator: &crate::agent::task_coordinator::AgentTaskCoordinator,
    ) -> TaskClosureDecision {
        self.task_closure_with_reader(turn_id, mutation_revision, |assignment_id| {
            coordinator.get_agent_task(assignment_id, Some(0))
        })
        .await
    }

    async fn task_closure_with_reader<F, Fut, E>(
        &self,
        turn_id: &str,
        mutation_revision: u64,
        read_assignment: F,
    ) -> TaskClosureDecision
    where
        F: Fn(AssignmentId) -> Fut,
        Fut: std::future::Future<Output = Result<codex_agent_task_store::AgentTask, E>>,
    {
        let (revision, mut obligations, reviewed, input_revision) = {
            let current = self.current.lock().await;
            let input = self
                .accepted_input
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                current.plan.as_ref().map(|_| current.revision().to_owned()),
                current.lineage.acceptance.clone(),
                current.lineage.acceptance_reviewed_revision.clone(),
                input
                    .as_ref()
                    .map(super::AcceptedPlanInput::review_revision),
            )
        };
        let mut evidence = self
            .acceptance_evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if evidence.turn_id != turn_id || evidence.mutation_revision != mutation_revision {
            evidence = AcceptanceEvidence {
                turn_id: turn_id.into(),
                mutation_revision,
                ..Default::default()
            };
        }
        // Query only explicitly mandatory assignments. Never wait on every
        // spawned agent, rerun a sealed worker, or hold PlanStore across IO.
        // Cached child acceptance must be retracted even when the fresh query
        // fails or a newer attempt/gate no longer satisfies the contract.
        evidence
            .observations
            .retain(|_, observation| !matches!(observation, AcceptedEvidence::Assignment { .. }));
        let assignments = obligations
            .values()
            .filter_map(|obligation| match obligation.requirement {
                EvidenceRequirement::Assignment {
                    assignment_id,
                    attempt_id,
                } if obligation.retired_by.is_none() => Some((assignment_id, attempt_id)),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        // Leave half of the four-connection store pool available to coordinator
        // traffic. Keyed evidence is deterministic regardless of completion
        // order. Refill either slot immediately; cancellation still drops every
        // in-flight read with this closure future.
        let mut tasks = futures::stream::iter(assignments)
            .map(|(assignment_id, attempt_id)| {
                let task = read_assignment(assignment_id);
                async move { (assignment_id, attempt_id, task.await) }
            })
            .buffer_unordered(2);
        while let Some((assignment_id, attempt_id, task)) = tasks.next().await {
            if let Ok(task) = task
                && child_integrated(&task, attempt_id)
            {
                use sha2::Digest;
                // Current receipt, gates, handoffs and epoch are the result
                // identity. Observation logs are neither copied nor authority.
                if let Ok(bytes) = serde_json::to_vec(&(
                    &task.current_attempt,
                    &task.receipt,
                    &task.criterion_assessments,
                    &task.gates,
                    &task.workspace_status,
                    &task.isolation_handoff,
                    &task.integration_handoffs,
                )) {
                    let reference = format!(
                        "assignment:{assignment_id}:{attempt_id}:{}",
                        hex::encode(sha2::Sha256::digest(bytes))
                    );
                    // The owner just proved this exact attempt integrated. Bind
                    // its typed criteria here rather than asking the model to
                    // discover and copy a newly generated opaque pointer.
                    for obligation in obligations.values_mut() {
                        if matches!(obligation.requirement, EvidenceRequirement::Assignment {
                            assignment_id: id, attempt_id: attempt
                        } if id == assignment_id && attempt == attempt_id)
                            && obligation.retired_by.is_none()
                        {
                            obligation.evidence_ref = Some(reference.clone());
                        }
                    }
                    evidence.observations.insert(
                        reference,
                        AcceptedEvidence::Assignment {
                            assignment_id,
                            attempt_id,
                        },
                    );
                }
            }
        }
        // A changed task/input revision invalidates the entire snapshot. This
        // check is cheap and does not hold an owner lock over child storage.
        let current = self.current.lock().await;
        let input = self
            .accepted_input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current_input = input
            .as_ref()
            .map(super::AcceptedPlanInput::review_revision);
        if current.plan.as_ref().map(|_| current.revision()) != revision.as_deref()
            || current_input != input_revision
        {
            return evaluate(
                revision,
                current_input,
                None,
                mutation_revision,
                &obligations,
                &AcceptanceEvidence::default(),
            );
        }
        self.publish_acceptance_evidence(evidence.clone());
        evaluate(
            revision,
            input_revision,
            reviewed.as_deref(),
            mutation_revision,
            &obligations,
            &evidence,
        )
    }
}

fn child_integrated(task: &codex_agent_task_store::AgentTask, attempt_id: AttemptId) -> bool {
    use codex_agent_task_store::AgentStatusClaim;
    use codex_agent_task_store::AssignmentAdmissionOrigin;
    use codex_agent_task_store::GateKind;
    use codex_agent_task_store::GateStatus;
    use codex_agent_task_store::IsolationHandoffState;
    let Some(receipt) = &task.receipt else {
        return false;
    };
    task.current_attempt.attempt_id == attempt_id
        // No destination/attempt-bound integration producer exists yet. Missing
        // handoffs must not turn a required integration plan into a pass.
        && task.assignment.integration_plan
            != codex_agent_task_store::IntegrationPlan::TypedIntegratorRequired
        && task.current_attempt.state == codex_agent_task_store::AttemptState::Completed
        && receipt.attempt_id == attempt_id
        && receipt.status == AgentStatusClaim::Completed
        && !matches!(
            task.assignment.admission_origin,
            AssignmentAdmissionOrigin::LegacyMessage { .. }
        )
        // Resolve through the store's current declared evidence policies, not
        // a receipt claim, a nonempty pointer, or the deprecated epoch counter.
        && !task.criterion_assessments.is_empty()
        && task.criterion_assessments.iter().all(|criterion| criterion.satisfied)
        && task.workspace_status.stale_reason.is_none()
        && task.workspace_status.pending_gates.is_empty()
        && task.gates.iter().all(|gate| {
            matches!(gate.status, GateStatus::Passed | GateStatus::Waived)
        })
        && [GateKind::Review, GateKind::Verification]
            .iter()
            .all(|required| task.gates.iter().any(|gate| gate.kind == *required))
        && task
            .isolation_handoff
            .as_ref()
            .is_none_or(|handoff| handoff.state == IsolationHandoffState::Integrated)
        && task
            .integration_handoffs
            .iter()
            .all(|handoff| handoff.state == IsolationHandoffState::Integrated)
}

#[cfg(test)]
#[path = "task_acceptance_tests.rs"]
mod tests;
#[cfg(test)]
mod audit150_projection_tests {
    use super::*;
    #[test]
    fn only_active_referenced_or_satisfying_evidence_enters_model_view() {
        let mut evidence = AcceptanceEvidence::default();
        for index in 0..100 {
            evidence.observations.insert(format!("source:{index}"), AcceptedEvidence::Inspection {
                environment_id:"local".into(),path:format!("/repo/{index}.rs"),ranges:vec![(0,100)],
            });
        }
        let origin = PlanInputReference { turn_id:"turn".into(),sha256:"a".repeat(64) };
        let obligation = |path: &str, reference: Option<&str>| AcceptanceObligation {
            text:"Read required source".into(),origin:origin.clone(),
            requirement:EvidenceRequirement::Inspection {environment_id:"local".into(),path:path.into(),start:0,end:100},
            plan_requirement_ids:vec![],evidence_ref:reference.map(str::to_owned),
            blocker:None,retired_by:None,retirement_reason:None,
        };
        let mut active = BTreeMap::from([("one".into(),obligation("/repo/1.rs",None)),
            ("two".into(),obligation("/repo/2.rs",Some("source:3")))]);
        let projected = evidence.for_obligations(&active);
        assert_eq!(projected.observations.len(),3);
        assert!(projected.observations.contains_key("source:1"));
        assert!(projected.observations.contains_key("source:2"));
        assert!(projected.observations.contains_key("source:3"));
        assert_eq!(evidence.observations.len(),100);
        active.get_mut("two").unwrap().retired_by = Some(origin.clone());
        assert_eq!(evidence.for_obligations(&active).observations.len(),1);
        active.insert("later".into(),obligation("/repo/99.rs",None));
        assert!(evidence.for_obligations(&active).observations.contains_key("source:99"));
    }
}
