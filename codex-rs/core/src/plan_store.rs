use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::UpdatePlanArgs;
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
    pub(crate) fn requests_generation(self) -> bool {
        matches!(self, Self::Initial | Self::StructuralRevision)
    }

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
}

/// Authoritative session-local TODO/checklist state when durable task evidence
/// is not enabled.
///
/// Durable task evidence owns its plan separately; ordinary plan updates use
/// this store without depending on that subsystem.
#[derive(Debug, Default)]
pub(crate) struct PlanStore {
    current: Mutex<Option<UpdatePlanArgs>>,
}

impl PlanStore {
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
            let FunctionCallOutputBody::Text(text) = &output.body else {
                return None;
            };
            let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
            serde_json::from_value(value.get("current_plan")?.clone()).ok()
        });
        let Some(restored) = restored else {
            return false;
        };
        *self.current.lock().await = Some(restored);
        true
    }

    pub(crate) async fn update(&self, next: UpdatePlanArgs) -> PlanStoreUpdate {
        let mut current = self.current.lock().await;
        let effect = match current.as_ref() {
            None => PlanUpdateEffect::Initial,
            Some(previous) if previous == &next => PlanUpdateEffect::NoOp,
            Some(previous) if same_structure(previous, &next) => PlanUpdateEffect::StatusOnly,
            Some(_) => PlanUpdateEffect::StructuralRevision,
        };
        *current = Some(next.clone());
        PlanStoreUpdate {
            current: next,
            effect,
        }
    }

    #[cfg(test)]
    pub(crate) async fn current_for_test(&self) -> Option<UpdatePlanArgs> {
        self.current.lock().await.clone()
    }
}

fn same_structure(left: &UpdatePlanArgs, right: &UpdatePlanArgs) -> bool {
    left.plan.len() == right.plan.len()
        && left
            .plan
            .iter()
            .zip(&right.plan)
            .all(|(left, right)| same_item_structure(left, right))
}

fn same_item_structure(left: &PlanItemArg, right: &PlanItemArg) -> bool {
    left.id == right.id
        && left.step == right.step
        && left.depends_on == right.depends_on
        && left.acceptance_criteria == right.acceptance_criteria
        && left.runtime_paths == right.runtime_paths
        && left.generated_artifacts == right.generated_artifacts
        && left.risks == right.risks
        && left.validation_route == right.validation_route
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
                id: Some("step-1".to_string()),
                step: step.to_string(),
                status,
                ..Default::default()
            }],
        }
    }

    #[tokio::test]
    async fn classifies_straight_line_plan_updates_without_task_evidence() {
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
    async fn explanation_only_update_does_not_request_generation() {
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
        assert!(!effect.requests_generation());
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
}
