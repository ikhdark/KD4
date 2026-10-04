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
#[derive(Clone)]
pub(crate) struct TaskState(Value);

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct TaskStateSnapshot {
    state: Value,
}

impl TaskState {
    pub(crate) fn new(plan: Option<(UpdatePlanArgs, PlanLineage)>) -> Self {
        let mut state = match plan {
            Some((plan, lineage)) => serde_json::json!({
                "has_plan": true,
                "revision": crate::plan_store::plan_revision_with_lineage(Some(&plan), &lineage),
                "step_ids": plan.plan.iter().map(|step| lineage.step_id(&step.step)).collect::<Vec<_>>(),
                "obligations": lineage.obligation_summary(&plan),
                "current_plan": plan,
                "lineage": lineage,
            }),
            None => serde_json::json!({"has_plan": false}),
        };
        super::remove_null_object_fields(&mut state);
        Self(state)
    }

    pub(crate) fn with_effect_recovery(mut self, recovery: Option<Value>) -> Self {
        if let Some(mut recovery) = recovery {
            super::remove_null_object_fields(&mut recovery);
            self.0["effect_recovery"] = recovery;
        }
        self
    }
}

impl WorldStateSection for TaskState {
    const ID: &'static str = "task_state";
    type Snapshot = TaskStateSnapshot;

    fn snapshot(&self) -> TaskStateSnapshot {
        TaskStateSnapshot { state: self.0.clone() }
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
            PreviousSectionState::Known(previous) if previous.state == self.0 => None,
            PreviousSectionState::Absent if self.0["has_plan"] == false
                && self.0.get("effect_recovery").is_none() => None,
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
        let mut body = "\nCurrent stored task state; replaces earlier task-state snapshots. This is a checklist and obligation ledger, not proof of completion or permission to narrow the user's request. Later user instructions remain authoritative.\n".to_string();
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
