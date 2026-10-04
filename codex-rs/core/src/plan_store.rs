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
            if requirement.superseded_reason.is_some() {
                summary.superseded += 1;
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
        self.step_identities.get(&key).cloned().unwrap_or(key)
    }

    fn is_empty(&self) -> bool {
        self.requirements.is_empty()
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
            if !previous_ids.contains_key(step.step.as_str()) && continued_id.is_none()
                && (previous.contains_key(&step_id) || self.step_requirements.contains_key(&step_id))
            {
                step_id = uuid::Uuid::now_v7().to_string();
            }
            if step_id != text_id {
                self.step_identities.insert(text_id, step_id.clone());
            }
            let mut ids = previous.get(&step_id).cloned().unwrap_or_default();
            for source in &step.continues {
                ids.extend(previous.get(source).into_iter().flatten().cloned());
            }
            ids.sort();
            ids.dedup();
            if ids.is_empty() {
                // Reintroducing identical text must not overwrite a retired
                // requirement's original status or supersession record.
                let id = if self.requirements.contains_key(&step_id) {
                    uuid::Uuid::now_v7().to_string()
                } else {
                    step_id.clone()
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
            self.step_requirements.insert(step_id, ids);
        }
        self.workflow.retain(|step_id, _| self.step_requirements.contains_key(step_id));
        for dropped in superseded {
            for id in previous.get(&dropped.step_id).into_iter().flatten() {
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
        let steps = plan.plan.iter().map(|step| (self.step_id(&step.step), step.status)).collect::<Vec<_>>();
        for (id, requirement) in &mut self.requirements {
            let statuses = steps.iter()
                .filter(|(step_id, _)| {
                    self.step_requirements
                        .get(step_id)
                        .is_some_and(|ids| ids.contains(id))
                })
                .map(|(_, status)| *status)
                .collect::<Vec<_>>();
            if !statuses.is_empty() {
                requirement.status = if statuses.iter().all(|status| *status == StepStatus::Completed) {
                    StepStatus::Completed
                } else if statuses.contains(&StepStatus::InProgress) {
                    StepStatus::InProgress
                } else {
                    StepStatus::Pending
                };
            }
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
            ids.retain(|id| self.requirements.get(id)
                .is_some_and(|requirement| requirement.superseded_reason.is_none()));
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

#[derive(Debug, Default)]
struct PlanState {
    plan: Option<UpdatePlanArgs>,
    lineage: PlanLineage,
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
    pub(crate) plan: Option<Vec<PlanStepArg>>,
    pub(crate) set: Option<Vec<PlanStatusUpdate>>,
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
    format!("{:x}", Sha256::digest(step.as_bytes()))
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
}

impl PlanStore {
    pub(crate) async fn active_requirement_count(&self) -> usize {
        self.current.lock().await.lineage.requirements.values().filter(|requirement| {
            requirement.status != StepStatus::Completed && requirement.superseded_reason.is_none()
        }).count()
    }

    pub(crate) async fn snapshot(&self) -> Option<UpdatePlanArgs> {
        self.current.lock().await.plan.clone()
    }

    pub(crate) async fn snapshot_with_lineage(&self) -> Option<(UpdatePlanArgs, PlanLineage)> {
        let current = self.current.lock().await;
        Some((current.plan.clone()?, current.lineage.clone()))
    }

    pub(crate) async fn restore_from_history(&self, items: &[ResponseItem]) -> bool {
        let update_call_ids = items
            .iter()
            .filter_map(|item| match item {
                ResponseItem::FunctionCall { name, call_id, .. } if name == "update_plan" => {
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
                PlanState { plan: Some(response.current_plan), lineage }
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
        };
    }

    #[cfg(test)]
    pub(crate) async fn update(&self, next: UpdatePlanArgs) -> PlanStoreUpdate {
        let mut current = self.current.lock().await;
        current.lineage = PlanLineage::from_plan(Some(&next));
        Self::commit(&mut current, next)
    }

    /// Update the checklist atomically.
    pub(crate) async fn update_tool(&self, args: PlanToolArgs) -> Result<PlanStoreUpdate, String> {
        let PlanToolArgs {
            explanation,
            expected_revision,
            plan,
            set: statuses,
            superseded,
            workflow,
            resolved_workflow,
        } = args;
        let mut current = self.current.lock().await;
        let mut lineage = current.lineage.clone();
        let revision = plan_revision_with_lineage(current.plan.as_ref(), &current.lineage);
        if let Some(expected) = expected_revision.as_deref()
            && expected != revision
        {
            return Err(format!(
                "stale plan revision; current revision is {}. Reconcile the current plan before retrying; no changes were made.",
                revision
            ));
        }
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
        } else if let Some(statuses) = statuses {
            Self::status_plan(
                &current.plan,
                &lineage,
                statuses,
                explanation,
                expected_revision.is_some(),
                &revision,
            )?
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
        current.lineage = lineage;
        let mut update = Self::commit(&mut current, next);
        if lineage_changed && update.effect == PlanUpdateEffect::NoOp {
            update.effect = PlanUpdateEffect::StructuralRevision;
        }
        Ok(update)
    }

    fn status_plan(
        current: &Option<UpdatePlanArgs>,
        lineage: &PlanLineage,
        updates: Vec<PlanStatusUpdate>,
        explanation: Option<String>,
        revision_checked: bool,
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
                        .filter(|(_, item)| lineage.step_id(&item.step) == id)
                        .map(|(index, _)| index).collect::<Vec<_>>();
                    if matches.len() != 1 {
                        return Err(format!("unknown or ambiguous plan step ID {id}"));
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
        current.lineage.update_statuses(&next);
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
            let Some(original) = previous_by_id.get(id.as_str()) else {
                return Err(format!(
                    "step {} continues unknown step ID {id}; use step_ids from the previous result. No changes were made.",
                    quoted_step(&item.step)
                ));
            };
            if original.status != StepStatus::Completed && item.status == StepStatus::Completed {
                return Err(format!(
                    "step {} continues unfinished step {} and cannot be completed in the same revision; mark it completed with a later status update once those obligations are met. No changes were made.",
                    quoted_step(&item.step),
                    quoted_step(&original.step)
                ));
            }
            continued.insert(id.as_str());
        }
    }
    let mut dropped = HashSet::new();
    let mut records = Vec::new();
    for entry in superseded {
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
        records.push(format!("Superseded {}: {reason}", quoted_step(&original.step)));
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

    fn plan(step: &str, status: StepStatus) -> UpdatePlanArgs {
        UpdatePlanArgs {
            explanation: None,
            plan: vec![PlanItemArg {
                step: step.to_string(),
                status,
            }],
        }
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
        assert_eq!(summary.superseded, 1);
        assert_eq!(summary.unresolved, vec![plan_step_id("required check")]);
        assert_eq!(PlanLineage::default().obligation_summary(&current).unresolved,
            summary.unresolved, "legacy checklist is not an empty obligation set");
        lineage.update_statuses(&plan("required check", StepStatus::Completed));
        let summary = lineage.obligation_summary(&current);
        assert_eq!(summary.completed, 2);
        assert!(summary.unresolved.is_empty());
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
                assert_eq!(store.active_requirement_count().await, 2, "{mode}");
                assert_eq!(repaired.obligation_summary(&restored).unresolved.len(), 2);
                assert_eq!(repaired.requirements["unmapped-open"].status, StepStatus::InProgress);
                let revision = plan_revision_with_lineage(Some(&restored), &repaired);
                store.restore_with_lineage(Some(restored.clone()), Some(repaired)).await;
                let (_, again) = store.snapshot_with_lineage().await.unwrap();
                assert_eq!(plan_revision_with_lineage(Some(&restored), &again), revision, "restore is idempotent");
                let closed = store.update_tool(PlanToolArgs {
                    set: Some(vec![PlanStatusUpdate { index: None, step_id: Some(id), status: StepStatus::Completed }]),
                    ..Default::default()
                }).await.unwrap();
                assert_eq!(closed.lineage.obligation_summary(&closed.current).unresolved, vec!["unmapped-open"]);
                assert_eq!(store.active_requirement_count().await, 1);
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
            superseded: superseded
                .into_iter()
                .map(|(step_id, reason)| SupersededStep { step_id: step_id.into(), reason: reason.into() })
                .collect(),
            explanation: explanation.map(str::to_string),
            ..Default::default()
        };
        // A narrower rewording cannot retire obligations, even with a prose
        // explanation, and a carried step cannot claim them completed at once.
        for rejected in [
            revise(vec![step("Inventory warnings", StepStatus::Completed, &[])], vec![], None),
            revise(
                vec![step("Inventory warnings", StepStatus::Completed, &[])],
                vec![],
                Some("The user requested only an inventory."),
            ),
            revise(
                vec![step("Inventory warnings", StepStatus::Completed, &[review_id.as_str(), fix_id.as_str()])],
                vec![],
                None,
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
            Some("Narrowed review scope.\nSuperseded \"Fix confirmed warnings\": The user will fix warnings separately.")
        );
        assert_eq!(revised.lineage.step_id("Review warnings in core only"), review_id);
        let resumed = PlanStore::default();
        let lineage = serde_json::from_value(serde_json::to_value(&revised.lineage).unwrap()).unwrap();
        resumed.restore_with_lineage(Some(revised.current), Some(lineage)).await;
        let completed = resumed.update_tool(PlanToolArgs {
            set: Some(vec![PlanStatusUpdate {
                index: None,
                step_id: Some(review_id.clone()),
                status: StepStatus::Completed,
            }]),
            ..Default::default()
        }).await.expect("the pre-rename identity survives persistence");
        assert_eq!(completed.current.plan[0].status, StepStatus::Completed);
        assert_eq!(completed.lineage.requirements[&review_id].text, review);
        assert_eq!(completed.lineage.requirements[&review_id].status, StepStatus::Completed);
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
