use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use tokio::sync::Mutex;

#[path = "task_acceptance.rs"]
pub(crate) mod acceptance;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanUpdateEffect {
    Initial,
    StructuralRevision,
    StatusOnly,
    NoOp,
}

impl PlanUpdateEffect {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::StructuralRevision => "structural_revision",
            Self::StatusOnly => "status_only",
            Self::NoOp => "no_op",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanStoreUpdate {
    pub(crate) current: UpdatePlanArgs,
    pub(crate) effect: PlanUpdateEffect,
    pub(crate) lineage: PlanLineage,
}

/// Original obligations are immutable, even when checklist wording changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanLineage {
    pub(crate) requirements: BTreeMap<String, PlanRequirement>,
    pub(crate) step_requirements: BTreeMap<String, Vec<String>>,
    /// Only renamed identities need an override; empty preserves legacy revisions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) step_identities: BTreeMap<String, String>,
    /// Executable nodes belong to the task coordinator, never checklist order.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) workflow: BTreeMap<String, PlanExecutionNode>,
    /// Explicit checklist resolutions; never behavioral validation receipts.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) resolved_requirements: BTreeSet<String>,
    /// Reopening/rewording a step advances its CAS version, independently of peers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) step_revisions: BTreeMap<String, u64>,
    /// Provenance of the accepted user input cited for a declared scope change.
    /// The host verifies provenance, not the model's interpretation of that input.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) accepted_supersessions: BTreeMap<String, PlanInputReference>,
    /// Acceptance criteria are independent of checklist status and survive
    /// replacing, resolving or completing every displayed step.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) acceptance: BTreeMap<String, acceptance::AcceptanceObligation>,
    /// Fulfilled prior task scope, not current proof or new-turn debt. IDs and
    /// immutable contracts remain reserved; explicit reviewed updates reopen them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) acceptance_history: BTreeMap<String, acceptance::AcceptanceObligation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) acceptance_reviewed_revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanInputReference {
    pub(crate) turn_id: String,
    pub(crate) sha256: String,
}

/// Exact text parts from authentic accepted inputs in the active turn, not rendered context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcceptedPlanInput(
    pub(crate) String,
    pub(crate) HashSet<String>,
    Vec<PlanInputReference>,
    u64,
);

impl AcceptedPlanInput {
    pub(crate) fn from_texts<'a>(turn_id: &str, texts: impl Iterator<Item = &'a str>) -> Self {
        Self(
            turn_id.to_owned(),
            texts
                .filter(|text| !text.trim().is_empty())
                .map(|text| hex::encode(Sha256::digest(text.as_bytes())))
                .collect(),
            Vec::new(),
            1,
        )
    }

    pub(crate) fn references(&self) -> Vec<PlanInputReference> {
        let mut current = self.1.iter().cloned().collect::<Vec<_>>();
        current.sort();
        current
            .into_iter()
            .map(|sha256| PlanInputReference {
                turn_id: self.0.clone(),
                sha256,
            })
            .chain(self.2.iter().cloned())
            .collect()
    }

    /// Keep provenance, not conversation text or semantic permission. Only the
    /// prompt projection is bounded: pruning the owner would revoke unapplied
    /// corrections after enough unrelated messages. Storage grows with accepted
    /// user boundaries, never tool output or model generations.
    pub(crate) fn retain_earlier(&mut self, earlier: &Self) {
        self.3 = self.3.saturating_add(earlier.3);
        // Non-text steering in this accepted turn does not revoke an earlier
        // correction. Across turns an unknown boundary remains conservative.
        if self.1.is_empty() && self.0 != earlier.0 {
            return;
        }
        for reference in earlier.references() {
            if !(reference.turn_id == self.0 && self.1.contains(&reference.sha256))
                && !self.2.contains(&reference)
            {
                self.2.push(reference);
            }
        }
    }

    pub(crate) fn review_revision(&self) -> String {
        let references = self.references();
        let mut hash = Sha256::new();
        // Include the latest boundary even when it has no text. A sampled
        // interpretation must be reconciled again after image-only steering.
        hash.update((self.0.len() as u64).to_le_bytes());
        hash.update(self.0.as_bytes());
        hash.update((self.1.len() as u64).to_le_bytes());
        hash.update(self.3.to_le_bytes());
        for reference in references {
            hash.update((reference.turn_id.len() as u64).to_le_bytes());
            hash.update(reference.turn_id.as_bytes());
            hash.update(reference.sha256.as_bytes());
        }
        hex::encode(hash.finalize())
    }

    fn permits(&self, reference: &PlanInputReference, reviewed: Option<&str>) -> bool {
        (reference.turn_id == self.0 && self.1.contains(&reference.sha256))
            || (self.2.contains(reference)
                && reviewed.is_some_and(|revision| revision == self.review_revision()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanExecutionNode {
    pub(crate) assignment_id: codex_agent_task_store::AssignmentId,
    pub(crate) dependencies: Vec<codex_agent_task_store::AssignmentId>,
    pub(crate) capability_profile: codex_agent_task_store::CapabilityProfile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanRequirement {
    pub(crate) text: String,
    pub(crate) status: StepStatus,
    /// Model-proposed retirement, not accepted user authorization. Kept under
    /// the historical field name for replay compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) superseded_reason: Option<String>,
}

/// Derived checklist state, never a validation or publication receipt. Keeping
/// the unresolved IDs separate avoids reconstructing obligations from prose.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanObligationSummary {
    pub(crate) completed: usize,
    pub(crate) superseded: usize,
    pub(crate) unresolved: Vec<String>,
}

impl PlanLineage {
    pub(crate) fn obligation_summary(&self, plan: &UpdatePlanArgs) -> PlanObligationSummary {
        // Old histories carry only the checklist. Reconstruct from that source,
        // not a possibly stale serialized summary.
        if self.is_empty() && !plan.plan.is_empty() {
            return Self::from_plan(Some(plan)).obligation_summary(plan);
        }
        let mut summary = PlanObligationSummary::default();
        for (id, requirement) in &self.requirements {
            if requirement.superseded_reason.is_some()
                && self.accepted_supersessions.contains_key(id)
            {
                summary.superseded += 1;
            } else if requirement.superseded_reason.is_some() {
                // This format records a model proposal, not an accepted user
                // instruction. Keep it unresolved until original scope is
                // explicitly accounted for; explanatory prose is not authority.
                summary.unresolved.push(id.clone());
            } else if requirement.status == StepStatus::Completed {
                summary.completed += 1;
            } else {
                summary.unresolved.push(id.clone());
            }
        }
        summary
    }

    pub(crate) fn step_id(&self, text: &str) -> String {
        let key = plan_step_id(text);
        // Preserve IDs published by older builds, including renamed steps.
        let legacy = hex::encode(Sha256::digest(text.as_bytes()));
        self.step_identities
            .get(&key)
            .or_else(|| self.step_identities.get(&legacy))
            .cloned()
            .or_else(|| {
                self.step_requirements
                    .contains_key(&legacy)
                    .then_some(legacy)
            })
            .unwrap_or(key)
    }

    fn is_empty(&self) -> bool {
        self.requirements.is_empty()
            && self.step_requirements.is_empty()
            && self.step_identities.is_empty()
            && self.workflow.is_empty()
            && self.resolved_requirements.is_empty()
            && self.step_revisions.is_empty()
            && self.accepted_supersessions.is_empty()
            && self.acceptance.is_empty()
            && self.acceptance_history.is_empty()
            && self.acceptance_reviewed_revision.is_none()
    }

    /// The checklist already carries these exact entries. Replay reconstructs
    /// them; preserve splits, renames, supersessions and legacy identities.
    pub(crate) fn compact_for_plan(&self, plan: &UpdatePlanArgs) -> Self {
        let mut compact = self.clone();
        let mut references = HashMap::<&str, usize>::new();
        for ids in self.step_requirements.values() {
            for id in ids {
                *references.entry(id.as_str()).or_default() += 1;
            }
        }
        for obligation in self.acceptance.values() {
            for id in &obligation.plan_requirement_ids {
                *references.entry(id.as_str()).or_default() += 1;
            }
        }
        for step in &plan.plan {
            let id = self.step_id(&step.step);
            if id == plan_step_id(&step.step)
                && self
                    .step_requirements
                    .get(&id)
                    .is_some_and(|ids| ids == std::slice::from_ref(&id))
                && references.get(id.as_str()) == Some(&1)
                && self.requirements.get(&id).is_some_and(|requirement| {
                    requirement.text == step.step
                        && requirement.status == step.status
                        && requirement.superseded_reason.is_none()
                })
            {
                compact.requirements.remove(&id);
                compact.step_requirements.remove(&id);
                compact.resolved_requirements.remove(&id);
            }
        }
        compact
    }

    /// Prompt projection only. Durable snapshots retain the full audit history.
    pub(crate) fn active_for_plan(&self, plan: &UpdatePlanArgs) -> Self {
        let mut active = self.compact_for_plan(plan);
        active.acceptance_history.clear();
        let mut mapped = active
            .step_requirements
            .values()
            .flatten()
            .cloned()
            .collect::<HashSet<_>>();
        mapped.extend(
            active
                .acceptance
                .values()
                .flat_map(|obligation| obligation.plan_requirement_ids.iter().cloned()),
        );
        active.requirements.retain(|id, requirement| {
            (!active.accepted_supersessions.contains_key(id)
                && (requirement.status != StepStatus::Completed
                    || requirement.superseded_reason.is_some()))
                || mapped.contains(id)
        });
        active
            .resolved_requirements
            .retain(|id| active.requirements.contains_key(id));
        active
            .accepted_supersessions
            .retain(|id, _| active.requirements.contains_key(id));
        active
    }

    fn orphan(&self, id: &str) -> Option<&PlanRequirement> {
        self.requirements.get(id).filter(|requirement| {
            (requirement.status != StepStatus::Completed || requirement.superseded_reason.is_some())
                && !self
                    .step_requirements
                    .values()
                    .any(|ids| ids.iter().any(|mapped| mapped == id))
        })
    }

    fn revise(
        &mut self,
        previous_plan: Option<&UpdatePlanArgs>,
        steps: &[PlanStepArg],
        superseded: &[SupersededStep],
    ) {
        let previous_ids = previous_plan
            .into_iter()
            .flat_map(|plan| &plan.plan)
            .map(|step| (step.step.as_str(), self.step_id(&step.step)))
            .collect::<HashMap<_, _>>();
        let mut references = HashMap::<String, usize>::new();
        for step in steps {
            let mut sources = step.continues.iter().cloned().collect::<HashSet<_>>();
            sources.extend(previous_ids.get(step.step.as_str()).cloned());
            for id in sources {
                *references.entry(id).or_default() += 1;
            }
        }
        let previous = std::mem::take(&mut self.step_requirements);
        self.step_identities.clear();
        for step in steps {
            let text_id = plan_step_id(&step.step);
            // A one-to-one continuation is a rename, not a new step. New split
            // and merge steps retain the original obligations under new IDs.
            let continued_id = (step.continues.len() == 1)
                .then(|| &step.continues[0])
                .filter(|id| references.get(*id) == Some(&1));
            let mut step_id = previous_ids
                .get(step.step.as_str())
                .cloned()
                .or_else(|| continued_id.cloned())
                .unwrap_or_else(|| text_id.clone());
            if !previous_ids.contains_key(step.step.as_str())
                && !continued_id.is_some_and(|id| previous.contains_key(id))
                && previous_plan.is_some()
            {
                step_id = uuid::Uuid::now_v7().to_string();
            }
            if step_id != text_id {
                self.step_identities
                    .insert(text_id.clone(), step_id.clone());
            }
            let mut ids = previous.get(&step_id).cloned().unwrap_or_default();
            for source in &step.continues {
                ids.extend(previous.get(source).into_iter().flatten().cloned());
                if !previous.contains_key(source) && self.requirements.contains_key(source) {
                    ids.push(source.clone());
                }
            }
            ids.sort();
            ids.dedup();
            // Inheritance carries old scope; it must not erase newly introduced
            // acceptance text. A rename may resolve its original IDs explicitly.
            if !ids.iter().any(|id| {
                self.requirements
                    .get(id)
                    .is_some_and(|r| r.text == step.step)
            }) {
                // Reintroducing identical text must not overwrite a retired
                // requirement's original status or supersession record.
                let id = if self.requirements.contains_key(&text_id) {
                    uuid::Uuid::now_v7().to_string()
                } else {
                    text_id
                };
                self.requirements.insert(
                    id.clone(),
                    PlanRequirement {
                        text: step.step.clone(),
                        status: step.status,
                        superseded_reason: None,
                    },
                );
                ids.push(id);
            }
            ids.sort();
            ids.dedup();
            self.step_requirements.insert(step_id, ids);
        }
        self.workflow
            .retain(|step_id, _| self.step_requirements.contains_key(step_id));
        self.step_revisions
            .retain(|step_id, _| self.step_requirements.contains_key(step_id));
        // Carrying scope forward reactivates it, rather than leaving a retirement
        // proposal that restore could mistake for an invalid mapping.
        for id in self.step_requirements.values().flatten() {
            if let Some(requirement) = self.requirements.get_mut(id) {
                requirement.superseded_reason = None;
            }
            self.accepted_supersessions.remove(id);
        }
        for dropped in superseded {
            let ids = previous
                .get(&dropped.step_id)
                .cloned()
                .unwrap_or_else(|| vec![dropped.step_id.clone()]);
            for id in &ids {
                // A split requirement may still be represented by another step.
                if !self.step_requirements.values().any(|ids| ids.contains(id))
                    && let Some(requirement) = self.requirements.get_mut(id)
                {
                    requirement.superseded_reason = Some(dropped.reason.trim().to_string());
                }
            }
        }
    }

    fn update_statuses(&mut self, plan: &UpdatePlanArgs) {
        // Aggregate actual edges once, rather than scanning every step for
        // every historical requirement (including no-longer-active history).
        let mut states = HashMap::<String, (bool, bool, bool, bool)>::new();
        for step in &plan.plan {
            let step_id = self.step_id(&step.step);
            for id in self.step_requirements.get(&step_id).into_iter().flatten() {
                let Some(requirement) = self.requirements.get(id) else {
                    continue;
                };
                let state = states
                    .entry(id.clone())
                    .or_insert((true, false, true, false));
                state.0 &= step.status == StepStatus::Completed;
                state.1 |= step.status == StepStatus::InProgress;
                state.2 &= step.step == requirement.text;
                state.3 |= step.status == StepStatus::Completed && step.step != requirement.text;
            }
        }
        for (id, (completed, in_progress, same_text, renamed_completion)) in states {
            if !completed {
                self.resolved_requirements.remove(&id);
            }
            #[expect(
                clippy::expect_used,
                reason = "states contains only existing requirements; the map is unchanged while aggregating"
            )]
            let requirement = self.requirements.get_mut(&id).expect("known requirement");
            requirement.status =
                if completed && (same_text || self.resolved_requirements.contains(&id)) {
                    requirement.superseded_reason = None;
                    StepStatus::Completed
                } else if in_progress || renamed_completion {
                    StepStatus::InProgress
                } else {
                    StepStatus::Pending
                };
        }
    }

    fn from_plan(plan: Option<&UpdatePlanArgs>) -> Self {
        let mut lineage = Self::default();
        if let Some(plan) = plan {
            lineage.revise(
                None,
                &plan
                    .plan
                    .iter()
                    .map(|step| PlanStepArg {
                        step: step.step.clone(),
                        status: step.status,
                        continues: Vec::new(),
                    })
                    .collect::<Vec<_>>(),
                &[],
            );
        }
        lineage
    }

    /// Reconcile historical metadata with the owning checklist, retaining
    /// original obligations even when they are no longer mapped to a step.
    /// A partial lineage must not make a pending checklist appear resolved.
    fn reconcile_restored(mut self, plan: Option<&UpdatePlanArgs>) -> Self {
        let Some(plan) = plan else {
            return self;
        };
        for ids in self.step_requirements.values_mut() {
            ids.retain(|id| self.requirements.contains_key(id));
        }
        self.revise(
            Some(plan),
            &plan
                .plan
                .iter()
                .map(|step| PlanStepArg {
                    step: step.step.clone(),
                    status: step.status,
                    continues: Vec::new(),
                })
                .collect::<Vec<_>>(),
            &[],
        );
        self.update_statuses(plan);
        self
    }
}

#[derive(Debug, Clone, Default)]
struct PlanState {
    plan: Option<UpdatePlanArgs>,
    lineage: PlanLineage,
    durably_published: bool,
    derived: std::sync::Arc<PlanDerived>,
}

/// Derived views share the immutable owner revision, never durable authority.
/// An update detaches this cache before changing either plan or lineage.
#[derive(Debug, Default)]
struct PlanDerived {
    revision: std::sync::OnceLock<String>,
    execution: std::sync::OnceLock<Option<PlanExecutionSnapshot>>,
    task: [std::sync::OnceLock<crate::context::world_state::TaskState>; 2],
}

/// Host-only sampling fence. Every actual change detaches the immutable owner;
/// identity also catches change-then-restore (ABA), without changing durable CAS.
#[derive(Debug, Clone)]
pub(crate) struct PlanSamplingRevision(std::sync::Arc<PlanDerived>);

impl PlanState {
    fn revision(&self) -> &str {
        self.derived
            .revision
            .get_or_init(|| plan_revision_with_lineage(self.plan.as_ref(), &self.lineage))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanExecutionSnapshot {
    pub(crate) revision: String,
    pub(crate) obligations: PlanObligationSummary,
    pub(crate) has_steps: bool,
    pub(crate) acceptance: BTreeMap<String, acceptance::AcceptanceObligation>,
    pub(crate) acceptance_reviewed_revision: Option<String>,
}

impl PlanExecutionSnapshot {
    #[cfg(test)]
    pub(crate) fn new(plan: &UpdatePlanArgs, lineage: &PlanLineage) -> Self {
        Self {
            revision: plan_revision_with_lineage(Some(plan), lineage),
            obligations: lineage.obligation_summary(plan),
            has_steps: !plan.plan.is_empty(),
            acceptance: lineage.acceptance.clone(),
            acceptance_reviewed_revision: lineage.acceptance_reviewed_revision.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanStatusUpdate {
    pub(crate) index: Option<usize>,
    pub(crate) step_id: Option<String>,
    pub(crate) status: StepStatus,
}

/// `update_plan` arguments. Exactly one of `plan` or `set` is supplied.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanToolArgs {
    pub(crate) explanation: Option<String>,
    pub(crate) expected_revision: Option<String>,
    #[serde(skip)]
    pub(crate) sampling_revision: Option<PlanSamplingRevision>,
    pub(crate) plan: Option<Vec<PlanStepArg>>,
    pub(crate) set: Option<Vec<PlanStatusUpdate>>,
    /// Per-step CAS for independent deltas. Zero is the initial/legacy version.
    pub(crate) expected_step_revisions: Option<BTreeMap<String, u64>>,
    /// Resolve original requirement IDs after all their descendants complete.
    #[serde(default)]
    pub(crate) resolve: Vec<String>,
    /// Exact accepted user text, keyed by superseded step/orphan ID. Provenance
    /// is verified under the plan lock; a free-form reason alone cannot retire.
    #[serde(default)]
    pub(crate) scope_change_instructions: BTreeMap<String, String>,
    /// Compact alternative with the same accepted-input membership checks.
    #[serde(default)]
    pub(crate) scope_change_refs: BTreeMap<String, PlanInputReference>,
    /// Explicit interpretation of earlier input against every later accepted
    /// input in this journal. Authenticity is host-checked; semantics are not.
    pub(crate) scope_change_reviewed_revision: Option<String>,
    /// Incremental independent criteria; omission never deletes existing scope.
    pub(crate) acceptance: Option<BTreeMap<String, acceptance::AcceptanceObligation>>,
    pub(crate) acceptance_reviewed_revision: Option<String>,
    #[serde(default)]
    pub(crate) superseded: Vec<SupersededStep>,
    /// Stable checklist step ID -> existing executable assignment ID.
    pub(crate) workflow: Option<BTreeMap<String, codex_agent_task_store::AssignmentId>>,
    /// Populated only by the handler after consulting the owning coordinator.
    #[serde(skip)]
    pub(crate) resolved_workflow: Option<BTreeMap<String, PlanExecutionNode>>,
}

/// A submitted checklist step. `continues` names step IDs from the previous
/// plan whose unfinished obligations this step carries forward.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanStepArg {
    pub(crate) step: String,
    pub(crate) status: StepStatus,
    #[serde(default)]
    pub(crate) continues: Vec<String>,
}

/// An unfinished step that a plan revision intentionally drops.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SupersededStep {
    pub(crate) step_id: String,
    pub(crate) reason: String,
}

/// Persisted `update_plan` response shared by rendering and legacy history replay.
/// The plan field is required; display metadata may be absent in older histories.
#[derive(Serialize, Deserialize)]
pub(crate) struct PlanToolResponse {
    pub(crate) current_plan: UpdatePlanArgs,
    #[serde(default)]
    pub(crate) obligations: PlanObligationSummary,
    #[serde(default = "checklist_completion_authority")]
    pub(crate) completion_authority: String,
    #[serde(default, skip_serializing_if = "PlanLineage::is_empty")]
    pub(crate) lineage: PlanLineage,
    /// Model results project active lineage; durable snapshots retain history.
    #[serde(default = "complete_lineage")]
    pub(crate) lineage_complete: bool,
    #[serde(default)]
    pub(crate) revision: String,
    #[serde(default)]
    pub(crate) step_ids: Vec<String>,
    #[serde(default)]
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) effect: String,
    #[serde(default)]
    pub(crate) no_progress: bool,
}

impl PlanToolResponse {
    /// A projection can borrow authority only from the same canonical revision.
    pub(crate) fn full_lineage_for(
        &self,
        plan: &UpdatePlanArgs,
        revision: Option<&str>,
    ) -> Option<PlanLineage> {
        if !self.lineage_complete || self.current_plan != *plan {
            return None;
        }
        let lineage = self.lineage.clone().reconcile_restored(Some(plan));
        if let Some(revision) = revision
            && (self.revision != revision
                || plan_revision_with_lineage(Some(plan), &lineage) != revision)
        {
            return None;
        }
        Some(lineage)
    }
}

fn complete_lineage() -> bool {
    true
}

pub(crate) fn checklist_completion_authority() -> String {
    "checklist_only".to_string()
}

pub(crate) fn plan_revision(plan: Option<&UpdatePlanArgs>) -> String {
    #[expect(
        clippy::expect_used,
        reason = "plan serialization contains only strings, vectors, options and unit enums"
    )]
    let bytes = serde_json::to_vec(&plan).expect("plan serializes");
    hex::encode(Sha256::digest(bytes))
}

