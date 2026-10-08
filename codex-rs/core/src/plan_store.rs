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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanInputReference {
    pub(crate) turn_id: String,
    pub(crate) sha256: String,
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
            if requirement.superseded_reason.is_some() && self.accepted_supersessions.contains_key(id) {
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
        let legacy = format!("{:x}", Sha256::digest(text.as_bytes()));
        self.step_identities.get(&key)
            .or_else(|| self.step_identities.get(&legacy)).cloned()
            .or_else(|| self.step_requirements.contains_key(&legacy).then_some(legacy))
            .unwrap_or(key)
    }

    fn is_empty(&self) -> bool {
        self.requirements.is_empty() && self.step_requirements.is_empty()
            && self.step_identities.is_empty() && self.workflow.is_empty()
            && self.resolved_requirements.is_empty() && self.step_revisions.is_empty()
            && self.accepted_supersessions.is_empty()
    }

    /// The checklist already carries these exact entries. Replay reconstructs
    /// them; preserve splits, renames, supersessions and legacy identities.
    pub(crate) fn compact_for_plan(&self, plan: &UpdatePlanArgs) -> Self {
        let mut compact = self.clone();
        let mut references = HashMap::<&str, usize>::new();
        for ids in self.step_requirements.values() {
            for id in ids { *references.entry(id.as_str()).or_default() += 1; }
        }
        for step in &plan.plan {
            let id = self.step_id(&step.step);
            if id == plan_step_id(&step.step)
                && self.step_requirements.get(&id).is_some_and(|ids| ids == &[id.clone()])
                && references.get(id.as_str()) == Some(&1)
                && self.requirements.get(&id).is_some_and(|requirement| {
                    requirement.text == step.step && requirement.status == step.status
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
        let mapped = active.step_requirements.values().flatten().cloned().collect::<HashSet<_>>();
        active.requirements.retain(|id, requirement| {
            (!active.accepted_supersessions.contains_key(id)
                && (requirement.status != StepStatus::Completed || requirement.superseded_reason.is_some()))
                || mapped.contains(id)
        });
        active.resolved_requirements.retain(|id| active.requirements.contains_key(id));
        active.accepted_supersessions.retain(|id, _| active.requirements.contains_key(id));
        active
    }

    fn orphan(&self, id: &str) -> Option<&PlanRequirement> {
        self.requirements.get(id).filter(|requirement| {
            (requirement.status != StepStatus::Completed || requirement.superseded_reason.is_some())
                && !self.step_requirements.values().any(|ids| ids.iter().any(|mapped| mapped == id))
        })
    }

    fn revise(&mut self, previous_plan: Option<&UpdatePlanArgs>, steps: &[PlanStepArg], superseded: &[SupersededStep]) {
        let previous_ids = previous_plan.into_iter().flat_map(|plan| &plan.plan)
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
            let mut step_id = previous_ids.get(step.step.as_str()).cloned()
                .or_else(|| continued_id.cloned())
                .unwrap_or_else(|| text_id.clone());
            if !previous_ids.contains_key(step.step.as_str())
                && !continued_id.is_some_and(|id| previous.contains_key(id))
                && previous_plan.is_some()
            {
                step_id = uuid::Uuid::now_v7().to_string();
            }
            if step_id != text_id {
                self.step_identities.insert(text_id.clone(), step_id.clone());
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
            if !ids.iter().any(|id| self.requirements.get(id).is_some_and(|r| r.text == step.step)) {
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
        self.workflow.retain(|step_id, _| self.step_requirements.contains_key(step_id));
        self.step_revisions.retain(|step_id, _| self.step_requirements.contains_key(step_id));
        // Carrying scope forward reactivates it, rather than leaving a retirement
        // proposal that restore could mistake for an invalid mapping.
        for id in self.step_requirements.values().flatten() {
            if let Some(requirement) = self.requirements.get_mut(id) {
                requirement.superseded_reason = None;
            }
            self.accepted_supersessions.remove(id);
        }
        for dropped in superseded {
            let ids = previous.get(&dropped.step_id).cloned()
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
                let Some(requirement) = self.requirements.get(id) else { continue };
                let state = states.entry(id.clone()).or_insert((true, false, true, false));
                state.0 &= step.status == StepStatus::Completed;
                state.1 |= step.status == StepStatus::InProgress;
                state.2 &= step.step == requirement.text;
                state.3 |= step.status == StepStatus::Completed && step.step != requirement.text;
            }
        }
        for (id, (completed, in_progress, same_text, renamed_completion)) in states {
            if !completed { self.resolved_requirements.remove(&id); }
            let requirement = self.requirements.get_mut(&id).expect("known requirement");
            requirement.status = if completed && (same_text || self.resolved_requirements.contains(&id)) {
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
            &plan.plan.iter().map(|step| PlanStepArg {
                step: step.step.clone(),
                status: step.status,
                continues: Vec::new(),
            }).collect::<Vec<_>>(),
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
        self.derived.revision.get_or_init(|| plan_revision_with_lineage(self.plan.as_ref(), &self.lineage))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanExecutionSnapshot {
    pub(crate) revision: String,
    pub(crate) obligations: PlanObligationSummary,
    pub(crate) has_steps: bool,
}

impl PlanExecutionSnapshot {
    #[cfg(test)]
    pub(crate) fn new(plan: &UpdatePlanArgs, lineage: &PlanLineage) -> Self {
        Self {
            revision: plan_revision_with_lineage(Some(plan), lineage),
            obligations: lineage.obligation_summary(plan),
            has_steps: !plan.plan.is_empty(),
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

fn complete_lineage() -> bool { true }

pub(crate) fn checklist_completion_authority() -> String {
    "checklist_only".to_string()
}

pub(crate) fn plan_revision(plan: Option<&UpdatePlanArgs>) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(&plan).expect("plan serializes")))
}

pub(crate) fn plan_revision_with_lineage(
    plan: Option<&UpdatePlanArgs>,
    lineage: &PlanLineage,
) -> String {
    if lineage.is_empty() || *lineage == PlanLineage::from_plan(plan) {
        return plan_revision(plan);
    }
    format!("{:x}", Sha256::digest(serde_json::to_vec(&(plan, lineage)).expect("plan serializes")))
}

pub(crate) fn plan_step_id(step: &str) -> String {
    format!("{:x}", Sha256::digest(step.as_bytes()))[..16].to_string()
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
    accepted_input: Mutex<Option<(String, HashSet<String>)>>,
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
        if let Some(next) = self.next.as_mut() { next.durably_published = true; }
        else { self.current.durably_published = true; }
        self.commit()
    }

    pub(crate) fn commit(mut self) -> PlanStoreUpdate {
        if let Some(next) = self.next { *self.current = next; }
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
    pub(crate) async fn record_accepted_input<'a>(&self, turn_id: &str, texts: impl Iterator<Item = &'a str>) {
        let hashes = texts.filter(|text| !text.trim().is_empty())
            .map(|text| format!("{:x}", Sha256::digest(text.as_bytes())))
            .collect();
        *self.accepted_input.lock().await = Some((turn_id.to_string(), hashes));
    }
    #[cfg(test)]
    pub(crate) async fn active_requirement_count(&self) -> usize {
        self.current.lock().await.lineage.requirements.values().filter(|requirement| {
            requirement.status != StepStatus::Completed || requirement.superseded_reason.is_some()
        }).count()
    }

    pub(crate) async fn snapshot(&self) -> Option<UpdatePlanArgs> {
        self.current.lock().await.plan.clone()
    }

    pub(crate) async fn execution_snapshot(&self) -> Option<PlanExecutionSnapshot> {
        let current = self.current.lock().await;
        current.derived.execution.get_or_init(|| {
            let plan = current.plan.as_ref()?;
            Some(PlanExecutionSnapshot {
                revision: current.revision().to_string(),
                obligations: current.lineage.obligation_summary(plan),
                has_steps: !plan.plan.is_empty(),
            })
        }).clone()
    }

    pub(crate) async fn task_state(&self, suspended: bool) -> crate::context::world_state::TaskState {
        let current = self.current.lock().await;
        current.derived.task[usize::from(suspended)].get_or_init(|| {
            crate::context::world_state::TaskState::from_plan(
                current.plan.as_ref().map(|plan| (plan, &current.lineage, current.revision())),
            ).with_execution_suspended(suspended)
        }).clone()
    }

    pub(crate) async fn snapshot_with_lineage(&self) -> Option<(UpdatePlanArgs, PlanLineage)> {
        let current = self.current.lock().await;
        Some((current.plan.clone()?, current.lineage.clone()))
    }

    pub(crate) async fn restore_from_history(&self, items: &[ResponseItem]) -> bool {
        let update_call_ids = items
            .iter()
            .filter_map(|item| match item {
                ResponseItem::FunctionCall { name, namespace: None, call_id, .. } if name == "update_plan" => {
                    Some(call_id.as_str())
                }
                _ => None,
            })
            .collect::<HashSet<_>>();
        let restored = items.iter().rev().find_map(|item| {
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
        });
        let found = restored.is_some();
        let mut current = self.current.lock().await;
        *current = restored
            .map(|response| {
                let lineage = response.lineage.reconcile_restored(Some(&response.current_plan));
                PlanState { plan: Some(response.current_plan), lineage, durably_published: false, derived: Default::default() }
            })
            .unwrap_or_default();
        found
    }

    #[cfg(test)]
    pub(crate) async fn restore(&self, plan: Option<UpdatePlanArgs>) {
        self.restore_with_lineage(plan, None).await;
    }

    pub(crate) async fn restore_with_lineage(
        &self,
        plan: Option<UpdatePlanArgs>,
        lineage: Option<PlanLineage>,
    ) {
        let mut current = self.current.lock().await;
        *current = PlanState {
            lineage: lineage.unwrap_or_default().reconcile_restored(plan.as_ref()),
            plan,
            durably_published: false,
            derived: Default::default(),
        };
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
            superseded,
            workflow,
            resolved_workflow,
        } = args;
        let accepted_input = if scope_change_instructions.is_empty() { None }
            else { self.accepted_input.lock().await.clone() };
        let guard = self.current.lock().await;
        let revision = guard.revision().to_string();
        let unchanged_since_sampling = sampling_revision.as_ref()
            .is_some_and(|sampled| std::sync::Arc::ptr_eq(&sampled.0, &guard.derived));
        let missing_revision = plan.is_some() && guard.plan.is_some()
            && expected_revision.is_none() && !unchanged_since_sampling;
        if missing_revision || expected_revision.as_deref().is_some_and(|expected| expected != revision) {
            let reconciliation = serde_json::json!({
                "revision": revision,
                "current_plan": guard.plan,
                "step_ids": guard.plan.iter().flat_map(|plan| &plan.plan)
                    .map(|step| guard.lineage.step_id(&step.step)).collect::<Vec<_>>(),
                "lineage": guard.plan.as_ref().map(|plan| guard.lineage.active_for_plan(plan)),
                "obligations": guard.plan.as_ref().map(|plan| guard.lineage.obligation_summary(plan)),
                "completion_authority": checklist_completion_authority(),
            });
            let reason = if missing_revision { "plan replacements require expected_revision when the plan changed since sampling or no sampling fence is available" } else { "stale plan revision" };
            return Err(format!("{reason}; no changes were made. Reconcile against: {reconciliation}"));
        }
        let status_next = if let Some(statuses) = statuses {
            Some(Self::status_plan(&guard.plan, &guard.lineage, statuses, explanation.clone(),
                expected_revision.is_some(), expected_step_revisions.as_ref(), &revision)?)
        } else { None };
        if plan.is_none() && superseded.is_empty() && resolve.is_empty()
            && scope_change_instructions.is_empty() && workflow.is_none()
            && status_next.as_ref().is_some_and(|next| guard.plan.as_ref() == Some(next))
        {
            let update = PlanStoreUpdate { current: status_next.expect("status update"),
                effect: PlanUpdateEffect::NoOp, lineage: guard.lineage.clone() };
            return Ok(StagedPlanUpdate { current: guard, next: None, update });
        }
        let mut current = guard.clone();
        let mut lineage = current.lineage.clone();
        let next = if let Some(plan) = plan {
            let mut steps = HashSet::new();
            if plan.iter().any(|item| item.step.trim().is_empty()) {
                return Err("plan step cannot be empty".into());
            }
            if plan.iter().any(|item| !steps.insert(item.step.as_str())) {
                return Err("plan steps must have distinct text so their stable IDs are unambiguous".into());
            }
            let explanation =
                account_for_removed_steps(current.plan.as_ref(), &lineage, &plan, &superseded, explanation)?;
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
        for (step_id, instruction) in scope_change_instructions {
            if !superseded.iter().any(|entry| entry.step_id == step_id) {
                return Err("scope_change_instructions must name a superseded entry; no changes were made".into());
            }
            let sha256 = format!("{:x}", Sha256::digest(instruction.as_bytes()));
            let Some((turn_id, _)) = accepted_input.as_ref().filter(|(_, hashes)| hashes.contains(&sha256)) else {
                return Err("scope change must cite exact, accepted user text; tool/context text and invented instructions are not authorization. No changes were made".into());
            };
            let ids = current.lineage.step_requirements.get(&step_id).cloned().unwrap_or_else(|| vec![step_id]);
            for id in ids {
                if !lineage.step_requirements.values().any(|mapped| mapped.contains(&id)) {
                    lineage.accepted_supersessions.insert(id, PlanInputReference { turn_id: turn_id.clone(), sha256: sha256.clone() });
                }
            }
        }
        for id in resolve {
            if !lineage.requirements.contains_key(&id) {
                return Err(format!("resolve names unknown requirement {id}; no changes were made"));
            }
            if next.plan.iter().any(|step| step.status != StepStatus::Completed
                && lineage.step_requirements.get(&lineage.step_id(&step.step)).is_some_and(|ids| ids.contains(&id)))
            {
                return Err(format!("complete the descendant checklist before resolving {id}; no changes were made"));
            }
            lineage.resolved_requirements.insert(id.clone());
            let requirement = lineage.requirements.get_mut(&id).expect("known requirement");
            requirement.status = StepStatus::Completed;
            requirement.superseded_reason = None;
            lineage.accepted_supersessions.remove(&id);
        }
        if let Some(previous) = &current.plan {
            let previous_steps = previous.plan.iter().map(|step| (current.lineage.step_id(&step.step), step)).collect::<HashMap<_, _>>();
            for step in &next.plan {
                let id = lineage.step_id(&step.step);
                if previous_steps.get(&id).is_some_and(|old| *old != step
                    || current.lineage.step_requirements.get(&id) != lineage.step_requirements.get(&id))
                {
                    let version = current.lineage.step_revisions.get(&id).copied().unwrap_or_default();
                    lineage.step_revisions.insert(id, version.checked_add(1).ok_or("plan step revision exhausted")?);
                }
            }
        }
        if let Some(workflow) = workflow {
            let resolved = resolved_workflow.ok_or(
                "workflow links must be resolved by the task coordinator; no changes were made"
            )?;
            if workflow.len() != resolved.len()
                || workflow.iter().any(|(step_id, assignment_id)| {
                    !lineage.step_requirements.contains_key(step_id)
                        || resolved.get(step_id).is_none_or(|node| node.assignment_id != *assignment_id)
                })
            {
                return Err("workflow names unknown steps or unverified assignments; no changes were made".into());
            }
            lineage.workflow = resolved;
        }
        let lineage_changed = current.lineage != lineage;
        if lineage_changed { current.derived = Default::default(); }
        current.lineage = lineage;
        let mut update = Self::commit(&mut current, next);
        if lineage_changed && update.effect == PlanUpdateEffect::NoOp {
            update.effect = PlanUpdateEffect::StructuralRevision;
        }
        Ok(StagedPlanUpdate { current: guard, next: Some(current), update })
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
        // proves an index refers to the plan the caller read.
        if !revision_checked && updates.iter().any(|update| update.index.is_some()) {
            return Err(format!(
                "index-based status updates require expected_revision; current revision is {}. Use step_id to address steps without a revision. No changes were made.",
                revision
            ));
        }
        let mut seen = HashSet::new();
        for update in updates {
            let index = match (update.index, update.step_id.as_deref()) {
                (Some(index), None) => index,
                (None, Some(id)) => {
                    let matches = next.plan.iter().enumerate()
                        .filter(|(_, item)| {
                            let current_id = lineage.step_id(&item.step);
                            current_id == id || (id.len() >= 8 && current_id.starts_with(id))
                        })
                        .map(|(index, _)| index).collect::<Vec<_>>();
                    if matches.len() != 1 {
                        let valid = next.plan.iter().map(|item| {
                            format!("{}: {}", lineage.step_id(&item.step), quoted_step(&item.step))
                        }).collect::<Vec<_>>().join("\n");
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
                || (!revision_checked && version != 0 && expected.is_none() && item.status != update.status)
            {
                return Err(format!("stale or missing step revision for {id}; current step revision is {version}. Supply expected_step_revisions or expected_revision. No changes were made"));
            }
            item.status = update.status;
        }
        if next
            .plan
            .iter()
            .filter(|item| item.status == StepStatus::InProgress)
            .count()
            > 1
        {
            return Err("update_plan permits at most one in_progress step at a time".to_string());
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
        if effect != PlanUpdateEffect::NoOp { current.lineage.update_statuses(&next); }
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
            if reason.is_empty() || !dropped.insert(entry.step_id.as_str())
                || continued.contains(entry.step_id.as_str())
            {
                return Err("an orphan must be continued or superseded with one non-empty reason, not both; no changes were made".into());
            }
            records.push(format!("Proposed supersession (unverified) {}: {reason}", quoted_step(&orphan.text)));
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
        records.push(format!("Proposed supersession (unverified) {}: {reason}", quoted_step(&original.step)));
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

fn quoted_step(step: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut text = step.chars().take(MAX_CHARS).collect::<String>();
    if step.chars().nth(MAX_CHARS).is_some() {
        text.push_str("...");
    }
    format!("{text:?}")
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
        store.stage_tool(serde_json::from_value(args).unwrap()).await.unwrap().commit_published()
    }

    #[tokio::test]
    async fn continuity_plan_cache_tracks_commit_noop_abort_and_lineage_only_changes() {
        use crate::context::ContextualUserFragment;
        let store = PlanStore::default();
        let initial = json_update(&store, serde_json::json!({"plan":[{"step":"work","status":"pending"}]})).await;
        let id = initial.lineage.step_id("work");
        let first = store.task_state(false).await.body().into_owned();
        let cached = store.current.lock().await.derived.clone();
        json_update(&store, serde_json::json!({"set":[{"step_id":id,"status":"pending"}]})).await;
        assert!(std::sync::Arc::ptr_eq(&cached, &store.current.lock().await.derived));
        let aborted = store.stage_tool(serde_json::from_value(serde_json::json!({
            "set":[{"step_id":id,"status":"completed"}]
        })).unwrap()).await.unwrap();
        drop(aborted);
        assert_eq!(store.task_state(false).await.body(), first);
        json_update(&store, serde_json::json!({"set":[{"step_id":id,"status":"completed"}]})).await;
        assert_ne!(store.task_state(false).await.body(), first);
        let revision = store.execution_snapshot().await.unwrap().revision;
        json_update(&store, serde_json::json!({"set":[{"step_id":id,"status":"completed"}],"resolve":[id]})).await;
        let next = store.execution_snapshot().await.unwrap().revision;
        assert_ne!(next, revision);
        let rendered = store.task_state(false).await.body().into_owned();
        assert!(rendered.contains(&next));
        assert!(!std::sync::Arc::ptr_eq(&cached, &store.current.lock().await.derived));
    }

    #[tokio::test]
    async fn introduced_scope_survives_reverting_a_continuation() {
        let store = PlanStore::default();
        let original = json_update(&store, serde_json::json!({"plan":[{"step":"Inspect A","status":"pending"}]})).await;
        let id = original.lineage.step_id("Inspect A");
        let expanded = json_update(&store, serde_json::json!({"plan":[{"step":"Inspect A and B","status":"pending","continues":[id]}]})).await;
        assert_eq!(expanded.lineage.requirements.len(), 2);
        let reverted = json_update(&store, serde_json::json!({"plan":[{"step":"Inspect A","status":"completed","continues":[id]}]})).await;
        let outstanding = reverted.lineage.obligation_summary(&reverted.current).unresolved;
        assert_eq!(outstanding, vec![plan_step_id("Inspect A and B")]);
        assert_eq!(reverted.lineage.requirements[&outstanding[0]].text, "Inspect A and B");
        store.restore_with_lineage(Some(reverted.current.clone()), Some(reverted.lineage.clone())).await;
        assert_eq!(store.snapshot_with_lineage().await.unwrap(), (reverted.current, reverted.lineage));
    }

    #[tokio::test]
    async fn explicit_original_resolution_survives_restore_and_reopening_invalidates_it() {
        let store = PlanStore::default();
        let original = json_update(&store, serde_json::json!({"plan":[{"step":"Verify the parser","status":"pending"}]})).await;
        let id = original.lineage.step_id("Verify the parser");
        let done = json_update(&store, serde_json::json!({
            "plan":[{"step":"Verify parser","status":"completed","continues":[id]}], "resolve":[id]
        })).await;
        assert!(done.lineage.obligation_summary(&done.current).unresolved.is_empty());
        assert_eq!(done.lineage.requirements[&id].text, "Verify the parser");
        store.restore_with_lineage(Some(done.current.clone()), Some(done.lineage.compact_for_plan(&done.current))).await;
        assert_eq!(store.snapshot_with_lineage().await.unwrap(), (done.current, done.lineage));
        let reopened = json_update(&store, serde_json::json!({"set":[{"step_id":id,"status":"pending"}]})).await;
        assert!(!reopened.lineage.resolved_requirements.contains(&id));
        assert_eq!(reopened.lineage.obligation_summary(&reopened.current).unresolved.len(), 2);
        let before = store.execution_snapshot().await;
        let error = store.update_tool(serde_json::from_value(serde_json::json!({
            "set":[{"step_id":id,"status":"pending"}], "resolve":[id]
        })).unwrap()).await.unwrap_err();
        assert!(error.contains("descendant"));
        assert_eq!(store.execution_snapshot().await, before);
    }

    #[tokio::test]
    async fn reattached_retirement_proposal_is_restore_idempotent() {
        let store = PlanStore::default();
        let original = json_update(&store, serde_json::json!({"plan":[{"step":"Deploy","status":"pending"}]})).await;
        let id = original.lineage.step_id("Deploy");
        json_update(&store, serde_json::json!({"plan":[],"superseded":[{"step_id":id,"reason":"unverified cancellation"}]})).await;
        let attached = json_update(&store, serde_json::json!({"plan":[{"step":"Deploy","status":"pending","continues":[id]}]})).await;
        let before = store.execution_snapshot().await;
        assert_eq!(before.as_ref().unwrap().obligations.unresolved.len(), 1);
        for _ in 0..3 {
            let (plan, lineage) = store.snapshot_with_lineage().await.unwrap();
            store.restore_with_lineage(Some(plan.clone()), Some(lineage.compact_for_plan(&plan))).await;
            assert_eq!(store.execution_snapshot().await, before);
            assert_eq!(store.snapshot_with_lineage().await.unwrap(), (attached.current.clone(), attached.lineage.clone()));
        }
    }

    #[tokio::test]
    async fn scope_retirement_requires_real_accepted_input_and_preserves_provenance() {
        let store = PlanStore::default();
        let original = json_update(&store, serde_json::json!({"plan":[{"step":"Deploy","status":"pending"}]})).await;
        let id = original.lineage.step_id("Deploy");
        let args = serde_json::json!({"plan":[],"expected_revision":store.execution_snapshot().await.unwrap().revision,
            "superseded":[{"step_id":id,"reason":"User cancelled deployment"}],
            "scope_change_instructions":{id.clone():"Do not deploy; review only."}});
        let before = store.execution_snapshot().await;
        assert!(store.update_tool(serde_json::from_value(args.clone()).unwrap()).await.is_err());
        assert_eq!(store.execution_snapshot().await, before);
        store.record_accepted_input("user-turn", ["Do not deploy; review only."].into_iter()).await;
        let update = store.update_tool(serde_json::from_value(args).unwrap()).await.unwrap();
        assert_eq!(update.lineage.obligation_summary(&update.current).superseded, 1);
        assert!(update.lineage.obligation_summary(&update.current).unresolved.is_empty());
        assert_eq!(update.lineage.accepted_supersessions[&id].turn_id, "user-turn");
        assert!(!update.lineage.active_for_plan(&update.current).requirements.contains_key(&id));
        store.restore_with_lineage(Some(update.current.clone()), Some(update.lineage.clone())).await;
        assert_eq!(store.snapshot_with_lineage().await.unwrap(), (update.current, update.lineage));
    }

    #[tokio::test]
    async fn step_cas_rejects_aba_but_allows_independent_peer_deltas() {
        let store = PlanStore::default();
        let initial = json_update(&store, serde_json::json!({"plan":[
            {"step":"A","status":"pending"},{"step":"B","status":"pending"}
        ]})).await;
        let a = initial.lineage.step_id("A");
        let b = initial.lineage.step_id("B");
        for id in [&a, &b] {
            store.update_tool(serde_json::from_value(serde_json::json!({
                "set":[{"step_id":id,"status":"completed"}], "expected_step_revisions":{id:0}
            })).unwrap()).await.unwrap();
        }
        json_update(&store, serde_json::json!({"set":[{"step_id":a,"status":"pending"}]})).await;
        let before = store.execution_snapshot().await;
        for versions in [None, Some(serde_json::json!({a.clone():0}))] {
            let mut args = serde_json::json!({"set":[{"step_id":a,"status":"completed"}]});
            if let Some(versions) = versions { args["expected_step_revisions"] = versions; }
            assert!(store.update_tool(serde_json::from_value(args).unwrap()).await.unwrap_err().contains("step revision"));
            assert_eq!(store.execution_snapshot().await, before);
        }
        let current = store.snapshot_with_lineage().await.unwrap();
        store.restore_with_lineage(Some(current.0), Some(current.1.clone())).await;
        let good = serde_json::json!({"set":[{"step_id":a,"status":"completed"}],
            "expected_step_revisions":{a.clone():current.1.step_revisions[&a]}});
        assert!(store.update_tool(serde_json::from_value(good).unwrap()).await.unwrap()
            .lineage.obligation_summary(store.snapshot().await.as_ref().unwrap()).unresolved.is_empty());
    }

    #[tokio::test]
    async fn noop_skips_state_rebuild_but_restored_state_still_needs_publication() {
        let store = PlanStore::default();
        json_update(&store, serde_json::json!({"plan":[{"step":"A","status":"pending"}]})).await;
        let args = || serde_json::from_value(serde_json::json!({"set":[{"step_id":plan_step_id("A"),"status":"pending"}]})).unwrap();
        let before = store.snapshot_with_lineage().await.unwrap();
        let staged = store.stage_tool(args()).await.unwrap();
        assert!(staged.next.is_none());
        assert!(!staged.needs_publication());
        staged.commit();
        store.restore_with_lineage(Some(before.0.clone()), Some(before.1.clone())).await;
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
        let expected_revision = store.execution_snapshot().await.map(|snapshot| snapshot.revision);
        store.update_tool(PlanToolArgs { plan: Some(plan), expected_revision, ..Default::default() })
            .await.unwrap()
    }

    #[tokio::test]
    async fn sampling_fence_rejects_concurrent_replacements_and_restore() {
        let store = PlanStore::default();
        let original = plan("work", StepStatus::Pending);
        store.update(original.clone()).await;
        let sampled = store.sampling_revision().await;
        let args = |status| PlanToolArgs {
            sampling_revision: Some(sampled.clone()),
            plan: Some(vec![PlanStepArg { step: "work".into(), status, continues: vec![] }]),
            ..Default::default()
        };
        // Dropping a staged write (for example failed persistence) changes no fence.
        drop(store.stage_tool(args(StepStatus::InProgress)).await.unwrap());
        let (left, right) = tokio::join!(
            store.update_tool(args(StepStatus::InProgress)),
            store.update_tool(args(StepStatus::Completed)),
        );
        assert_ne!(left.is_ok(), right.is_ok(), "only one sampled replacement commits");
        store.restore(Some(original)).await;
        assert!(store.update_tool(args(StepStatus::Completed)).await.is_err(), "restoring identical content cannot revive an old fence");
        let mut fresh = args(StepStatus::Completed);
        fresh.sampling_revision = Some(store.sampling_revision().await);
        store.update_tool(fresh).await.unwrap();
        assert!(serde_json::from_value::<PlanToolArgs>(serde_json::json!({
            "plan": [], "sampling_revision": "forged"
        })).is_err(), "sampling fences cannot be supplied by the model");
    }

    #[tokio::test]
    async fn retired_step_identity_cannot_complete_reintroduced_work() {
        let store = PlanStore::default();
        let step = |status| PlanStepArg { step: "same work".into(), status, continues: vec![] };
        let original = replace(&store, vec![step(StepStatus::Completed)]).await;
        let old_id = original.lineage.step_id("same work");
        replace(&store, vec![]).await;
        let recreated = replace(&store, vec![step(StepStatus::Pending)]).await;
        let new_id = recreated.lineage.step_id("same work");
        assert_ne!(new_id, old_id);
        assert!(store.update_tool(PlanToolArgs {
            set: Some(vec![PlanStatusUpdate { index: None, step_id: Some(old_id.clone()), status: StepStatus::Completed }]),
            ..Default::default()
        }).await.is_err());
        assert_eq!(store.snapshot().await, Some(recreated.current.clone()));
        assert_eq!(recreated.lineage.requirements[&old_id].status, StepStatus::Completed);
        store.restore_with_lineage(Some(recreated.current), Some(recreated.lineage)).await;
        assert_eq!(store.snapshot_with_lineage().await.unwrap().1.step_id("same work"), new_id);
    }

    #[tokio::test]
    async fn replacement_conflicts_return_reconciliation_without_reopening_work() {
        let store = PlanStore::default();
        let original = plan("work", StepStatus::Pending);
        store.update(original.clone()).await;
        let stale = store.execution_snapshot().await.unwrap().revision;
        store.update_tool(PlanToolArgs {
            set: Some(vec![PlanStatusUpdate { index: None, step_id: Some(plan_step_id("work")), status: StepStatus::Completed }]),
            ..Default::default()
        }).await.unwrap();
        let current = store.execution_snapshot().await.unwrap();
        for expected_revision in [None, Some(stale)] {
            let error = store.update_tool(PlanToolArgs {
                expected_revision,
                plan: Some(vec![PlanStepArg { step: "work".into(), status: StepStatus::Pending, continues: vec![] }]),
                ..Default::default()
            }).await.unwrap_err();
            let reconciliation: serde_json::Value = serde_json::from_str(error.split_once("Reconcile against: ").unwrap().1).unwrap();
            assert_eq!(reconciliation["revision"], current.revision);
            assert_eq!(reconciliation["current_plan"]["plan"][0]["status"], "completed");
            assert_eq!(reconciliation["step_ids"][0], plan_step_id("work"));
            assert_eq!(store.execution_snapshot().await, Some(current.clone()));
        }
    }

    #[tokio::test]
    async fn rename_and_completion_are_atomic_and_keep_original_obligations() {
        let store = PlanStore::default();
        store.update(plan("original", StepStatus::Pending)).await;
        let id = plan_step_id("original");
        let update = replace(&store, vec![PlanStepArg {
            step: "finished work".into(), status: StepStatus::Completed, continues: vec![id.clone()],
        }]).await;
        assert_eq!(update.lineage.step_id("finished work"), id);
        assert_eq!(update.lineage.requirements[&id].text, "original");
        assert_eq!(update.current.plan[0].status, StepStatus::Completed);
        assert_eq!(update.lineage.obligation_summary(&update.current).unresolved, vec![id.clone()]);

        let two_calls = PlanStore::default();
        two_calls.update(plan("original", StepStatus::Pending)).await;
        replace(&two_calls, vec![PlanStepArg {
            step: "finished work".into(), status: StepStatus::InProgress, continues: vec![id.clone()],
        }]).await;
        let second = two_calls.update_tool(PlanToolArgs {
            expected_revision: Some(two_calls.execution_snapshot().await.unwrap().revision),
            set: Some(vec![PlanStatusUpdate { index: None, step_id: Some(id.clone()), status: StepStatus::Completed }]),
            ..Default::default()
        }).await.unwrap();
        assert_eq!(second.lineage.step_revisions[&id], 2);
        assert_eq!(update.lineage.step_revisions[&id], 1);
        let mut second_lineage = second.lineage;
        second_lineage.step_revisions = update.lineage.step_revisions.clone();
        assert_eq!(second_lineage, update.lineage, "a second call adds no authority");
        let restored = update.lineage.compact_for_plan(&update.current).reconcile_restored(Some(&update.current));
        assert_eq!(restored.requirements[&id].status, StepStatus::InProgress);
        assert!(restored.active_for_plan(&update.current).requirements.contains_key(&id));
    }

    #[tokio::test]
    async fn active_projection_does_not_grow_with_retired_history_and_restore_is_exact() {
        let store = PlanStore::default();
        for index in 0..40 {
            replace(&store, vec![PlanStepArg {
                step: format!("finished {index}"), status: StepStatus::Completed, continues: vec![],
            }]).await;
        }
        let update = replace(&store, vec![PlanStepArg {
            step: "current".into(), status: StepStatus::Pending, continues: vec![],
        }]).await;
        assert_eq!(update.lineage.requirements.len(), 41);
        let active = update.lineage.active_for_plan(&update.current);
        assert_eq!(active.requirements.len(), 1);
        let durable = update.lineage.compact_for_plan(&update.current);
        let restored = durable.reconcile_restored(Some(&update.current));
        assert_eq!(restored, update.lineage);
        assert_eq!(active.obligation_summary(&update.current).unresolved,
            restored.obligation_summary(&update.current).unresolved);
    }

    #[tokio::test]
    async fn status_step_prefixes_are_unique_and_atomic() {
        let store = PlanStore::default();
        let original = plan("inspect", StepStatus::Pending);
        store.update(original.clone()).await;
        let id = plan_step_id("inspect");
        let update = |ids: Vec<String>| PlanToolArgs {
            set: Some(ids.into_iter().map(|step_id| PlanStatusUpdate {
                index: None, step_id: Some(step_id), status: StepStatus::Completed,
            }).collect()),
            ..Default::default()
        };
        for bad in [id[..7].to_string(), "not-a-step".to_string()] {
            let error = store.update_tool(update(vec![id[..8].to_string(), bad])).await.unwrap_err();
            assert!(error.contains(&id));
            assert!(error.contains("inspect"));
            assert_eq!(store.snapshot().await, Some(original.clone()));
        }
        assert!(store.update_tool(update(vec![id.clone(), id[..8].to_string()])).await.is_err());
        assert_eq!(store.snapshot().await, Some(original));
        let result = store.update_tool(update(vec![id[..8].to_string()])).await.unwrap();
        assert_eq!(result.current.plan[0].status, StepStatus::Completed);

        let current = UpdatePlanArgs {
            explanation: None,
            plan: vec![
                PlanItemArg { step: "one".into(), status: StepStatus::Pending },
                PlanItemArg { step: "two".into(), status: StepStatus::Pending },
            ],
        };
        let mut lineage = PlanLineage::default();
        lineage.step_identities.insert(plan_step_id("one"), "12345678aaaa".into());
        lineage.step_identities.insert(plan_step_id("two"), "12345678bbbb".into());
        let error = PlanStore::status_plan(&Some(current), &lineage,
            update(vec!["12345678".into()]).set.unwrap(), None, false, None, "revision").unwrap_err();
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

        let legacy_id = format!("{:x}", Sha256::digest(b"inspect"));
        let mut legacy = PlanLineage::default();
        legacy.requirements.insert(legacy_id.clone(), PlanRequirement {
            text: "inspect".into(), status: StepStatus::Pending, superseded_reason: None,
        });
        legacy.step_requirements.insert(legacy_id.clone(), vec![legacy_id.clone()]);
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
                PlanItemArg { step: "inspect".into(), status: StepStatus::Pending },
                PlanItemArg { step: "verify".into(), status: StepStatus::Pending },
            ],
        };
        store.update(original.clone()).await;
        let revision = plan_revision(Some(&original));
        let mut reordered = original.clone();
        reordered.plan.reverse();
        store.update(reordered.clone()).await;
        let updates = || Some(vec![PlanStatusUpdate {
            index: None, step_id: Some(plan_step_id("inspect")), status: StepStatus::Completed,
        }]);
        let set = |set, expected_revision: Option<&str>| PlanToolArgs {
            set,
            expected_revision: expected_revision.map(str::to_string),
            ..Default::default()
        };
        assert!(store.update_tool(set(updates(), Some(&revision))).await.is_err());
        assert_eq!(store.snapshot().await, Some(reordered.clone()));
        // Index 0 meant "inspect" in the original plan but "verify" after the
        // reorder; without a revision the host cannot tell which was meant.
        let first_by_index = || Some(vec![PlanStatusUpdate {
            index: Some(0), step_id: None, status: StepStatus::Completed,
        }]);
        assert!(store.update_tool(set(first_by_index(), None)).await.is_err());
        assert_eq!(store.snapshot().await, Some(reordered.clone()));
        let bound = store.update_tool(
            set(first_by_index(), Some(&plan_revision(Some(&reordered)))),
        ).await.expect("revision-bound index update");
        assert_eq!(bound.current.plan[0].step, "verify");
        assert_eq!(bound.current.plan[0].status, StepStatus::Completed);
        let resumed = PlanStore::default();
        resumed.restore(Some(reordered.clone())).await;
        let result = resumed.update_tool(
            set(updates(), Some(&plan_revision(Some(&reordered)))),
        ).await.unwrap();
        assert_eq!(result.current.plan[0].status, StepStatus::Pending);
        assert_eq!(result.current.plan[1].status, StepStatus::Completed);
    }

    #[test]
    fn completion_summary_separates_resolved_unresolved_and_superseded_requirements() {
        let current = plan("required check", StepStatus::Pending);
        let mut lineage = PlanLineage::from_plan(Some(&current));
        lineage.requirements.insert("completed".into(), PlanRequirement {
            text: "verified implementation".into(), status: StepStatus::Completed,
            superseded_reason: None,
        });
        lineage.requirements.insert("retired".into(), PlanRequirement {
            text: "user removed scope".into(), status: StepStatus::Pending,
            superseded_reason: Some("explicit correction".into()),
        });
        let summary = lineage.obligation_summary(&current);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.superseded, 0);
        assert_eq!(summary.unresolved, vec![plan_step_id("required check"), "retired".into()]);
        assert_eq!(PlanLineage::default().obligation_summary(&current).unresolved,
            vec![plan_step_id("required check")], "legacy checklist is not an empty obligation set");
        lineage.update_statuses(&plan("required check", StepStatus::Completed));
        let summary = lineage.obligation_summary(&current);
        assert_eq!(summary.completed, 2);
        assert_eq!(summary.unresolved, vec!["retired"]);
        assert!(lineage.active_for_plan(&current).requirements.contains_key("retired"));
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
        store.restore_with_lineage(Some(parsed.current_plan), Some(parsed.lineage)).await;
        let (restored, lineage) = store.snapshot_with_lineage().await.unwrap();
        let summary = lineage.obligation_summary(&restored);
        assert_eq!(summary.completed, 0);
        assert_eq!(summary.unresolved, vec![plan_step_id("still required")]);
    }

    #[tokio::test]
    async fn completion_audit_restores_partial_lineage_without_losing_obligations() {
        for mode in ["missing-step", "unknown-reference", "retired-reference", "stale-status"] {
            for history_replay in [false, true] {
                let current = plan("verify current work", StepStatus::Pending);
                let id = plan_step_id("verify current work");
                let mut lineage = PlanLineage::from_plan(Some(&plan("earlier work", StepStatus::Completed)));
                let earlier_id = plan_step_id("earlier work");
                lineage.requirements.insert("unmapped-open".into(), PlanRequirement {
                    text: "original unresolved obligation".into(),
                    status: StepStatus::InProgress,
                    superseded_reason: None,
                });
                match mode {
                    "unknown-reference" => { lineage.step_requirements.insert(id.clone(), vec!["absent".into()]); }
                    "retired-reference" => {
                        lineage.requirements.get_mut(&earlier_id).unwrap().superseded_reason = Some("user removed scope".into());
                        lineage.step_requirements.insert(id.clone(), vec![earlier_id.clone()]);
                    }
                    "stale-status" => {
                        lineage.requirements.insert(id.clone(), PlanRequirement {
                            text: "verify current work".into(), status: StepStatus::Completed,
                            superseded_reason: None,
                        });
                        lineage.step_requirements.insert(id.clone(), vec![id.clone()]);
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
                    store.restore_with_lineage(Some(current.clone()), Some(lineage)).await;
                }
                let (restored, repaired) = store.snapshot_with_lineage().await.unwrap();
                assert_eq!(restored, current);
                let unresolved = 2 + usize::from(mode == "retired-reference");
                assert_eq!(store.active_requirement_count().await, unresolved, "{mode}");
                assert_eq!(repaired.obligation_summary(&restored).unresolved.len(), unresolved);
                assert_eq!(repaired.requirements["unmapped-open"].status, StepStatus::InProgress);
                let revision = plan_revision_with_lineage(Some(&restored), &repaired);
                store.restore_with_lineage(Some(restored.clone()), Some(repaired)).await;
                let (_, again) = store.snapshot_with_lineage().await.unwrap();
                assert_eq!(plan_revision_with_lineage(Some(&restored), &again), revision, "restore is idempotent");
                let closed = store.update_tool(PlanToolArgs {
                    set: Some(vec![PlanStatusUpdate { index: None, step_id: Some(id), status: StepStatus::Completed }]),
                    ..Default::default()
                }).await.unwrap();
                let outstanding = closed.lineage.obligation_summary(&closed.current).unresolved;
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
                PlanItemArg { step: review.into(), status: StepStatus::InProgress },
                PlanItemArg { step: "Fix confirmed warnings".into(), status: StepStatus::Pending },
            ],
        };
        store.update(original.clone()).await;
        let review_id = plan_step_id(review);
        let fix_id = plan_step_id("Fix confirmed warnings");
        let step = |text: &str, status, continues: &[&str]| PlanStepArg {
            step: text.into(),
            status,
            continues: continues.iter().map(|id| id.to_string()).collect(),
        };
        let revise = |plan, superseded: Vec<(&str, &str)>, explanation: Option<&str>| PlanToolArgs {
            plan: Some(plan),
            expected_revision: Some(plan_revision(Some(&original))),
            superseded: superseded
                .into_iter()
                .map(|(step_id, reason)| SupersededStep { step_id: step_id.into(), reason: reason.into() })
                .collect(),
            explanation: explanation.map(str::to_string),
            ..Default::default()
        };
        // A narrower rewording cannot retire obligations, even with a prose explanation.
        for rejected in [
            revise(vec![step("Inventory warnings", StepStatus::Completed, &[])], vec![], None),
            revise(
                vec![step("Inventory warnings", StepStatus::Completed, &[])],
                vec![],
                Some("The user requested only an inventory."),
            ),
            revise(
                vec![step("Inventory warnings", StepStatus::InProgress, &[review_id.as_str()])],
                vec![(fix_id.as_str(), "  ")],
                None,
            ),
            revise(
                vec![step("Inventory warnings", StepStatus::InProgress, &["unknown"])],
                vec![(fix_id.as_str(), "Dropped by the user.")],
                None,
            ),
        ] {
            assert!(store.update_tool(rejected).await.is_err());
            assert_eq!(store.snapshot().await, Some(original.clone()));
        }
        let revised = store
            .update_tool(revise(
                vec![step("Review warnings in core only", StepStatus::InProgress, &[review_id.as_str()])],
                vec![(fix_id.as_str(), "The user will fix warnings separately.")],
                Some("Narrowed review scope."),
            ))
            .await
            .expect("every removed unfinished step is accounted");
        assert_eq!(revised.effect, PlanUpdateEffect::StructuralRevision);
        assert_eq!(
            revised.current.explanation.as_deref(),
            Some("Narrowed review scope.\nProposed supersession (unverified) \"Fix confirmed warnings\": The user will fix warnings separately.")
        );
        assert_eq!(revised.lineage.step_id("Review warnings in core only"), review_id);
        let resumed = PlanStore::default();
        let lineage = serde_json::from_value(serde_json::to_value(&revised.lineage).unwrap()).unwrap();
        resumed.restore_with_lineage(Some(revised.current), Some(lineage)).await;
        let completed = resumed.update_tool(PlanToolArgs {
            expected_revision: Some(resumed.execution_snapshot().await.unwrap().revision),
            set: Some(vec![PlanStatusUpdate {
                index: None,
                step_id: Some(review_id.clone()),
                status: StepStatus::Completed,
            }]),
            ..Default::default()
        }).await.expect("the pre-rename identity survives persistence");
        assert_eq!(completed.current.plan[0].status, StepStatus::Completed);
        assert_eq!(completed.lineage.requirements[&review_id].text, review);
        assert_eq!(completed.lineage.requirements[&review_id].status, StepStatus::InProgress);
        assert_eq!(completed.lineage.obligation_summary(&completed.current).unresolved.len(), 2);
    }

    #[tokio::test]
    async fn classifies_straight_line_checklist_updates() {
        let store = PlanStore::default();

        assert_eq!(
            store
                .update(plan("inspect", StepStatus::InProgress))
                .await
                .effect,
            PlanUpdateEffect::Initial
        );
        assert_eq!(
            store
                .update(plan("inspect", StepStatus::Completed))
                .await
                .effect,
            PlanUpdateEffect::StatusOnly
        );
        assert_eq!(
            store
                .update(plan("inspect", StepStatus::Completed))
                .await
                .effect,
            PlanUpdateEffect::NoOp
        );
        assert_eq!(
            store
                .update(plan("implement", StepStatus::InProgress))
                .await
                .effect,
            PlanUpdateEffect::StructuralRevision
        );
    }

    #[tokio::test]
    async fn explanation_only_update_is_status_only() {
        let store = PlanStore::default();
        let mut initial = plan("inspect", StepStatus::InProgress);
        initial.explanation = Some("first explanation".to_string());
        assert_eq!(
            store.update(initial.clone()).await.effect,
            PlanUpdateEffect::Initial
        );

        initial.explanation = Some("reworded explanation".to_string());
        let effect = store.update(initial).await.effect;

        assert_eq!(effect, PlanUpdateEffect::StatusOnly);
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
        assert_eq!(store.update(expected).await.effect, PlanUpdateEffect::NoOp);
    }

    #[tokio::test]
    async fn namespaced_update_plan_cannot_restore_builtin_plan_state() {
        let mut history = vec![
            ResponseItem::FunctionCall { id:None, name:"update_plan".into(), namespace:Some("vendor".into()),
                arguments:"{}".into(), call_id:"vendor".into(), internal_chat_message_metadata_passthrough:None },
            ResponseItem::FunctionCallOutput { id:None, call_id:"vendor".into(),
                output:FunctionCallOutputPayload::from_text(serde_json::json!({"current_plan":plan("vendor", StepStatus::Pending)}).to_string()),
                internal_chat_message_metadata_passthrough:None },
        ];
        let store = PlanStore::default();
        assert!(!store.restore_from_history(&history).await);
        if let ResponseItem::FunctionCall { namespace, .. } = &mut history[0] { *namespace = None; }
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
