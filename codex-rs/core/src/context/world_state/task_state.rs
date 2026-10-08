use super::PreviousSectionState;
use super::WorldStateSection;
use crate::context::ContextualUserFragment;
use crate::context::environment_context::push_xml_escaped_text;
use crate::plan_store::PlanLineage;
use codex_protocol::plan_tool::UpdatePlanArgs;
use serde_json::Value;

/// Recompile ordinary task state from its owner, not from a prose summary.
/// Delivery tracking lets unchanged state stay out of subsequent prompts while
/// restoring the obligations when their last delivered fragment is compacted.
#[derive(Clone, Debug)]
pub(crate) struct TaskState(std::sync::Arc<Value>);

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct TaskStateSnapshot {
    state: Value,
}

impl TaskState {
    #[cfg(test)]
    pub(crate) fn new(plan: Option<(UpdatePlanArgs, PlanLineage)>) -> Self {
        let revision = plan.as_ref().map(|(plan, lineage)|
            crate::plan_store::plan_revision_with_lineage(Some(plan), lineage));
        Self::from_plan(plan.as_ref().zip(revision.as_deref()).map(|((plan, lineage), revision)| (plan, lineage, revision)))
    }

    pub(crate) fn from_plan(plan: Option<(&UpdatePlanArgs, &PlanLineage, &str)>) -> Self {
        let mut state = match plan {
            Some((plan, lineage, revision)) => serde_json::json!({
                "has_plan": true,
                "revision": revision,
                "step_ids": plan.plan.iter().map(|step| lineage.step_id(&step.step)).collect::<Vec<_>>(),
                "obligations": lineage.obligation_summary(plan),
                "current_plan": plan,
                "lineage": lineage.active_for_plan(plan),
            }),
            None => serde_json::json!({"has_plan": false}),
        };
        super::remove_null_object_fields(&mut state);
        Self(std::sync::Arc::new(state))
    }

    pub(crate) fn with_execution_suspended(mut self, suspended: bool) -> Self {
        if self.0["has_plan"] == true {
            std::sync::Arc::make_mut(&mut self.0)["execution_suspended"] = suspended.into();
        }
        self
    }
}

impl WorldStateSection for TaskState {
    const ID: &'static str = "task_state";
    type Snapshot = TaskStateSnapshot;

    fn snapshot(&self) -> TaskStateSnapshot {
        TaskStateSnapshot { state: (*self.0).clone() }
    }

    fn required() -> bool {
        true
    }

    fn records_delivery() -> bool {
        true
    }

    fn has_retained_fragment_matcher() -> bool {
        true
    }

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "user" && Self::matches_text(text)
    }

    fn matches_retained_fragment(role: &str, text: &str) -> bool {
        role == "user" && Self::matches_text(text)
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, TaskStateSnapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        match previous {
            PreviousSectionState::Known(previous) if previous.state == *self.0 => None,
            PreviousSectionState::Absent if self.0["has_plan"] == false => None,
            _ => Some(Box::new(self.clone())),
        }
    }
}

impl ContextualUserFragment for TaskState {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<codex_task_state>", "</codex_task_state>")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        let mut body = "\nCurrent stored task state; replaces earlier task-state snapshots. This is a checklist and obligation ledger, not proof of completion or permission to narrow the user's request. Later user instructions remain authoritative. When execution_suspended is true, this is retained implementation state, not debt of the current planning deliverable; do not repair or complete it in Plan Mode. Retired requirement details remain in durable plan history.\n".to_string();
        push_xml_escaped_text(&mut body, &self.0.to_string());
        body.push('\n');
        body.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ContentItem;
    use codex_protocol::plan_tool::PlanItemArg;
    use codex_protocol::plan_tool::StepStatus;