pub(crate) fn plan_revision_with_lineage(
    plan: Option<&UpdatePlanArgs>,
    lineage: &PlanLineage,
) -> String {
    if lineage.is_empty() || *lineage == PlanLineage::from_plan(plan) {
        return plan_revision(plan);
    }
    #[expect(
        clippy::expect_used,
        reason = "plan and lineage use derived JSON-compatible fields and string-keyed maps"
    )]
    let bytes = serde_json::to_vec(&(plan, lineage)).expect("plan serializes");
    hex::encode(Sha256::digest(bytes))
}

pub(crate) fn plan_step_id(step: &str) -> String {
    hex::encode(Sha256::digest(step.as_bytes()))[..16].to_string()
}

pub(crate) fn plan_response_from_tool_output(
    output: &FunctionCallOutputPayload,
) -> Option<PlanToolResponse> {
    if output.success == Some(false) {
        return None;
    }
    let FunctionCallOutputBody::Text(text) = &output.body else {
        return None;
    };
    serde_json::from_str::<PlanToolResponse>(text).ok()
}

const PLAN_SNAPSHOT_PREFIX: &str = "<codex_plan_state_v1>\n";
const PLAN_SNAPSHOT_SUFFIX: &str = "\n</codex_plan_state_v1>";

/// Rollout-only metadata. Unlike ordinary tool output, this also survives
/// nested calls and cancellation after a state commit.
pub(crate) fn plan_snapshot_item(response: &serde_json::Value) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "developer".into(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: format!("{PLAN_SNAPSHOT_PREFIX}{response}{PLAN_SNAPSHOT_SUFFIX}"),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

pub(crate) fn plan_snapshot_from_item(item: &ResponseItem) -> Option<PlanToolResponse> {
    let ResponseItem::Message { role, content, .. } = item else {
        return None;
    };
    let [codex_protocol::models::ContentItem::InputText { text }] = content.as_slice() else {
        return None;
    };
    if role != "developer" {
        return None;
    }
    let json = text
        .strip_prefix(PLAN_SNAPSHOT_PREFIX)?
        .strip_suffix(PLAN_SNAPSHOT_SUFFIX)?;
    serde_json::from_str(json).ok()
}

/// Authoritative session-local TODO/checklist state.
#[derive(Debug, Default)]
pub(crate) struct PlanStore {
    current: Mutex<PlanState>,
    /// Serializes publication through client notification, independently of
    /// repository tool admission. Lock order: publication -> current -> rollout.
    publication: Mutex<()>,
    // Every production access already holds current. These short in-memory
    // reads/writes need no second async acquisition while publication is fenced.
    accepted_input: std::sync::Mutex<Option<AcceptedPlanInput>>,
    /// Turn-local observations, deliberately never restored as passing proof.
    acceptance_evidence: std::sync::Mutex<acceptance::AcceptanceEvidence>,
}

/// Holds the existing plan lock across publication so revision validation and
/// the durable snapshot refer to the same update. Dropping it changes nothing.
pub(crate) struct StagedPlanUpdate<'a> {
    current: tokio::sync::MutexGuard<'a, PlanState>,
    next: Option<PlanState>,
    pub(crate) update: PlanStoreUpdate,
}

impl StagedPlanUpdate<'_> {
    pub(crate) fn needs_publication(&self) -> bool {
        self.update.effect != PlanUpdateEffect::NoOp || !self.current.durably_published
    }

    pub(crate) fn commit_published(mut self) -> PlanStoreUpdate {
        if let Some(next) = self.next.as_mut() {
            next.durably_published = true;
        } else {
            self.current.durably_published = true;
        }
        self.commit()
    }

    pub(crate) fn commit(mut self) -> PlanStoreUpdate {
        if let Some(next) = self.next {
            *self.current = next;
        }
        self.update
    }
}

impl PlanStore {
    pub(crate) async fn sampling_revision(&self) -> PlanSamplingRevision {
        PlanSamplingRevision(std::sync::Arc::clone(&self.current.lock().await.derived))
    }

