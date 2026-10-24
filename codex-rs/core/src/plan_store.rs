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

#[path = "plan_store_legacy.rs"]
mod legacy;


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

/// Legacy scope fields decode old snapshots; only checklist identity and revisions are live.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanLineage {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) requirements: BTreeMap<String, PlanRequirement>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) step_requirements: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) step_identities: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) workflow: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) resolved_requirements: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) step_revisions: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) accepted_supersessions: BTreeMap<String, serde_json::Value>,
    /// Read-only historical fields, used only to authenticate pre-retirement
    /// revisions. Reconciliation drops them; no retired behavior is executed.
    #[serde(flatten)]
    legacy_metadata: BTreeMap<String, serde_json::Value>,
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
        let mut summary = PlanObligationSummary::default();
        for step in &plan.plan {
            if step.status == StepStatus::Completed {
                summary.completed += 1;
            } else {
                summary.unresolved.push(self.step_id(&step.step));
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
            && self.legacy_metadata.is_empty()

    }

    pub(crate) fn compact_for_plan(&self, _plan: &UpdatePlanArgs) -> Self {
        let mut compact = self.clone();
        compact.requirements.clear();
        compact.step_requirements.clear();
        compact.resolved_requirements.clear();
        compact.accepted_supersessions.clear();
        compact.workflow.clear();
        compact
    }


    /// Model-facing checklist metadata, without the legacy obligation ledger.
    pub(crate) fn checklist_metadata(&self) -> Self {
        Self {
            step_identities: self.step_identities.clone(),
            workflow: self.workflow.clone(),
            step_revisions: self.step_revisions.clone(),


            ..Self::default()
        }
    }


    fn revise(
        &mut self,
        previous_plan: Option<&UpdatePlanArgs>,
        steps: &[PlanStepArg],

    ) {
        let previous = previous_plan.into_iter().flat_map(|plan| &plan.plan)
            .map(|step| (step.step.clone(), self.step_id(&step.step)))
            .collect::<HashMap<_, _>>();
        let known = previous.values().cloned().collect::<HashSet<_>>();
        let mut used = HashSet::new();
        self.step_identities.clear();
        for step in steps {
            let text_id = plan_step_id(&step.step);
            let id = previous.get(&step.step).cloned().or_else(|| {
                (step.continues.len() == 1 && known.contains(&step.continues[0]))
                    .then(|| step.continues[0].clone())
            }).filter(|id| used.insert(id.clone()))
                .unwrap_or_else(|| if previous_plan.is_none() {
                    text_id.clone()
                } else {
                    uuid::Uuid::now_v7().to_string()
                });
            used.insert(id.clone());
            if id != text_id {
                self.step_identities.insert(text_id, id);
            }
        }
        self.requirements.clear();
        self.step_requirements.clear();
        self.resolved_requirements.clear();
        self.accepted_supersessions.clear();
        self.workflow.clear();
        self.step_revisions.retain(|id, _| used.contains(id));
    }

    fn update_statuses(&mut self, _plan: &UpdatePlanArgs) {
        // Old snapshots may contain a requirement graph. It is historical data,
        // not live checklist debt or an independent completion authority.
        self.requirements.clear();
        self.step_requirements.clear();
        self.resolved_requirements.clear();
        self.accepted_supersessions.clear();
        self.workflow.clear();
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

            );
        }
        lineage
    }

    /// Reconcile historical metadata with the owning checklist, retaining
    /// original obligations even when they are no longer mapped to a step.
    /// A partial lineage must not make a pending checklist appear resolved.
    fn reconcile_restored(mut self, plan: Option<&UpdatePlanArgs>) -> Self {
        self.legacy_metadata.clear();
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

/// Exactly one of plan or set is supplied.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanToolArgs {
    pub(crate) explanation: Option<String>,
    pub(crate) expected_revision: Option<String>,
    #[serde(skip)]
    pub(crate) sampling_revision: Option<PlanSamplingRevision>,
    pub(crate) plan: Option<Vec<PlanStepArg>>,
    pub(crate) set: Option<Vec<PlanStatusUpdate>>,
    pub(crate) expected_step_revisions: Option<BTreeMap<String, u64>>,
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
        // Authenticate the saved representation before retiring executable
        // workflow metadata. Compact snapshots need their reconstructible rows
        // restored, but their historical workflow still participates in CAS.
        let mut historical = lineage.clone();
        historical.workflow = self.lineage.workflow.clone();
        historical.legacy_metadata = self.lineage.legacy_metadata.clone();
        if let Some(revision) = revision
            && (self.revision != revision
                || (plan_revision_with_lineage(Some(plan), &lineage) != revision
                    && plan_revision_with_lineage(Some(plan), &self.lineage) != revision
                    && plan_revision_with_lineage(Some(plan), &historical) != revision
                    && legacy::revision(plan, &self.lineage).as_deref() != Some(revision)
                    && legacy::revision(plan, &historical).as_deref() != Some(revision)))
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

/// Authoritative session-local checklist state.
#[derive(Debug, Default)]
pub(crate) struct PlanStore {
    current: Mutex<PlanState>,
    /// Orders durable publication and client notification without taking a workspace writer.
    publication: Mutex<()>,
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
                })
            })
            .clone()
    }


    pub(crate) async fn snapshot_with_lineage(&self) -> Option<(UpdatePlanArgs, PlanLineage)> {
        let current = self.current.lock().await;
        Some((current.plan.clone()?, current.lineage.clone()))
    }

    #[cfg(test)]
    pub(crate) async fn restore_from_history(&self, items: &[ResponseItem]) -> bool {
        self.restore_reconstructed(None, None, items).await
    }

    /// Replace checklist, lineage and input authority under the same owner lock.
    /// Missing trustworthy provenance must clear an old in-process register.
    pub(crate) async fn restore_reconstructed(
        &self,
        plan: Option<UpdatePlanArgs>,
        lineage: Option<PlanLineage>,

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
        } = args;
        let guard = self.current.lock().await;
        let revision = guard.revision().to_string();
        let unchanged_since_sampling = sampling_revision
            .as_ref()
            .is_some_and(|sampled| std::sync::Arc::ptr_eq(&sampled.0, &guard.derived));
        let missing_revision =
            plan.is_some()
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
                "lineage": guard.lineage.checklist_metadata(),
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
            lineage.revise(current.plan.as_ref(), &plan);
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
        let store = PlanStore::default();
        let initial = json_update(
            &store,
            serde_json::json!({"plan":[{"step":"work","status":"pending"}]}),
        )
        .await;
        let id = initial.lineage.step_id("work");
        let first = store.snapshot().await;
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
        assert_eq!(store.snapshot().await, first);
        json_update(&store, serde_json::json!({"set":[{"step_id":id,"status":"completed"}]})).await;
        assert_eq!(store.snapshot().await.unwrap().plan[0].status, StepStatus::Completed);
        assert!(!std::sync::Arc::ptr_eq(
            &cached,
            &store.current.lock().await.derived
        ));
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
    fn authentic_historical_lineage_is_checked_before_retired_metadata_projection() {
        let text_key = plan_step_id("renamed");
        // Literal historical field order, independent of the compatibility
        // serializer under test (JSON Value sorts these nested object keys).
        let original = format!(r#"[{{"explanation":null,"plan":[{{"step":"renamed","status":"pending"}}]}},{{"requirements":{{"r":{{"text":"renamed","status":"pending"}}}},"step_requirements":{{"old-id":["r"]}},"step_identities":{{"{text_key}":"old-id"}},"workflow":{{"old-id":{{"assignment_id":"assignment","dependencies":[],"capability_profile":"read_search"}}}},"step_revisions":{{"old-id":3}},"acceptance":{{"scope":{{"text":"inspect","origin":{{"turn_id":"u","sha256":"h"}},"requirement":{{"kind":"inspection","environment_id":"local","path":"src.rs","start":0,"end":1}}}}}}}}]"#);
        let values: Vec<serde_json::Value> = serde_json::from_str(&original).unwrap();
        let revision = hex::encode(Sha256::digest(original.as_bytes()));
        let mut response: PlanToolResponse = serde_json::from_value(serde_json::json!({
            "current_plan":values[0], "lineage":values[1], "revision":revision,
        })).unwrap();
        let restored = response.full_lineage_for(&response.current_plan, Some(&revision)).unwrap();
        assert_eq!(restored.step_id("renamed"), "old-id");
        assert_eq!(restored.step_revisions["old-id"], 3);
        assert!(restored.workflow.is_empty() && restored.legacy_metadata.is_empty());
        response.lineage.requirements.get_mut("r").unwrap().text = "tampered".into();
        assert!(response.full_lineage_for(&response.current_plan, Some(&revision)).is_none());
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
        assert!(restored.requirements.is_empty());
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