    #[tokio::test]
    async fn continuity_cached_task_views_match_owner_and_detach_on_change() {
        let store = crate::plan_store::PlanStore::default();
        let plan = UpdatePlanArgs { explanation: None, plan: vec![PlanItemArg {
            step: "retain obligations".into(), status: StepStatus::Pending,
        }] };
        store.restore(Some(plan.clone())).await;
        let first = store.task_state(false).await;
        let again = store.task_state(false).await;
        assert!(std::sync::Arc::ptr_eq(&first.0, &again.0));
        let expected = TaskState::new(store.snapshot_with_lineage().await).with_execution_suspended(false);
        assert_eq!(first.0, expected.0);
        let suspended = store.task_state(true).await;
        assert_eq!(suspended.0["execution_suspended"], true);
        assert_eq!(first.0["execution_suspended"], false);
        assert_eq!(suspended.0["revision"], first.0["revision"]);
        let before = store.execution_snapshot().await.unwrap();
        let mut changed = plan;
        changed.plan[0].status = StepStatus::Completed;
        store.restore(Some(changed)).await;
        let next = store.task_state(false).await;
        assert!(!std::sync::Arc::ptr_eq(&first.0, &next.0));
        assert_ne!(next.0["revision"], first.0["revision"]);
        assert_ne!(store.execution_snapshot().await.unwrap(), before);
        assert_eq!(first.0["current_plan"]["plan"][0]["status"], "pending");
        store.restore(None).await;
        assert_eq!(store.task_state(false).await.0["has_plan"], false);
        assert!(store.execution_snapshot().await.is_none());
    }

    #[tokio::test]
    async fn suspended_execution_context_retains_unresolved_work_not_retired_details() {
        let store = crate::plan_store::PlanStore::default();
        let plan = UpdatePlanArgs { explanation: None, plan: vec![PlanItemArg {
            step: "implementation".into(), status: StepStatus::Pending,
        }] };
        let mut lineage = PlanLineage::default();
        lineage.requirements.insert("retired".into(), crate::plan_store::PlanRequirement {
            text: "historical completed detail".into(), status: StepStatus::Completed, superseded_reason: None,
        });
        lineage.requirements.insert("orphan".into(), crate::plan_store::PlanRequirement {
            text: "still required".into(), status: StepStatus::Pending, superseded_reason: None,
        });
        store.restore_with_lineage(Some(plan), Some(lineage)).await;
        let state = TaskState::new(store.snapshot_with_lineage().await).with_execution_suspended(true);
        assert_eq!(state.0["execution_suspended"], true);
        assert!(state.0["lineage"]["requirements"].get("retired").is_none());
        assert_eq!(state.0["obligations"]["unresolved"].as_array().unwrap().len(), 2);
        assert!(state.body().contains("not debt of the current planning deliverable"));
        assert!(store.snapshot_with_lineage().await.unwrap().1.requirements.contains_key("retired"));
    }

    #[test]
    fn task_state_is_context_not_a_new_request_and_survives_delivery_metadata() {
        let state = TaskState::new(Some((UpdatePlanArgs {
            explanation: None,
            plan: vec![PlanItemArg {
                step: "Preserve </codex_task_state> obligations".into(),
                status: StepStatus::Pending,
            }],
        }, PlanLineage::default())));
        let fragment = state.render_diff(PreviousSectionState::Absent).unwrap().render();
        assert_eq!(state.0["obligations"]["completed"], 0);
        assert!(state.0["lineage"]["requirements"].as_object().unwrap().is_empty());
        assert_eq!(state.0["obligations"]["unresolved"].as_array().unwrap().len(), 1);
        assert!(fragment.contains("&lt;/codex_task_state&gt;"));
        assert!(crate::context::is_contextual_user_fragment(&ContentItem::InputText {
            text: fragment,
        }));
        let mut snapshot = serde_json::to_value(WorldStateSection::snapshot(&state)).unwrap();
        snapshot["_retained_delivery"] = "host-delivery-digest".into();
        let previous: TaskStateSnapshot = serde_json::from_value(snapshot).unwrap();
        assert!(state.render_diff(PreviousSectionState::Known(&previous)).is_none());
        assert!(state.render_diff(PreviousSectionState::Unknown).is_some());
        assert!(TaskState::new(None).render_diff(PreviousSectionState::Absent).is_none());
    }
}