    pub(crate) async fn publication_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.publication.lock().await
    }

    /// Called only after a real user input has been accepted and recorded. Tool
    /// output, injected context and inter-agent messages never enter this path.
    pub(crate) async fn record_accepted_input<'a>(
        &self,
        turn_id: &str,
        texts: impl Iterator<Item = &'a str>,
    ) {
        let mut accepted = AcceptedPlanInput::from_texts(turn_id, texts);
        // Use the current-state lock first: accepted provenance cannot change
        // between staging a scope retirement and publishing it durably.
        let mut current = self.current.lock().await;
        let mut inputs = self
            .accepted_input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = inputs.as_ref() {
            accepted.retain_earlier(previous);
        }
        *inputs = Some(accepted);
        self.publish_acceptance_evidence(Default::default());
        // Authenticity is not interpretation: retaining an earlier reference
        // does not authorize ignoring a later contradictory user instruction.
        current.derived = Default::default();
    }
    /// Publish a matched explicit UI answer only after its source receipt is
    /// persisted. Lock order remains publication -> current -> rollout.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Receipt persistence and accepted-input publication form one transaction against concurrent restores and input acceptance"
    )]
    pub(crate) async fn record_accepted_decision(
        &self,
        mut accepted: AcceptedPlanInput,
        persist: impl std::future::Future<Output = std::io::Result<()>>,
    ) -> std::io::Result<()> {
        let mut current = self.current.lock().await;
        persist.await?;
        let mut inputs = self
            .accepted_input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = inputs.as_ref() {
            accepted.retain_earlier(previous);
        }
        *inputs = Some(accepted);
        current.derived = Default::default();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn active_requirement_count(&self) -> usize {
        self.current
            .lock()
            .await
            .lineage
            .requirements
            .values()
            .filter(|requirement| {
                requirement.status != StepStatus::Completed
                    || requirement.superseded_reason.is_some()
            })
            .count()
    }

    pub(crate) async fn snapshot(&self) -> Option<UpdatePlanArgs> {
        self.current.lock().await.plan.clone()
    }

    pub(crate) async fn execution_snapshot(&self) -> Option<PlanExecutionSnapshot> {
        let current = self.current.lock().await;
        current
            .derived
            .execution
            .get_or_init(|| {
                let plan = current.plan.as_ref()?;
                Some(PlanExecutionSnapshot {
                    revision: current.revision().to_string(),
                    obligations: current.lineage.obligation_summary(plan),
                    has_steps: !plan.plan.is_empty(),
                    acceptance: current.lineage.acceptance.clone(),
                    acceptance_reviewed_revision: current
                        .lineage
                        .acceptance_reviewed_revision
                        .clone(),
                })
            })
            .clone()
    }

    pub(crate) async fn task_state(
        &self,
        suspended: bool,
    ) -> crate::context::world_state::TaskState {
        let current = self.current.lock().await;
        let accepted = self
            .accepted_input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.derived.task[usize::from(suspended)]
            .get_or_init(|| {
                crate::context::world_state::TaskState::from_plan(
                    current
                        .plan
                        .as_ref()
                        .map(|plan| (plan, &current.lineage, current.revision())),
                )
                .with_execution_suspended(suspended)
                .with_accepted_input_refs(
                    accepted
                        .as_ref()
                        .filter(|_| current.plan.is_some())
                        .map(AcceptedPlanInput::references)
                        .unwrap_or_default(),
                )
                .with_scope_input_revision(
                    accepted.as_ref().map(AcceptedPlanInput::review_revision),
                )
            })
            .clone()
            .with_acceptance_evidence(
                &self
                    .acceptance_evidence
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .for_obligations(&current.lineage.acceptance),
            )
    }

    pub(crate) async fn snapshot_with_lineage(&self) -> Option<(UpdatePlanArgs, PlanLineage)> {
        let current = self.current.lock().await;
        Some((current.plan.clone()?, current.lineage.clone()))
    }

    #[cfg(test)]
    pub(crate) async fn restore_from_history(&self, items: &[ResponseItem]) -> bool {
        self.restore_reconstructed(None, None, None, items).await
    }

    /// Replace checklist, lineage and input authority under the same owner lock.
    /// Missing trustworthy provenance must clear an old in-process register.
    pub(crate) async fn restore_reconstructed(
        &self,
        plan: Option<UpdatePlanArgs>,
        lineage: Option<PlanLineage>,
        accepted_input: Option<AcceptedPlanInput>,
        items: &[ResponseItem],
    ) -> bool {
        let update_call_ids = items
            .iter()
            .filter_map(|item| match item {
                ResponseItem::FunctionCall {
                    name,
                    namespace: None,
                    call_id,
                    ..
                } if name == "update_plan" => Some(call_id.as_str()),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let restored = plan
            .map(|plan| (plan, lineage.unwrap_or_default()))
            .or_else(|| {
                items.iter().rev().find_map(|item| {
                    let ResponseItem::FunctionCallOutput {
                        call_id, output, ..
                    } = item
                    else {
                        return None;
                    };
                    if !update_call_ids.contains(call_id.as_str()) {
                        return None;
                    }
                    plan_response_from_tool_output(output)
                        .map(|response| (response.current_plan, response.lineage))
                })
            });
        let found = restored.is_some();
        let mut current = self.current.lock().await;
        *current = restored
            .map(|(plan, lineage)| {
                let lineage = lineage.reconcile_restored(Some(&plan));
                PlanState {
                    plan: Some(plan),
                    lineage,
                    durably_published: false,
                    derived: Default::default(),
                }
            })
            .unwrap_or_default();
        *self
            .accepted_input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = accepted_input;
        self.publish_acceptance_evidence(Default::default());
        found
    }

    #[cfg(test)]
    pub(crate) async fn restore(&self, plan: Option<UpdatePlanArgs>) {
        self.restore_with_lineage(plan, None).await;
    }

    #[cfg(test)]
    pub(crate) async fn restore_with_lineage(
        &self,
        plan: Option<UpdatePlanArgs>,
        lineage: Option<PlanLineage>,
    ) {
        let mut current = self.current.lock().await;
        *current = PlanState {
            lineage: lineage
                .unwrap_or_default()
                .reconcile_restored(plan.as_ref()),
            plan,
            durably_published: false,
            derived: Default::default(),
        };
        *self
            .accepted_input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.publish_acceptance_evidence(Default::default());
    }

    #[cfg(test)]
    pub(crate) async fn update(&self, next: UpdatePlanArgs) -> PlanStoreUpdate {
        let mut current = self.current.lock().await;
        current.lineage = PlanLineage::from_plan(Some(&next));
        current.derived = Default::default();
        Self::commit(&mut current, next)
    }

    #[cfg(test)]
    pub(crate) async fn update_tool(&self, args: PlanToolArgs) -> Result<PlanStoreUpdate, String> {
        Ok(self.stage_tool(args).await?.commit())
    }

    /// Prepare an update without publishing the plan or its revision.
    pub(crate) async fn stage_tool(&self, args: PlanToolArgs) -> Result<StagedPlanUpdate<'_>, String> {
        let PlanToolArgs {
            explanation,
            expected_revision,
            sampling_revision,
            plan,
            set: statuses,
            expected_step_revisions,
            resolve,
            scope_change_instructions,
            mut scope_change_refs,
            scope_change_reviewed_revision,
            acceptance,
            acceptance_reviewed_revision,
            superseded,
            workflow,
            resolved_workflow,
        } = args;
        let guard = self.current.lock().await;
        let accepted_input = if scope_change_instructions.is_empty()
            && scope_change_refs.is_empty()
            && acceptance.is_none()
            && acceptance_reviewed_revision.is_none()
        {
            None
        } else {
            self.accepted_input
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        };
        let revision = guard.revision().to_string();
        let unchanged_since_sampling = sampling_revision
            .as_ref()
            .is_some_and(|sampled| std::sync::Arc::ptr_eq(&sampled.0, &guard.derived));
        let missing_revision =
            (plan.is_some() || acceptance.is_some() || acceptance_reviewed_revision.is_some())
                && guard.plan.is_some()
                && expected_revision.is_none()
                && !unchanged_since_sampling;
        if missing_revision
            || expected_revision
                .as_deref()
                .is_some_and(|expected| expected != revision)
        {
            let reconciliation = serde_json::json!({
                "revision": revision,
                "current_plan": guard.plan,
                "step_ids": guard.plan.iter().flat_map(|plan| &plan.plan)
                    .map(|step| guard.lineage.step_id(&step.step)).collect::<Vec<_>>(),
                "lineage": guard.plan.as_ref().map(|plan| guard.lineage.active_for_plan(plan)),
                "obligations": guard.plan.as_ref().map(|plan| guard.lineage.obligation_summary(plan)),
                "completion_authority": checklist_completion_authority(),
            });
            let reason = if missing_revision {
                "plan replacements require expected_revision when the plan changed since sampling or no sampling fence is available"
            } else {
                "stale plan revision"
            };
            return Err(format!(
                "{reason}; no changes were made. Reconcile against: {reconciliation}"
            ));
        }
        let status_next = if let Some(statuses) = statuses {
            Some(Self::status_plan(
                &guard.plan,
                &guard.lineage,
                statuses,
                explanation.clone(),
                // The fence that admits a replacement also proves the indexes
                // and statuses this sample read are still current.
                expected_revision.is_some() || unchanged_since_sampling,
                expected_step_revisions.as_ref(),
                &revision,
            )?)
        } else {
            None
        };
        if plan.is_none()
            && superseded.is_empty()
            && resolve.is_empty()
            && scope_change_instructions.is_empty()
            && scope_change_refs.is_empty()
            && workflow.is_none()
            && acceptance.is_none()
            && acceptance_reviewed_revision.is_none()
            && status_next
                .as_ref()
                .is_some_and(|next| guard.plan.as_ref() == Some(next))
        {
            let update = PlanStoreUpdate {
                current: status_next.ok_or("missing unchanged status update")?,
                effect: PlanUpdateEffect::NoOp,
                lineage: guard.lineage.clone(),
            };
            return Ok(StagedPlanUpdate {
                current: guard,
                next: None,
                update,
            });
        }
        let mut current = guard.clone();
        let mut lineage = current.lineage.clone();
        if let Some(updates) = acceptance {
            if updates.len() > 128 || updates.keys().any(|id| id.trim().is_empty()) {
                return Err("acceptance requires nonempty IDs and at most 128 updates".into());
            }
            let input = accepted_input
                .as_ref()
                .ok_or("acceptance needs authentic user input")?;
            if acceptance_reviewed_revision.as_deref() != Some(input.review_revision().as_str()) {
                return Err(
                    "acceptance must be reconciled through current scope_input_revision".into(),
                );
            }
            for (id, obligation) in updates {
                match &obligation.requirement {
                    acceptance::EvidenceRequirement::Inspection { environment_id, path, .. }
                    | acceptance::EvidenceRequirement::Validation { environment_id, path, .. }
                        if environment_id.trim().is_empty() || path.trim().is_empty() => {
                        return Err("acceptance requires nonempty environment_id and path; no changes were made".into());
                    }
                    acceptance::EvidenceRequirement::Inspection { start, end, .. } if start >= end => {
                        return Err(
                            "inspection acceptance requires start < end; no changes were made".into(),
                        );
                    }
                    acceptance::EvidenceRequirement::Validation {
                        test_execution,
                        required_tests,
                        ..
                    } if (!required_tests.is_empty() && !test_execution)
                        || required_tests.iter().any(|test| {
                            test.binary.trim().is_empty()
                                || test.test.trim().is_empty()
                                || test.helpers.iter().any(|helper| helper.trim().is_empty())
                                || test.helpers.windows(2).any(|pair| pair[0] >= pair[1])
                        }) =>
                    {
                        return Err("required_tests needs test_execution and nonempty exact runner identities; no changes were made".into());
                    }
                    _ => {}
                }
                let retained_origin = lineage.acceptance.get(&id)
                    .or_else(|| lineage.acceptance_history.get(&id))
                    .is_some_and(|previous| previous.origin == obligation.origin);
                if obligation.text.trim().is_empty()
                    || (!retained_origin && !input.references().contains(&obligation.origin))
                {
                    return Err(
                        "acceptance criteria need text and an authentic accepted origin".into(),
                    );
                }
                if let Some(previous) = lineage.acceptance.get(&id)
                    .or_else(|| lineage.acceptance_history.get(&id))
                    && (previous.text != obligation.text
                        || previous.origin != obligation.origin
                        || !previous.requirement.same_contract(&obligation.requirement))
                {
                    return Err("acceptance criterion identity and proof kind are immutable; retire scope with accepted input instead".into());
                }
                if let Some(reference) = &obligation.retired_by
                    && (!input.permits(reference, acceptance_reviewed_revision.as_deref())
                        || obligation
                            .retirement_reason
                            .as_ref()
                            .is_none_or(|reason| reason.trim().is_empty()))
                {
                    return Err(
                        "retiring acceptance requires reconciled accepted input and a reason".into(),
                    );
                }
                lineage.acceptance.insert(id, obligation);
            }
            if lineage.acceptance.values().filter(|criterion| criterion.retired_by.is_none()).count() > 128 {
                return Err(
                    "at most 128 active acceptance criteria are supported; split the task explicitly".into(),
                );
            }
        }
        if let Some(reviewed) = acceptance_reviewed_revision {
            if accepted_input
                .as_ref()
                .is_none_or(|input| input.review_revision() != reviewed)
            {
                return Err(
                    "acceptance review is stale; reconcile current scope_input_revision".into(),
                );
            }
            lineage.acceptance_reviewed_revision = Some(reviewed);
        }
        let next = if let Some(plan) = plan {
            let mut steps = HashSet::new();
            if plan.iter().any(|item| item.step.trim().is_empty()) {
                return Err("plan step cannot be empty".into());
            }
            if plan.iter().any(|item| !steps.insert(item.step.as_str())) {
                return Err(
                    "plan steps must have distinct text so their stable IDs are unambiguous".into(),
                );
            }
            let explanation = account_for_removed_steps(
                current.plan.as_ref(),
                &lineage,
                &plan,
                &superseded,
                explanation,
            )?;
            lineage.revise(current.plan.as_ref(), &plan, &superseded);
            UpdatePlanArgs {
                explanation,
                plan: plan
                    .into_iter()
                    .map(|item| PlanItemArg {
                        step: item.step,
                        status: item.status,
                    })
                    .collect(),
            }
        } else if !superseded.is_empty() {
            return Err("superseded applies only to a plan revision; no changes were made".into());
        } else if let Some(next) = status_next {
            next
        } else {
            let mut next = current.plan.clone().unwrap_or(UpdatePlanArgs {
                explanation: None,
                plan: Vec::new(),
            });
            if explanation.is_some() {
                next.explanation = explanation;
            }
            next
        };
        // Step IDs name presentation rows; readiness, reconciliation and archive
        // key scope links by the requirement IDs those rows carry.
        for obligation in lineage.acceptance.values_mut() {
            if !obligation.plan_requirement_ids.iter().any(|id| {
                !lineage.requirements.contains_key(id) && lineage.step_requirements.contains_key(id)
            }) {
                continue;
            }
            let mut ids = Vec::new();
            for id in std::mem::take(&mut obligation.plan_requirement_ids) {
                match lineage.step_requirements.get(&id) {
                    Some(carried) if !lineage.requirements.contains_key(&id) => {
                        ids.extend(carried.iter().cloned());
                    }
                    _ => ids.push(id),
                }
            }
            ids.sort();
            ids.dedup();
            obligation.plan_requirement_ids = ids;
        }
        for obligation in lineage.acceptance.values() {
            if obligation.plan_requirement_ids.len() > 128
                || obligation.plan_requirement_ids.iter().any(|id| {
                    !lineage.requirements.contains_key(id)
                        && !lineage
                            .step_requirements
                            .values()
                            .any(|ids| ids.contains(id))
                        && !next
                            .plan
                            .iter()
                            .any(|step| lineage.step_id(&step.step) == *id)
                })
            {
                return Err("acceptance plan_requirement_ids must name known checklist requirements (at most 128); no changes were made".into());
            }
        }
        for (step_id, instruction) in scope_change_instructions {
            let sha256 = hex::encode(Sha256::digest(instruction.as_bytes()));
            let Some(reference) = accepted_input.as_ref().and_then(|input| {
                input.references().into_iter().find(|reference| {
                    reference.sha256 == sha256
                        && input.permits(reference, scope_change_reviewed_revision.as_deref())
                })
            }) else {
                return Err("scope change must cite exact, accepted user text; tool/context text and invented instructions are not authorization. No changes were made".into());
            };
            if scope_change_refs
                .get(&step_id)
                .is_some_and(|existing| existing != &reference)
            {
                return Err("conflicting scope change text and reference; no changes were made".into());
            }
            scope_change_refs.insert(step_id, reference);
        }
        for (step_id, reference) in scope_change_refs {
            if !superseded.iter().any(|entry| entry.step_id == step_id) {
                return Err(
                    "scope change sources must name a superseded entry; no changes were made".into(),
                );
            }
            if !accepted_input.as_ref().is_some_and(|input| {
                input.permits(&reference, scope_change_reviewed_revision.as_deref())
            }) {
                return Err("scope change reference is not accepted, or earlier input has not been reconciled through the current scope_input_revision; no changes were made".into());
            }
            let ids = current
                .lineage
                .step_requirements
                .get(&step_id)
                .cloned()
                .unwrap_or_else(|| vec![step_id]);
            for id in ids {
                if !lineage
                    .step_requirements
                    .values()
                    .any(|mapped| mapped.contains(&id))
                {
                    lineage.accepted_supersessions.insert(id, reference.clone());
                }
            }
        }
        for id in resolve {
            if !lineage.requirements.contains_key(&id) {
                return Err(format!(
                    "resolve names unknown requirement {id}; no changes were made"
                ));
            }
            let mut descendants = next.plan.iter().filter(|step| {
                lineage
                    .step_requirements
                    .get(&lineage.step_id(&step.step))
                    .is_some_and(|ids| ids.contains(&id))
            });
            let Some(first) = descendants.next() else {
                return Err(format!(
                    "reattach {id} to a descendant checklist before resolving it; orphaned scope is not completed. No changes were made"
                ));
            };
            if first.status != StepStatus::Completed
                || descendants.any(|step| step.status != StepStatus::Completed)
            {
                return Err(format!(
                    "complete the descendant checklist before resolving {id}; no changes were made"
                ));
            }
            lineage.resolved_requirements.insert(id.clone());
            let requirement = lineage.requirements.get_mut(&id).ok_or_else(|| {
                format!("resolve names unknown requirement {id}; no changes were made")
            })?;
            requirement.status = StepStatus::Completed;
            requirement.superseded_reason = None;
            lineage.accepted_supersessions.remove(&id);
        }
        if let Some(previous) = &current.plan {
            let previous_steps = previous
                .plan
                .iter()
                .map(|step| (current.lineage.step_id(&step.step), step))
                .collect::<HashMap<_, _>>();
            for step in &next.plan {
                let id = lineage.step_id(&step.step);
                if previous_steps.get(&id).is_some_and(|old| {
                    *old != step
                        || current.lineage.step_requirements.get(&id)
                            != lineage.step_requirements.get(&id)
                }) {
                    let version = current
                        .lineage
                        .step_revisions
                        .get(&id)
                        .copied()
                        .unwrap_or_default();
                    lineage.step_revisions.insert(
                        id,
                        version
                            .checked_add(1)
                            .ok_or("plan step revision exhausted")?,
                    );
                }
            }
        }
        if let Some(workflow) = workflow {
            let resolved = resolved_workflow.ok_or(
                "workflow links must be resolved by the task coordinator; no changes were made",
            )?;
            if workflow.len() != resolved.len()
                || workflow.iter().any(|(step_id, assignment_id)| {
                    !lineage.step_requirements.contains_key(step_id)
                        || resolved
                            .get(step_id)
                            .is_none_or(|node| node.assignment_id != *assignment_id)
                })
            {
                return Err(
                    "workflow names unknown steps or unverified assignments; no changes were made"
                        .into(),
                );
            }
            lineage.workflow = resolved;
        }
        // Checklist equality does not imply graph equality: a retained pending
        // step can acquire a resolved requirement without changing any row.
        // Visible changes are normalized by commit; avoid aggregating twice.
        if current.plan.as_ref() == Some(&next) && current.lineage != lineage {
            lineage.update_statuses(&next);
        }
        let lineage_changed = current.lineage != lineage;
        if lineage_changed {
            current.derived = Default::default();
        }
        current.lineage = lineage;
        let mut update = Self::commit(&mut current, next);
        if lineage_changed && update.effect == PlanUpdateEffect::NoOp {
            update.effect = PlanUpdateEffect::StructuralRevision;
        }
        Ok(StagedPlanUpdate {
            current: guard,
            next: Some(current),
            update,
        })
    }

    fn status_plan(
        current: &Option<UpdatePlanArgs>,
        lineage: &PlanLineage,
        updates: Vec<PlanStatusUpdate>,
        explanation: Option<String>,
        revision_checked: bool,
        expected_step_revisions: Option<&BTreeMap<String, u64>>,
        revision: &str,
    ) -> Result<UpdatePlanArgs, String> {
        let mut next = current
            .clone()
            .ok_or("create a plan before updating statuses")?;
        if updates.is_empty() {
            return Err("set must contain at least one status update".to_string());
        }
        // Positions change when the plan is revised; only a checked revision
        // or an unchanged sampling fence proves an index refers to the plan
        // the caller read.
        if !revision_checked && updates.iter().any(|update| update.index.is_some()) {
            return Err(format!(
                "index-based status updates require expected_revision once the plan changed since this sampling step began; current revision is {revision}. Use step_id for stable addressing. When changing the status of a versioned step, also supply its current expected_step_revisions entry or expected_revision; an unchanged status may be idempotent. No changes were made."
            ));
        }
        let mut seen = HashSet::new();
        let mut activated = HashSet::new();
        let mut by_step_id = false;
        for update in updates {
            by_step_id |= update.step_id.is_some();
            let index = match (update.index, update.step_id.as_deref()) {
                (Some(index), None) => index,
                (None, Some(id)) => {
                    let matches = next
                        .plan
                        .iter()
                        .enumerate()
                        .filter(|(_, item)| {
                            let current_id = lineage.step_id(&item.step);
                            current_id == id || (id.len() >= 8 && current_id.starts_with(id))
                        })
                        .map(|(index, _)| index)
                        .collect::<Vec<_>>();
                    if matches.len() != 1 {
                        let valid = next
                            .plan
                            .iter()
                            .map(|item| {
                                format!(
                                    "{}: {}",
                                    lineage.step_id(&item.step),
                                    quoted_step(&item.step)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        return Err(format!(
                            "unknown, too short, or ambiguous plan step ID {id}; use a full ID or a unique prefix of at least 8 characters. Current steps:\n{valid}\nNo changes were made."
                        ));
                    }
                    matches[0]
                }
                _ => return Err("provide exactly one of index or step_id per status update".into()),
            };
            if !seen.insert(index) {
                return Err(format!("duplicate plan index {index}"));
            }
            let item = next
                .plan
                .get_mut(index)
                .ok_or_else(|| format!("plan index {index} is out of range"))?;
            let id = lineage.step_id(&item.step);
            let version = lineage.step_revisions.get(&id).copied().unwrap_or_default();
            let expected = expected_step_revisions.and_then(|versions| versions.get(&id));
            if expected.is_some_and(|expected| *expected != version)
                || (!revision_checked
                    && version != 0
                    && expected.is_none()
                    && item.status != update.status)
            {
                return Err(format!(
                    "stale or missing step revision for {id}; current step revision is {version}. Supply expected_step_revisions or expected_revision. No changes were made"
                ));
            }
            if update.status == StepStatus::InProgress {
                activated.insert(index);
            }
            item.status = update.status;
        }
        let active = next
            .plan
            .iter()
            .enumerate()
            .filter(|(_, item)| item.status == StepStatus::InProgress)
            .map(|(index, item)| (index, lineage.step_id(&item.step), quoted_step(&item.step)))
            .collect::<Vec<_>>();
        if active.len() > 1 {
            // Never infer that the earlier step finished: completion is the
            // caller's claim. Name the conflict and the exact repair instead.
            return Err(in_progress_conflict(&active, &activated, by_step_id));
        }
        if explanation.is_some() {
            next.explanation = explanation;
        }
        Ok(next)
    }

    fn commit(current: &mut PlanState, next: UpdatePlanArgs) -> PlanStoreUpdate {
        let effect = match current.plan.as_ref() {
            None => PlanUpdateEffect::Initial,
            Some(previous) if previous == &next => PlanUpdateEffect::NoOp,
            Some(previous) if same_structure(previous, &next) => PlanUpdateEffect::StatusOnly,
            Some(_) => PlanUpdateEffect::StructuralRevision,
        };
        if effect != PlanUpdateEffect::NoOp {
            current.lineage.update_statuses(&next);
        }
        if effect != PlanUpdateEffect::NoOp {
            current.durably_published = false;
            current.derived = Default::default();
        }
        current.plan = Some(next.clone());
        PlanStoreUpdate {
            current: next,
            effect,
            lineage: current.lineage.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) async fn current_for_test(&self) -> Option<UpdatePlanArgs> {
        self.snapshot().await
    }
}

/// Whether two plans have the same steps in the same order, so that they differ
/// at most in statuses or explanation.
pub(crate) fn same_structure(left: &UpdatePlanArgs, right: &UpdatePlanArgs) -> bool {
    left.plan.len() == right.plan.len()
        && left
            .plan
            .iter()
            .zip(&right.plan)
            .all(|(left, right)| same_item_structure(left, right))
}

fn same_item_structure(left: &PlanItemArg, right: &PlanItemArg) -> bool {
    left.step == right.step
}

/// A plan revision must account for every unfinished step it removes: a new
/// step `continues` it, or `superseded` drops it with a reason. Rewording alone
/// never retires an obligation. Returns the explanation to store, recording
/// superseded steps in the committed update itself.
fn account_for_removed_steps(
    previous: Option<&UpdatePlanArgs>,
    lineage: &PlanLineage,
    next: &[PlanStepArg],
    superseded: &[SupersededStep],
    explanation: Option<String>,
) -> Result<Option<String>, String> {
    let previous_steps = previous
        .into_iter()
        .flat_map(|plan| &plan.plan)
        .map(|item| (lineage.step_id(&item.step), item))
        .collect::<Vec<_>>();
    let previous_by_id = previous_steps
        .iter()
        .map(|(id, item)| (id.as_str(), *item))
        .collect::<HashMap<_, _>>();
    let retained = next
        .iter()
        .map(|item| item.step.as_str())
        .collect::<HashSet<_>>();
    let mut continued = HashSet::new();
    for item in next {
        for id in &item.continues {
            if !previous_by_id.contains_key(id.as_str()) && lineage.orphan(id).is_none() {
                return Err(format!(
                    "step {} continues unknown step or unresolved orphan ID {id}; use step_ids or lineage from the previous result. No changes were made.",
                    quoted_step(&item.step)
                ));
            }
            continued.insert(id.as_str());
        }
    }
    let mut dropped = HashSet::new();
    let mut records = Vec::new();
    for entry in superseded {
        if let Some(orphan) = lineage.orphan(&entry.step_id) {
            let reason = entry.reason.trim();
            if reason.is_empty()
                || !dropped.insert(entry.step_id.as_str())
                || continued.contains(entry.step_id.as_str())
            {
                return Err("an orphan must be continued or superseded with one non-empty reason, not both; no changes were made".into());
            }
            records.push(format!(
                "Proposed supersession (unverified) {}: {reason}",
                quoted_step(&orphan.text)
            ));
            continue;
        }
        let Some(original) = previous_by_id.get(entry.step_id.as_str()) else {
            return Err(format!(
                "superseded names unknown step ID {}; no changes were made",
                entry.step_id
            ));
        };
        if original.status == StepStatus::Completed || retained.contains(original.step.as_str()) {
            return Err(format!(
                "superseded applies only to unfinished steps this revision removes, not {}; no changes were made",
                quoted_step(&original.step)
            ));
        }
        let reason = entry.reason.trim();
        if reason.is_empty() || !dropped.insert(entry.step_id.as_str()) {
            return Err(format!(
                "each superseded step needs exactly one non-empty reason; check {}. No changes were made.",
                entry.step_id
            ));
        }
        records.push(format!(
            "Proposed supersession (unverified) {}: {reason}",
            quoted_step(&original.step)
        ));
    }
    let unaccounted = previous_steps
        .iter()
        .filter(|(id, item)| {
            item.status != StepStatus::Completed
                && !retained.contains(item.step.as_str())
                && !continued.contains(id.as_str())
                && !dropped.contains(id.as_str())
        })
        .map(|(id, item)| format!("{id} {}", quoted_step(&item.step)))
        .collect::<Vec<_>>();
    if !unaccounted.is_empty() {
        return Err(format!(
            "every unfinished step a plan revision removes must be carried forward or superseded; no changes were made. Unaccounted: {}. Add its step ID to `continues` on the step that carries its remaining obligations, or list it in `superseded` with the user-authorized reason.",
            unaccounted.join("; ")
        ));
    }
    if records.is_empty() {
        return Ok(explanation);
    }
    let mut stored = explanation
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_default();
    for record in records {
        if !stored.is_empty() {
            stored.push('\n');
        }
        stored.push_str(&record);
    }
    Ok(Some(stored))
}

pub(crate) fn quoted_step(step: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut text = step.chars().take(MAX_CHARS).collect::<String>();
    if step.chars().nth(MAX_CHARS).is_some() {
        text.push_str("...");
    }
    format!("{text:?}")
}

/// Explains a status update that would leave several steps `in_progress`.
/// A bare rule statement sent callers into repeated rejected updates, so name
/// each active step and show the single-call repair.
fn in_progress_conflict(
    active: &[(usize, String, String)],
    activated: &HashSet<usize>,
    by_step_id: bool,
) -> String {
    let keep = active
        .iter()
        .map(|(index, _, _)| *index)
        .find(|index| activated.contains(index))
        .or_else(|| active.first().map(|(index, _, _)| *index));
    let mut steps = Vec::with_capacity(active.len());
    let mut repair = Vec::with_capacity(active.len());
    for (index, id, step) in active {
        let origin = if activated.contains(index) {
            "set by this call"
        } else {
            "already in_progress"
        };
        steps.push(format!("index {index} {step} (step_id {id}, {origin})"));
        let status = if Some(*index) == keep {
            "in_progress"
        } else if activated.contains(index) {
            "pending"
        } else {
            "completed"
        };
        let address = if by_step_id {
            format!("\"step_id\":\"{id}\"")
        } else {
            format!("\"index\":{index}")
        };
        repair.push(format!("{{{address},\"status\":\"{status}\"}}"));
    }
    format!(
        "update_plan permits at most one in_progress step at a time; no changes were made. In progress after this update: {}. In the same call, mark each other step completed if it is finished or pending to pause it, for example \"set\":[{}]",
        steps.join("; "),
        repair.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::plan_tool::StepStatus;

    async fn json_update(store: &PlanStore, mut args: serde_json::Value) -> PlanStoreUpdate {
        if let Some(snapshot) = store.execution_snapshot().await {
            args["expected_revision"] = snapshot.revision.into();
        }
        store
            .stage_tool(serde_json::from_value(args).unwrap())
            .await
            .unwrap()
            .commit_published()
    }

    #[tokio::test]
    async fn continuity_plan_cache_tracks_commit_noop_abort_and_lineage_only_changes() {
        use crate::context::ContextualUserFragment;
        let store = PlanStore::default();
        let initial = json_update(
            &store,
            serde_json::json!({"plan":[{"step":"work","status":"pending"}]}),
        )
        .await;
        let id = initial.lineage.step_id("work");
        let first = store.task_state(false).await.body().into_owned();
        let cached = store.current.lock().await.derived.clone();
        json_update(
            &store,
            serde_json::json!({"set":[{"step_id":id,"status":"pending"}]}),
        )
        .await;
        assert!(std::sync::Arc::ptr_eq(
            &cached,
            &store.current.lock().await.derived
        ));
        let aborted = store
            .stage_tool(
                serde_json::from_value(serde_json::json!({
                    "set":[{"step_id":id,"status":"completed"}]
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        drop(aborted);
        assert_eq!(store.task_state(false).await.body(), first);
        json_update(
            &store,
            serde_json::json!({"set":[{"step_id":id,"status":"completed"}]}),
        )
        .await;
        assert_ne!(store.task_state(false).await.body(), first);
        let revision = store.execution_snapshot().await.unwrap().revision;
        json_update(
            &store,
            serde_json::json!({"set":[{"step_id":id,"status":"completed"}],"resolve":[id]}),
        )
        .await;
        let next = store.execution_snapshot().await.unwrap().revision;
        assert_ne!(next, revision);
        let rendered = store.task_state(false).await.body().into_owned();
        assert!(rendered.contains(&next));
        assert!(!std::sync::Arc::ptr_eq(
            &cached,
            &store.current.lock().await.derived
        ));
    }

    #[tokio::test]
    async fn introduced_scope_survives_reverting_a_continuation() {
        let store = PlanStore::default();
        let original = json_update(
            &store,
            serde_json::json!({"plan":[{"step":"Inspect A","status":"pending"}]}),
        )
        .await;
        let id = original.lineage.step_id("Inspect A");
        let expanded = json_update(&store, serde_json::json!({"plan":[{"step":"Inspect A and B","status":"pending","continues":[id]}]})).await;
        assert_eq!(expanded.lineage.requirements.len(), 2);
        let reverted = json_update(&store, serde_json::json!({"plan":[{"step":"Inspect A","status":"completed","continues":[id]}]})).await;
        let outstanding = reverted
            .lineage
            .obligation_summary(&reverted.current)
            .unresolved;
        assert_eq!(outstanding, vec![plan_step_id("Inspect A and B")]);
        assert_eq!(
            reverted.lineage.requirements[&outstanding[0]].text,
            "Inspect A and B"
        );
        store
            .restore_with_lineage(
                Some(reverted.current.clone()),
                Some(reverted.lineage.clone()),
            )
            .await;
        assert_eq!(
            store.snapshot_with_lineage().await.unwrap(),
            (reverted.current, reverted.lineage)
        );
        assert_eq!(
            store
                .execution_snapshot()
                .await
                .unwrap()
                .obligations
                .unresolved,
            outstanding
        );
    }

    #[tokio::test]
    async fn audit32_lineage_only_descendant_reopens_requirement_before_publication() {
        for status in [StepStatus::Pending, StepStatus::InProgress] {
            for resolved in [false, true] {
                let store = PlanStore::default();
                let a = plan_step_id("A");
                let mut initial = serde_json::json!({"plan":[
                    {"step":"A","status":"completed"},
                    {"step":"B","status":status}
                ]});
                if resolved {
                    initial["resolve"] = serde_json::json!([a]);
                }
                json_update(&store, initial.clone()).await;
                let before = store.execution_snapshot().await;
                let noop = json_update(&store, initial).await;
                assert_eq!(noop.effect, PlanUpdateEffect::NoOp);
                assert_eq!(store.execution_snapshot().await, before);
                let args = serde_json::json!({"plan":[
                    {"step":"A","status":"completed"},
                    {"step":"B","status":status,"continues":[a]}
                ]});
                let revision = before.unwrap().revision;
                let mut staged_args = args.clone();
                staged_args["expected_revision"] = revision.into();
                let staged = store
                    .stage_tool(serde_json::from_value(staged_args).unwrap())
                    .await
                    .unwrap();
                assert!(staged.needs_publication());
                assert!(!staged.update.lineage.resolved_requirements.contains(&a));
                let expected_status = if status == StepStatus::InProgress {
                    StepStatus::InProgress
                } else {
                    StepStatus::Pending
                };
                assert_eq!(
                    staged.update.lineage.requirements[&a].status,
                    expected_status
                );
                let update = staged.commit_published();
                assert_eq!(update.effect, PlanUpdateEffect::StructuralRevision);
                let canonical = store.snapshot_with_lineage().await.unwrap();
                let before_restore = store.execution_snapshot().await;
                store
                    .restore_with_lineage(Some(update.current), Some(update.lineage))
                    .await;
                assert_eq!(store.snapshot_with_lineage().await.unwrap(), canonical);
                assert_eq!(store.execution_snapshot().await, before_restore);
                let noop = store
                    .stage_tool({
                        let mut value = args;
                        value["expected_revision"] = before_restore.unwrap().revision.into();
                        serde_json::from_value(value).unwrap()
                    })
                    .await
                    .unwrap();
                // Restored state needs publication once, even though meaning is unchanged.
                assert_eq!(noop.update.effect, PlanUpdateEffect::NoOp);
                noop.commit_published();
            }
        }
    }

    #[tokio::test]
    async fn explicit_original_resolution_survives_restore_and_reopening_invalidates_it() {
        let store = PlanStore::default();
        let original = json_update(
            &store,
            serde_json::json!({"plan":[{"step":"Verify the parser","status":"pending"}]}),
        )
        .await;
        let id = original.lineage.step_id("Verify the parser");
        let done = json_update(&store, serde_json::json!({
            "plan":[{"step":"Verify parser","status":"completed","continues":[id]}], "resolve":[id]
        })).await;
        assert!(
            done.lineage
                .obligation_summary(&done.current)
                .unresolved
                .is_empty()
        );
        assert_eq!(done.lineage.requirements[&id].text, "Verify the parser");
        store
            .restore_with_lineage(
                Some(done.current.clone()),
                Some(done.lineage.compact_for_plan(&done.current)),
            )
            .await;
        assert_eq!(
            store.snapshot_with_lineage().await.unwrap(),
            (done.current, done.lineage)
        );
        let reopened = json_update(
            &store,
            serde_json::json!({"set":[{"step_id":id,"status":"pending"}]}),
        )
        .await;
        assert!(!reopened.lineage.resolved_requirements.contains(&id));
        assert_eq!(
            reopened
                .lineage
                .obligation_summary(&reopened.current)
                .unresolved
                .len(),
            2
        );
        let before = store.execution_snapshot().await;
        let error = store
            .update_tool(
                serde_json::from_value(serde_json::json!({
                    "set":[{"step_id":id,"status":"pending"}], "resolve":[id]
                }))
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(error.contains("descendant"));
        assert_eq!(store.execution_snapshot().await, before);
    }

    #[tokio::test]
    async fn reattached_retirement_proposal_is_restore_idempotent() {
        let store = PlanStore::default();
        let original = json_update(
            &store,
            serde_json::json!({"plan":[{"step":"Deploy","status":"pending"}]}),
        )
        .await;
        let id = original.lineage.step_id("Deploy");
        json_update(&store, serde_json::json!({"plan":[],"superseded":[{"step_id":id,"reason":"unverified cancellation"}]})).await;
        let attached = json_update(
            &store,
            serde_json::json!({"plan":[{"step":"Deploy","status":"pending","continues":[id]}]}),
        )
        .await;
        let before = store.execution_snapshot().await;
        assert_eq!(before.as_ref().unwrap().obligations.unresolved.len(), 1);
        for _ in 0..3 {
            let (plan, lineage) = store.snapshot_with_lineage().await.unwrap();
            store
                .restore_with_lineage(Some(plan.clone()), Some(lineage.compact_for_plan(&plan)))
                .await;
            assert_eq!(store.execution_snapshot().await, before);
            assert_eq!(
                store.snapshot_with_lineage().await.unwrap(),
                (attached.current.clone(), attached.lineage.clone())
            );
        }
    }

    #[tokio::test]
    async fn scope_retirement_requires_real_accepted_input_and_preserves_provenance() {
        let store = PlanStore::default();
        let original = json_update(
            &store,
            serde_json::json!({"plan":[{"step":"Deploy","status":"pending"}]}),
        )
        .await;
        let id = original.lineage.step_id("Deploy");
        let args = serde_json::json!({"plan":[],"expected_revision":store.execution_snapshot().await.unwrap().revision,
            "superseded":[{"step_id":id,"reason":"User cancelled deployment"}],
            "scope_change_instructions":{id.clone():"Do not deploy; review only."}});
        let before = store.execution_snapshot().await;
        assert!(
            store
                .update_tool(serde_json::from_value(args.clone()).unwrap())
                .await
                .is_err()
        );
        assert_eq!(store.execution_snapshot().await, before);
        let before_lineage = store.snapshot_with_lineage().await;
        store
            .record_accepted_input("unrelated-turn", ["Continue with deployment."].into_iter())
            .await;
        assert!(
            store
                .update_tool(serde_json::from_value(args.clone()).unwrap())
                .await
                .is_err()
        );
        assert_eq!(store.execution_snapshot().await, before);
        assert_eq!(store.snapshot_with_lineage().await, before_lineage);
        store
            .record_accepted_input("older-turn", ["Do not deploy; review only."].into_iter())
            .await;
        store
            .record_accepted_input("current-turn", ["Continue with deployment."].into_iter())
            .await;
        assert!(
            store
                .update_tool(serde_json::from_value(args.clone()).unwrap())
                .await
                .is_err()
        );
        assert_eq!(store.execution_snapshot().await, before);
        assert_eq!(store.snapshot_with_lineage().await, before_lineage);
        store
            .record_accepted_input("user-turn", ["Do not deploy; review only."].into_iter())
            .await;
        let update = store
            .update_tool(serde_json::from_value(args).unwrap())
            .await
            .unwrap();
        assert_eq!(
            update
                .lineage
                .obligation_summary(&update.current)
                .superseded,
            1
        );
        assert!(
            update
                .lineage
                .obligation_summary(&update.current)
                .unresolved
                .is_empty()
        );
        assert_eq!(
            update.lineage.accepted_supersessions[&id].turn_id,
            "user-turn"
        );
        assert!(
            !update
                .lineage
                .active_for_plan(&update.current)
                .requirements
                .contains_key(&id)
        );
        store
            .restore_with_lineage(Some(update.current.clone()), Some(update.lineage.clone()))
            .await;
        assert_eq!(
            store.snapshot_with_lineage().await.unwrap(),
            (update.current, update.lineage)
        );
        assert_eq!(
            store
                .execution_snapshot()
                .await
                .unwrap()
                .obligations
                .superseded,
            1
        );
    }

    #[tokio::test]
    async fn accepted_scope_references_match_text_and_reject_replaced_or_foreign_inputs() {
        use crate::context::ContextualUserFragment;

        let text = format!(
            "Cancel deployment, preserve review. λ\n{}",
            "scope detail ".repeat(1000)
        );
        let reference = serde_json::json!({"turn_id":"accepted", "sha256":hex::encode(Sha256::digest(text.as_bytes()))});
        assert!(serde_json::to_vec(&reference).unwrap().len() < 256);
        assert!(text.len() > 10000);
        let mut outcomes = Vec::new();
        for use_reference in [false, true] {
            let store = PlanStore::default();
            store
                .record_accepted_input("accepted", [text.as_str()].into_iter())
                .await;
            assert!(
                !store
                    .task_state(false)
                    .await
                    .body()
                    .contains("accepted_input_refs")
            );
            let original = json_update(
                &store,
                serde_json::json!({"plan":[{"step":"Deploy","status":"pending"}]}),
            )
            .await;
            let id = original.lineage.step_id("Deploy");
            let state = store.task_state(false).await.body().into_owned();
            assert!(state.contains("accepted_input_refs"));
            let mut args = serde_json::json!({"plan":[], "expected_revision":store.execution_snapshot().await.unwrap().revision,
                "superseded":[{"step_id":id, "reason":"User cancelled deployment"}]});
            args[if use_reference {
                "scope_change_refs"
            } else {
                "scope_change_instructions"
            }] = if use_reference {
                serde_json::json!({id.clone():reference.clone()})
            } else {
                serde_json::json!({id.clone():text.clone()})
            };
            let before = store.snapshot_with_lineage().await;
            let mut foreign = args.clone();
            foreign["scope_change_refs"] =
                serde_json::json!({id.clone():{"turn_id":"foreign","sha256":reference["sha256"]}});
            assert!(
                store
                    .update_tool(serde_json::from_value(foreign).unwrap())
                    .await
                    .is_err()
            );
            assert_eq!(store.snapshot_with_lineage().await, before);
            store
                .record_accepted_input("replacement", ["Keep deployment."].into_iter())
                .await;
            assert!(
                store
                    .update_tool(serde_json::from_value(args.clone()).unwrap())
                    .await
                    .is_err()
            );
            assert_eq!(store.snapshot_with_lineage().await, before);
            store
                .record_accepted_input("accepted", [text.as_str()].into_iter())
                .await;
            let update = store
                .update_tool(serde_json::from_value(args).unwrap())
                .await
                .unwrap();
            outcomes.push((update.current, update.lineage));
        }
        assert_eq!(outcomes[0], outcomes[1]);
    }

    #[tokio::test]
    async fn accepted_scope_keeps_separate_same_turn_messages_and_textless_delivery() {
        let store = PlanStore::default();
        let original = json_update(
            &store,
            serde_json::json!({"plan":[{"step":"Deploy","status":"pending"}]}),
        )
        .await;
        let id = original.lineage.step_id("Deploy");
        let cancellation = "Cancel deployment; keep the review.";
        store
            .record_accepted_input("turn", [cancellation].into_iter())
            .await;
        store
            .record_accepted_input("turn", ["Also update README."].into_iter())
            .await;
        let before_image = store
            .accepted_input
            .lock()
            .unwrap()
            .clone()
            .unwrap()
            .review_revision();
        store.record_accepted_input("turn", [""].into_iter()).await;
        let inputs = store.accepted_input.lock().unwrap().clone().unwrap();
        assert_eq!(inputs.references().len(), 2);
        assert_ne!(inputs.review_revision(), before_image);
        store.record_accepted_input("turn", [""].into_iter()).await;
        let after_second_image = store.accepted_input.lock().unwrap().clone().unwrap();
        assert_ne!(
            after_second_image.review_revision(),
            inputs.review_revision(),
            "even repeated textless steering invalidates an earlier interpretation"
        );
        assert!(
            store
                .stage_tool(
                    serde_json::from_value(serde_json::json!({"plan":[],
            "expected_revision":store.execution_snapshot().await.unwrap().revision,
            "superseded":[{"step_id":id,"reason":"User cancelled deployment"}],
            "scope_change_instructions":{id.clone():cancellation},
            "scope_change_reviewed_revision":before_image}))
                    .unwrap()
                )
                .await
                .is_err()
        );
        let update = json_update(
            &store,
            serde_json::json!({"plan":[],
            "superseded":[{"step_id":id,"reason":"User cancelled deployment"}],
            "scope_change_instructions":{id.clone():cancellation},
            "scope_change_reviewed_revision":after_second_image.review_revision()}),
        )
        .await;
        assert_eq!(update.lineage.accepted_supersessions[&id].turn_id, "turn");
        store
            .record_accepted_input("next-turn", ["Keep deployment."].into_iter())
            .await;
        let inputs = store.accepted_input.lock().unwrap().clone().unwrap();
        assert_eq!(inputs.0, "next-turn");
        assert!(
            !inputs
                .1
                .contains(&hex::encode(Sha256::digest(cancellation.as_bytes())))
        );
    }

    #[tokio::test]
    async fn accepted_input_waits_for_staged_publication_without_lock_inversion() {
        let store = PlanStore::default();
        json_update(
            &store,
            serde_json::json!({"plan":[{"step":"Review","status":"pending"}]}),
        )
        .await;
        store
            .record_accepted_input("old", ["old input"].into_iter())
            .await;
        let revision = store.execution_snapshot().await.unwrap().revision;
        let staged = store
            .stage_tool(
                serde_json::from_value(serde_json::json!({
                    "expected_revision": revision, "set":[{"index":0,"status":"completed"}]
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let accepted = store.record_accepted_input("new", ["new input"].into_iter());
        tokio::pin!(accepted);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut accepted)
                .await
                .is_err()
        );
        assert_eq!(
            store.accepted_input.lock().unwrap().as_ref().unwrap().0,
            "old"
        );
        drop(staged);
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut accepted)
            .await
            .unwrap();
        assert_eq!(
            store.accepted_input.lock().unwrap().as_ref().unwrap().0,
            "new"
        );
    }

    #[tokio::test]
    async fn second_in_progress_step_is_rejected_with_an_applicable_repair() {
        // Shape of real rejected calls: step 0 is still in_progress when the
        // caller starts step 1 without finishing it in the same `set`.
        for by_step_id in [false, true] {
            let store = PlanStore::default();
            let initial = json_update(
                &store,
                serde_json::json!({"plan":[
                    {"step":"Inspect","status":"in_progress"},
                    {"step":"Fix","status":"pending"},
                    {"step":"Verify","status":"pending"}
                ]}),
            )
            .await;
            let inspect = initial.lineage.step_id("Inspect");
            let fix = initial.lineage.step_id("Fix");
            let start_fix = if by_step_id {
                serde_json::json!({"step_id":fix,"status":"in_progress"})
            } else {
                serde_json::json!({"index":1,"status":"in_progress"})
            };
            let before = store.current_for_test().await;
            let revision = store.execution_snapshot().await.unwrap().revision;
            let error = store
                .update_tool(
                    serde_json::from_value(serde_json::json!({
                        "expected_revision": revision, "set":[start_fix]
                    }))
                    .unwrap(),
                )
                .await
                .err()
                .expect("a second in_progress step must be rejected");
            assert!(error.contains("at most one in_progress"), "{error}");
            assert!(error.contains("no changes were made"), "{error}");
            assert!(
                error.contains(&format!(
                    "index 0 \"Inspect\" (step_id {inspect}, already in_progress)"
                )),
                "{error}"
            );
            assert!(
                error.contains(&format!(
                    "index 1 \"Fix\" (step_id {fix}, set by this call)"
                )),
                "{error}"
            );
            assert_eq!(store.current_for_test().await, before);
            assert_eq!(store.execution_snapshot().await.unwrap().revision, revision);

            // The example uses the caller's addressing and applies as sent.
            let (_, example) = error.split_once("for example ").expect("repair example");
            let mut repair: serde_json::Value =
                serde_json::from_str(&format!("{{{example}}}")).expect("example is JSON");
            assert_eq!(repair["set"][0].get("step_id").is_some(), by_step_id);
            repair["expected_revision"] = revision.into();
            store
                .update_tool(serde_json::from_value(repair).unwrap())
                .await
                .expect("the example repair applies");
            let statuses = store
                .current_for_test()
                .await
                .expect("plan")
                .plan
                .into_iter()
                .map(|item| item.status)
                .collect::<Vec<_>>();
            assert_eq!(
                statuses,
                [
                    StepStatus::Completed,
                    StepStatus::InProgress,
                    StepStatus::Pending
                ]
            );
        }
    }

    #[tokio::test]
    async fn step_cas_rejects_aba_but_allows_independent_peer_deltas() {
        let store = PlanStore::default();
        let initial = json_update(
            &store,
            serde_json::json!({"plan":[
                {"step":"A","status":"pending"},{"step":"B","status":"pending"}
            ]}),
        )
        .await;
        let a = initial.lineage.step_id("A");
        let b = initial.lineage.step_id("B");
        for id in [&a, &b] {
            store.update_tool(serde_json::from_value(serde_json::json!({
                "set":[{"step_id":id,"status":"completed"}], "expected_step_revisions":{id:0}
            })).unwrap()).await.unwrap();
        }
        json_update(
            &store,
            serde_json::json!({"set":[{"step_id":a,"status":"pending"}]}),
        )
        .await;
        let before = store.execution_snapshot().await;
        for versions in [None, Some(serde_json::json!({a.clone():0}))] {
            let mut args = serde_json::json!({"set":[{"step_id":a,"status":"completed"}]});
            if let Some(versions) = versions {
                args["expected_step_revisions"] = versions;
            }
            assert!(
                store
                    .update_tool(serde_json::from_value(args).unwrap())
                    .await
                    .unwrap_err()
                    .contains("step revision")
            );
            assert_eq!(store.execution_snapshot().await, before);
        }
        let current = store.snapshot_with_lineage().await.unwrap();
        store
            .restore_with_lineage(Some(current.0), Some(current.1.clone()))
            .await;
        let good = serde_json::json!({"set":[{"step_id":a,"status":"completed"}],
            "expected_step_revisions":{a.clone():current.1.step_revisions[&a]}});
        assert!(
            store
                .update_tool(serde_json::from_value(good).unwrap())
                .await
                .unwrap()
                .lineage
                .obligation_summary(store.snapshot().await.as_ref().unwrap())
                .unresolved
                .is_empty()
        );
    }

    #[tokio::test]
    async fn noop_skips_state_rebuild_but_restored_state_still_needs_publication() {
        let store = PlanStore::default();
        json_update(
            &store,
            serde_json::json!({"plan":[{"step":"A","status":"pending"}]}),
        )
        .await;
        let args = || {
            serde_json::from_value(
                serde_json::json!({"set":[{"step_id":plan_step_id("A"),"status":"pending"}]}),
            )
            .unwrap()
        };
        let before = store.snapshot_with_lineage().await.unwrap();
        let staged = store.stage_tool(args()).await.unwrap();
        assert!(staged.next.is_none());
        assert!(!staged.needs_publication());
        staged.commit();
        store
            .restore_with_lineage(Some(before.0.clone()), Some(before.1.clone()))
            .await;
        let staged = store.stage_tool(args()).await.unwrap();
        assert!(staged.next.is_none());
        assert!(staged.needs_publication());
        staged.commit_published();
        assert_eq!(store.snapshot_with_lineage().await.unwrap(), before);
    }

    fn plan(step: &str, status: StepStatus) -> UpdatePlanArgs {
        UpdatePlanArgs {
            explanation: None,
            plan: vec![PlanItemArg {
                step: step.to_string(),
                status,
            }],
        }
    }

    async fn replace(store: &PlanStore, plan: Vec<PlanStepArg>) -> PlanStoreUpdate {
        let expected_revision = store
            .execution_snapshot()
            .await
            .map(|snapshot| snapshot.revision);
        store
            .update_tool(PlanToolArgs {
                plan: Some(plan),
                expected_revision,
                ..Default::default()
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn sampling_fence_rejects_concurrent_replacements_and_restore() {
        let store = PlanStore::default();
        let original = plan("work", StepStatus::Pending);
        store.update(original.clone()).await;
        let sampled = store.sampling_revision().await;
        let args = |status| PlanToolArgs {
            sampling_revision: Some(sampled.clone()),
            plan: Some(vec![PlanStepArg {
                step: "work".into(),
                status,
                continues: vec![],
            }]),
            ..Default::default()
        };
        // Dropping a staged write (for example failed persistence) changes no fence.
        drop(
            store
                .stage_tool(args(StepStatus::InProgress))
                .await
                .unwrap(),
        );
        let (left, right) = tokio::join!(
            store.update_tool(args(StepStatus::InProgress)),
            store.update_tool(args(StepStatus::Completed)),
        );
        assert_ne!(
            left.is_ok(),
            right.is_ok(),
            "only one sampled replacement commits"
        );
        store.restore(Some(original)).await;
        assert!(
            store
                .update_tool(args(StepStatus::Completed))
                .await
                .is_err(),
            "restoring identical content cannot revive an old fence"
        );
        let mut fresh = args(StepStatus::Completed);
        fresh.sampling_revision = Some(store.sampling_revision().await);
        store.update_tool(fresh).await.unwrap();
        assert!(
            serde_json::from_value::<PlanToolArgs>(serde_json::json!({
                "plan": [], "sampling_revision": "forged"
            }))
            .is_err(),
            "sampling fences cannot be supplied by the model"
        );
    }

    #[tokio::test]
    async fn retired_step_identity_cannot_complete_reintroduced_work() {
        let store = PlanStore::default();
        let step = |status| PlanStepArg {
            step: "same work".into(),
            status,
            continues: vec![],
        };
        let original = replace(&store, vec![step(StepStatus::Completed)]).await;
        let old_id = original.lineage.step_id("same work");
        replace(&store, vec![]).await;
        let recreated = replace(&store, vec![step(StepStatus::Pending)]).await;
        let new_id = recreated.lineage.step_id("same work");
        assert_ne!(new_id, old_id);
        assert!(
            store
                .update_tool(PlanToolArgs {
                    set: Some(vec![PlanStatusUpdate {
                        index: None,
                        step_id: Some(old_id.clone()),
                        status: StepStatus::Completed
                    }]),
                    ..Default::default()
                })
                .await
                .is_err()
        );
        assert_eq!(store.snapshot().await, Some(recreated.current.clone()));
        assert_eq!(
            recreated.lineage.requirements[&old_id].status,
            StepStatus::Completed
        );
        store
            .restore_with_lineage(Some(recreated.current), Some(recreated.lineage))
            .await;
        assert_eq!(
            store
                .snapshot_with_lineage()
                .await
                .unwrap()
                .1
                .step_id("same work"),
            new_id
        );
    }

    #[tokio::test]
    async fn replacement_conflicts_return_reconciliation_without_reopening_work() {
        let store = PlanStore::default();
        let original = plan("work", StepStatus::Pending);
        store.update(original.clone()).await;
        let stale = store.execution_snapshot().await.unwrap().revision;
        store
            .update_tool(PlanToolArgs {
                set: Some(vec![PlanStatusUpdate {
                    index: None,
                    step_id: Some(plan_step_id("work")),
                    status: StepStatus::Completed,
                }]),
                ..Default::default()
            })
            .await
            .unwrap();
        let current = store.execution_snapshot().await.unwrap();
        for expected_revision in [None, Some(stale)] {
            let error = store
                .update_tool(PlanToolArgs {
                    expected_revision,
                    plan: Some(vec![PlanStepArg {
                        step: "work".into(),
                        status: StepStatus::Pending,
                        continues: vec![],
                    }]),
                    ..Default::default()
                })
                .await
                .unwrap_err();
            let reconciliation: serde_json::Value =
                serde_json::from_str(error.split_once("Reconcile against: ").unwrap().1).unwrap();
            assert_eq!(reconciliation["revision"], current.revision);
            assert_eq!(
                reconciliation["current_plan"]["plan"][0]["status"],
                "completed"
            );
            assert_eq!(reconciliation["step_ids"][0], plan_step_id("work"));
            assert_eq!(store.execution_snapshot().await, Some(current.clone()));
        }
    }

    #[tokio::test]
    async fn rename_and_completion_are_atomic_and_keep_original_obligations() {
        let store = PlanStore::default();
        store.update(plan("original", StepStatus::Pending)).await;
        let id = plan_step_id("original");
        let update = replace(
            &store,
            vec![PlanStepArg {
                step: "finished work".into(),
                status: StepStatus::Completed,
                continues: vec![id.clone()],
            }],
        )
        .await;
        assert_eq!(update.lineage.step_id("finished work"), id);
        assert_eq!(update.lineage.requirements[&id].text, "original");
        assert_eq!(update.current.plan[0].status, StepStatus::Completed);
        assert_eq!(
            update
                .lineage
                .obligation_summary(&update.current)
                .unresolved,
            vec![id.clone()]
        );

        let two_calls = PlanStore::default();
        two_calls
            .update(plan("original", StepStatus::Pending))
            .await;
        replace(
            &two_calls,
            vec![PlanStepArg {
                step: "finished work".into(),
                status: StepStatus::InProgress,
                continues: vec![id.clone()],
            }],
        )
        .await;
        let second = two_calls
            .update_tool(PlanToolArgs {
                expected_revision: Some(two_calls.execution_snapshot().await.unwrap().revision),
                set: Some(vec![PlanStatusUpdate {
                    index: None,
                    step_id: Some(id.clone()),
                    status: StepStatus::Completed,
                }]),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(second.lineage.step_revisions[&id], 2);
        assert_eq!(update.lineage.step_revisions[&id], 1);
        let mut second_lineage = second.lineage;
        second_lineage.step_revisions = update.lineage.step_revisions.clone();
        assert_eq!(
            second_lineage, update.lineage,
            "a second call adds no authority"
        );
        let restored = update
            .lineage
            .compact_for_plan(&update.current)
            .reconcile_restored(Some(&update.current));
        assert_eq!(restored.requirements[&id].status, StepStatus::InProgress);
        assert!(
            restored
                .active_for_plan(&update.current)
                .requirements
                .contains_key(&id)
        );
    }

    #[tokio::test]
    async fn active_projection_does_not_grow_with_retired_history_and_restore_is_exact() {
        let store = PlanStore::default();
        for index in 0..40 {
            replace(
                &store,
                vec![PlanStepArg {
                    step: format!("finished {index}"),
                    status: StepStatus::Completed,
                    continues: vec![],
                }],
            )
            .await;
        }
        let update = replace(
            &store,
            vec![PlanStepArg {
                step: "current".into(),
                status: StepStatus::Pending,
                continues: vec![],
            }],
        )
        .await;
        assert_eq!(update.lineage.requirements.len(), 41);
        let active = update.lineage.active_for_plan(&update.current);
        assert_eq!(active.requirements.len(), 1);
        let durable = update.lineage.compact_for_plan(&update.current);
        let restored = durable.reconcile_restored(Some(&update.current));
        assert_eq!(restored, update.lineage);
        assert_eq!(
            active.obligation_summary(&update.current).unresolved,
            restored.obligation_summary(&update.current).unresolved
        );
    }

    #[tokio::test]
    async fn status_step_prefixes_are_unique_and_atomic() {
        let store = PlanStore::default();
        let original = plan("inspect", StepStatus::Pending);
        store.update(original.clone()).await;
        let id = plan_step_id("inspect");
        let update = |ids: Vec<String>| PlanToolArgs {
            set: Some(
                ids.into_iter()
                    .map(|step_id| PlanStatusUpdate {
                        index: None,
                        step_id: Some(step_id),
                        status: StepStatus::Completed,
                    })
                    .collect(),
            ),
            ..Default::default()
        };
        for bad in [id[..7].to_string(), "not-a-step".to_string()] {
            let error = store
                .update_tool(update(vec![id[..8].to_string(), bad]))
                .await
                .unwrap_err();
            assert!(error.contains(&id));
            assert!(error.contains("inspect"));
            assert_eq!(store.snapshot().await, Some(original.clone()));
        }
        assert!(
            store
                .update_tool(update(vec![id.clone(), id[..8].to_string()]))
                .await
                .is_err()
        );
        assert_eq!(store.snapshot().await, Some(original));
        let result = store
            .update_tool(update(vec![id[..8].to_string()]))
            .await
            .unwrap();
        assert_eq!(result.current.plan[0].status, StepStatus::Completed);

        let current = UpdatePlanArgs {
            explanation: None,
            plan: vec![
                PlanItemArg {
                    step: "one".into(),
                    status: StepStatus::Pending,
                },
                PlanItemArg {
                    step: "two".into(),
                    status: StepStatus::Pending,
                },
            ],
        };
        let mut lineage = PlanLineage::default();
        lineage
            .step_identities
            .insert(plan_step_id("one"), "12345678aaaa".into());
        lineage
            .step_identities
            .insert(plan_step_id("two"), "12345678bbbb".into());
        let error = PlanStore::status_plan(
            &Some(current),
            &lineage,
            update(vec!["12345678".into()]).set.unwrap(),
            None,
            false,
            None,
            "revision",
        )
        .unwrap_err();
        assert!(error.contains("12345678aaaa: \"one\""));
        assert!(error.contains("12345678bbbb: \"two\""));
    }

    #[test]
    fn compact_lineage_and_short_ids_preserve_resume_and_legacy_identity() {
        let current = plan("inspect", StepStatus::Pending);
        let full = PlanLineage::from_plan(Some(&current));
        assert_eq!(full.step_id("inspect").len(), 16);
        let compact = full.compact_for_plan(&current);
        assert!(compact.is_empty());
        assert_eq!(compact.reconcile_restored(Some(&current)), full);

        let legacy_id = hex::encode(Sha256::digest(b"inspect"));
        let mut legacy = PlanLineage::default();
        legacy.requirements.insert(
            legacy_id.clone(),
            PlanRequirement {
                text: "inspect".into(),
                status: StepStatus::Pending,
                superseded_reason: None,
            },
        );
        legacy
            .step_requirements
            .insert(legacy_id.clone(), vec![legacy_id.clone()]);
        let restored = legacy.reconcile_restored(Some(&current));
        assert_eq!(restored.step_id("inspect"), legacy_id);
        assert_eq!(restored.requirements.len(), 1);
        assert!(!restored.compact_for_plan(&current).is_empty());
    }

    #[tokio::test]
    async fn stale_revision_is_rejected_and_step_ids_survive_reordering_and_restore() {
        let store = PlanStore::default();
        let original = UpdatePlanArgs {
            explanation: None,
            plan: vec![
                PlanItemArg {
                    step: "inspect".into(),
                    status: StepStatus::Pending,
                },
                PlanItemArg {
                    step: "verify".into(),
                    status: StepStatus::Pending,
                },
            ],
        };
        store.update(original.clone()).await;
        let revision = plan_revision(Some(&original));
        let mut reordered = original.clone();
        reordered.plan.reverse();
        store.update(reordered.clone()).await;
        let updates = || {
            Some(vec![PlanStatusUpdate {
                index: None,
                step_id: Some(plan_step_id("inspect")),
                status: StepStatus::Completed,
            }])
        };
        let set = |set, expected_revision: Option<&str>| PlanToolArgs {
            set,
            expected_revision: expected_revision.map(str::to_string),
            ..Default::default()
        };
        assert!(
            store
                .update_tool(set(updates(), Some(&revision)))
                .await
                .is_err()
        );
        assert_eq!(store.snapshot().await, Some(reordered.clone()));
        // Index 0 meant "inspect" in the original plan but "verify" after the
        // reorder; without a revision the host cannot tell which was meant.
        let first_by_index = || {
            Some(vec![PlanStatusUpdate {
                index: Some(0),
                step_id: None,
                status: StepStatus::Completed,
            }])
        };
        assert!(
            store
                .update_tool(set(first_by_index(), None))
                .await
                .is_err()
        );
        assert_eq!(store.snapshot().await, Some(reordered.clone()));
        let bound = store
            .update_tool(set(
                first_by_index(),
                Some(&plan_revision(Some(&reordered))),
            ))
            .await
            .expect("revision-bound index update");
        assert_eq!(bound.current.plan[0].step, "verify");
        assert_eq!(bound.current.plan[0].status, StepStatus::Completed);
        let resumed = PlanStore::default();
        resumed.restore(Some(reordered.clone())).await;
        let result = resumed
            .update_tool(set(updates(), Some(&plan_revision(Some(&reordered)))))
            .await
            .unwrap();
        assert_eq!(result.current.plan[0].status, StepStatus::Pending);
        assert_eq!(result.current.plan[1].status, StepStatus::Completed);
    }

    #[test]
    fn completion_summary_separates_resolved_unresolved_and_superseded_requirements() {
        let current = plan("required check", StepStatus::Pending);
        let mut lineage = PlanLineage::from_plan(Some(&current));
        lineage.requirements.insert(
            "completed".into(),
            PlanRequirement {
                text: "verified implementation".into(),
                status: StepStatus::Completed,
                superseded_reason: None,
            },
        );
        lineage.requirements.insert(
            "retired".into(),
            PlanRequirement {
                text: "user removed scope".into(),
                status: StepStatus::Pending,
                superseded_reason: Some("explicit correction".into()),
            },
        );
        let summary = lineage.obligation_summary(&current);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.superseded, 0);
        assert_eq!(
            summary.unresolved,
            vec![plan_step_id("required check"), "retired".into()]
        );
        assert_eq!(
            PlanLineage::default()
                .obligation_summary(&current)
                .unresolved,
            vec![plan_step_id("required check")],
            "legacy checklist is not an empty obligation set"
        );
        lineage.update_statuses(&plan("required check", StepStatus::Completed));
        let summary = lineage.obligation_summary(&current);
        assert_eq!(summary.completed, 2);
        assert_eq!(summary.unresolved, vec!["retired"]);
        assert!(
            lineage
                .active_for_plan(&current)
                .requirements
                .contains_key("retired")
        );
    }

    #[tokio::test]
    async fn completion_summary_is_recompiled_after_restore_not_trusted_as_evidence() {
        let current = plan("still required", StepStatus::Pending);
        let response = serde_json::json!({
            "current_plan": current,
            "obligations": {"completed": 999, "superseded": 0, "unresolved": []}
        });
        let parsed: PlanToolResponse = serde_json::from_value(response).unwrap();
        let store = PlanStore::default();
        store
            .restore_with_lineage(Some(parsed.current_plan), Some(parsed.lineage))
            .await;
        let (restored, lineage) = store.snapshot_with_lineage().await.unwrap();
        let summary = lineage.obligation_summary(&restored);
        assert_eq!(summary.completed, 0);
        assert_eq!(summary.unresolved, vec![plan_step_id("still required")]);
    }

    #[tokio::test]
    async fn completion_audit_restores_partial_lineage_without_losing_obligations() {
        for mode in [
            "missing-step",
            "unknown-reference",
            "retired-reference",
            "stale-status",
        ] {
            for history_replay in [false, true] {
                let current = plan("verify current work", StepStatus::Pending);
                let id = plan_step_id("verify current work");
                let mut lineage =
                    PlanLineage::from_plan(Some(&plan("earlier work", StepStatus::Completed)));
                let earlier_id = plan_step_id("earlier work");
                lineage.requirements.insert(
                    "unmapped-open".into(),
                    PlanRequirement {
                        text: "original unresolved obligation".into(),
                        status: StepStatus::InProgress,
                        superseded_reason: None,
                    },
                );
                match mode {
                    "unknown-reference" => {
                        lineage
                            .step_requirements
                            .insert(id.clone(), vec!["absent".into()]);
                    }
                    "retired-reference" => {
                        lineage
                            .requirements
                            .get_mut(&earlier_id)
                            .unwrap()
                            .superseded_reason = Some("user removed scope".into());
                        lineage
                            .step_requirements
                            .insert(id.clone(), vec![earlier_id.clone()]);
                    }
                    "stale-status" => {
                        lineage.requirements.insert(
                            id.clone(),
                            PlanRequirement {
                                text: "verify current work".into(),
                                status: StepStatus::Completed,
                                superseded_reason: None,
                            },
                        );
                        lineage
                            .step_requirements
                            .insert(id.clone(), vec![id.clone()]);
                    }
                    _ => {}
                }
                let store = PlanStore::default();
                if history_replay {
                    let history = vec![
                        ResponseItem::FunctionCall {
                            id: None, name: "update_plan".into(), namespace: None,
                            arguments: "{}".into(), call_id: "restore".into(),
                            internal_chat_message_metadata_passthrough: None,
                        },
                        ResponseItem::FunctionCallOutput {
                            id: None, call_id: "restore".into(),
                            output: FunctionCallOutputPayload::from_text(serde_json::json!({
                                "current_plan": current, "lineage": lineage,
                                "obligations": {"completed": 999, "superseded": 0, "unresolved": []}
                            }).to_string()),
                            internal_chat_message_metadata_passthrough: None,
                        },
                    ];
                    assert!(store.restore_from_history(&history).await);
                } else {
                    store
                        .restore_with_lineage(Some(current.clone()), Some(lineage))
                        .await;
                }
                let (restored, repaired) = store.snapshot_with_lineage().await.unwrap();
                assert_eq!(restored, current);
                let unresolved = 2 + usize::from(mode == "retired-reference");
                assert_eq!(store.active_requirement_count().await, unresolved, "{mode}");
                assert_eq!(
                    repaired.obligation_summary(&restored).unresolved.len(),
                    unresolved
                );
                assert_eq!(
                    repaired.requirements["unmapped-open"].status,
                    StepStatus::InProgress
                );
                let revision = plan_revision_with_lineage(Some(&restored), &repaired);
                store
                    .restore_with_lineage(Some(restored.clone()), Some(repaired))
                    .await;
                let (_, again) = store.snapshot_with_lineage().await.unwrap();
                assert_eq!(
                    plan_revision_with_lineage(Some(&restored), &again),
                    revision,
                    "restore is idempotent"
                );
                let closed = store
                    .update_tool(PlanToolArgs {
                        set: Some(vec![PlanStatusUpdate {
                            index: None,
                            step_id: Some(id),
                            status: StepStatus::Completed,
                        }]),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                let outstanding = closed
                    .lineage
                    .obligation_summary(&closed.current)
                    .unresolved;
                assert!(outstanding.contains(&"unmapped-open".to_string()));
                assert_eq!(outstanding.len(), unresolved - 1);
                assert_eq!(store.active_requirement_count().await, unresolved - 1);
            }
        }
    }

    #[tokio::test]
    async fn unfinished_scope_cannot_disappear_without_an_accounted_revision() {
        let store = PlanStore::default();
        let review = "Review every warning against source";
        let original = UpdatePlanArgs {
            explanation: None,
            plan: vec![
                PlanItemArg {
                    step: review.into(),
                    status: StepStatus::InProgress,
                },
                PlanItemArg {
                    step: "Fix confirmed warnings".into(),
                    status: StepStatus::Pending,
                },
            ],
        };
        store.update(original.clone()).await;
        let review_id = plan_step_id(review);
        let fix_id = plan_step_id("Fix confirmed warnings");
        let step = |text: &str, status, continues: &[&str]| PlanStepArg {
            step: text.into(),
            status,
            continues: continues.iter().map(ToString::to_string).collect(),
        };
        let revise =
            |plan, superseded: Vec<(&str, &str)>, explanation: Option<&str>| PlanToolArgs {
                plan: Some(plan),
                expected_revision: Some(plan_revision(Some(&original))),
                superseded: superseded
                    .into_iter()
                    .map(|(step_id, reason)| SupersededStep {
                        step_id: step_id.into(),
                        reason: reason.into(),
                    })
                    .collect(),
                explanation: explanation.map(str::to_string),
                ..Default::default()
            };
        // A narrower rewording cannot retire obligations, even with a prose explanation.
        for rejected in [
            revise(
                vec![step("Inventory warnings", StepStatus::Completed, &[])],
                vec![],
                None,
            ),
            revise(
                vec![step("Inventory warnings", StepStatus::Completed, &[])],
                vec![],
                Some("The user requested only an inventory."),
            ),
            revise(
                vec![step(
                    "Inventory warnings",
                    StepStatus::InProgress,
                    &[review_id.as_str()],
                )],
                vec![(fix_id.as_str(), "  ")],
                None,
            ),
            revise(
                vec![step(
                    "Inventory warnings",
                    StepStatus::InProgress,
                    &["unknown"],
                )],
                vec![(fix_id.as_str(), "Dropped by the user.")],
                None,
            ),
        ] {
            assert!(store.update_tool(rejected).await.is_err());
            assert_eq!(store.snapshot().await, Some(original.clone()));
        }
        let revised = store
            .update_tool(revise(
                vec![step(
                    "Review warnings in core only",
                    StepStatus::InProgress,
                    &[review_id.as_str()],
                )],
                vec![(fix_id.as_str(), "The user will fix warnings separately.")],
                Some("Narrowed review scope."),
            ))
            .await
            .expect("every removed unfinished step is accounted");
        assert_eq!(revised.effect, PlanUpdateEffect::StructuralRevision);
        assert_eq!(
            revised.current.explanation.as_deref(),
            Some(
                "Narrowed review scope.\nProposed supersession (unverified) \"Fix confirmed warnings\": The user will fix warnings separately."
            )
        );
        assert_eq!(
            revised.lineage.step_id("Review warnings in core only"),
            review_id
        );
        let resumed = PlanStore::default();
        let lineage =
            serde_json::from_value(serde_json::to_value(&revised.lineage).unwrap()).unwrap();
        resumed
            .restore_with_lineage(Some(revised.current), Some(lineage))
            .await;
        let completed = resumed
            .update_tool(PlanToolArgs {
                expected_revision: Some(resumed.execution_snapshot().await.unwrap().revision),
                set: Some(vec![PlanStatusUpdate {
                    index: None,
                    step_id: Some(review_id.clone()),
                    status: StepStatus::Completed,
                }]),
                ..Default::default()
            })
            .await
            .expect("the pre-rename identity survives persistence");
        assert_eq!(completed.current.plan[0].status, StepStatus::Completed);
        assert_eq!(completed.lineage.requirements[&review_id].text, review);
        assert_eq!(
            completed.lineage.requirements[&review_id].status,
            StepStatus::InProgress
        );
        assert_eq!(
            completed
                .lineage
                .obligation_summary(&completed.current)
                .unresolved
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn classifies_straight_line_checklist_updates() {
        let store = PlanStore::default();
        for (step, status, explanation, effect) in [
            (
                "inspect",
                StepStatus::InProgress,
                "first explanation",
                PlanUpdateEffect::Initial,
            ),
            (
                "inspect",
                StepStatus::InProgress,
                "reworded explanation",
                PlanUpdateEffect::StatusOnly,
            ),
            (
                "inspect",
                StepStatus::Completed,
                "reworded explanation",
                PlanUpdateEffect::StatusOnly,
            ),
            (
                "inspect",
                StepStatus::Completed,
                "reworded explanation",
                PlanUpdateEffect::NoOp,
            ),
            (
                "implement",
                StepStatus::InProgress,
                "new work",
                PlanUpdateEffect::StructuralRevision,
            ),
        ] {
            let mut expected = plan(step, status);
            expected.explanation = Some(explanation.to_string());
            let updated = json_update(&store, serde_json::to_value(&expected).unwrap()).await;
            assert_eq!(updated.effect, effect);
            assert_eq!(updated.current, expected);
            assert_eq!(store.snapshot().await, Some(expected));
        }
    }

    #[tokio::test]
    async fn reconstructed_authoritative_plan_makes_identical_update_a_no_op() {
        let expected = plan("inspect", StepStatus::Completed);
        let history = vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "update_plan".to_string(),
                namespace: None,
                arguments: "{}".to_string(),
                call_id: "plan-call".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "plan-call".to_string(),
                output: FunctionCallOutputPayload::from_text(
                    serde_json::json!({"current_plan": expected.clone()}).to_string(),
                ),
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let store = PlanStore::default();

        assert!(store.restore_from_history(&history).await);
        assert_eq!(
            json_update(&store, serde_json::to_value(&expected).unwrap())
                .await
                .effect,
            PlanUpdateEffect::NoOp
        );
        assert_eq!(store.snapshot().await, Some(expected));
    }

    #[tokio::test]
    async fn namespaced_update_plan_cannot_restore_builtin_plan_state() {
        let mut history = vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "update_plan".into(),
                namespace: Some("vendor".into()),
                arguments: "{}".into(),
                call_id: "vendor".into(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "vendor".into(),
                output: FunctionCallOutputPayload::from_text(
                    serde_json::json!({"current_plan":plan("vendor", StepStatus::Pending)})
                        .to_string(),
                ),
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let store = PlanStore::default();
        assert!(!store.restore_from_history(&history).await);
        if let ResponseItem::FunctionCall { namespace, .. } = &mut history[0] {
            *namespace = None;
        }
        assert!(store.restore_from_history(&history).await);
    }

    #[tokio::test]
    async fn history_replay_ignores_invalid_unrelated_and_failed_plan_outputs() {
        let earlier = plan("earlier", StepStatus::InProgress);
        let expected = plan("accepted", StepStatus::Completed);
        let rejected = plan("rejected", StepStatus::Pending);
        for (name, output_text, success) in [
            ("update_plan", "{\"current_plan\":".to_string(), None),
            (
                "update_plan",
                "Plan update aborted by user".to_string(),
                None,
            ),
            (
                "update_plan",
                r#"{"current_plan":{"plan":"invalid"}}"#.to_string(),
                None,
            ),
            (
                "another_tool",
                serde_json::json!({"current_plan": rejected}).to_string(),
                None,
            ),
            (
                "update_plan",
                serde_json::json!({"current_plan": rejected}).to_string(),
                Some(false),
            ),
        ] {
            let mut history = Vec::new();
            for (call_id, tool_name, text, success) in [
                (
                    "earlier",
                    "update_plan",
                    serde_json::json!({"current_plan": earlier}).to_string(),
                    Some(true),
                ),
                (
                    "accepted",
                    "update_plan",
                    serde_json::json!({"current_plan": expected}).to_string(),
                    None,
                ),
                ("rejected", name, output_text, success),
            ] {
                history.push(ResponseItem::FunctionCall {
                    id: None,
                    name: tool_name.to_string(),
                    namespace: None,
                    arguments: "{}".to_string(),
                    call_id: call_id.to_string(),
                    internal_chat_message_metadata_passthrough: None,
                });
                let mut output = FunctionCallOutputPayload::from_text(text);
                output.success = success;
                history.push(ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: call_id.to_string(),
                    output,
                    internal_chat_message_metadata_passthrough: None,
                });
            }
            let store = PlanStore::default();
            store.update(earlier.clone()).await;
            store.restore(Some(rejected.clone())).await;
            assert_eq!(store.current_for_test().await, Some(rejected.clone()));
            assert!(store.restore_from_history(&history).await);
            assert_eq!(store.current_for_test().await, Some(expected.clone()));
        }
    }
}
#[cfg(test)]
#[path = "obligation_plan_tests.rs"]
mod obligation_regressions;
