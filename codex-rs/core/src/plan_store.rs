use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use serde::Deserialize;
use serde::Serialize;
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanStatusUpdate {
    pub(crate) index: usize,
    pub(crate) status: StepStatus,
}

/// Persisted `update_plan` response shared by rendering and legacy history replay.
/// The plan field is required; display metadata may be absent in older histories.
#[derive(Serialize, Deserialize)]
pub(crate) struct PlanToolResponse {
    pub(crate) current_plan: UpdatePlanArgs,
    #[serde(default)]
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) effect: String,
    #[serde(default)]
    pub(crate) no_progress: bool,
}

pub(crate) fn plan_from_tool_output(output: &FunctionCallOutputPayload) -> Option<UpdatePlanArgs> {
    if output.success == Some(false) {
        return None;
    }
    let FunctionCallOutputBody::Text(text) = &output.body else {
        return None;
    };
    serde_json::from_str::<PlanToolResponse>(text)
        .ok()
        .map(|response| response.current_plan)
}

/// Authoritative session-local TODO/checklist state.
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
            plan_from_tool_output(output)
        });
        let found = restored.is_some();
        self.restore(restored).await;
        found
    }

    pub(crate) async fn restore(&self, plan: Option<UpdatePlanArgs>) {
        *self.current.lock().await = plan;
    }

    pub(crate) async fn update(&self, next: UpdatePlanArgs) -> PlanStoreUpdate {
        let mut current = self.current.lock().await;
        Self::commit(&mut current, next)
    }

    pub(crate) async fn update_statuses(
        &self,
        updates: Vec<PlanStatusUpdate>,
        explanation: Option<String>,
    ) -> Result<PlanStoreUpdate, String> {
        let mut current = self.current.lock().await;
        let mut next = current
            .clone()
            .ok_or("create a plan before updating statuses")?;
        if updates.is_empty() {
            return Err("set must contain at least one status update".to_string());
        }
        let mut seen = HashSet::new();
        for update in updates {
            if !seen.insert(update.index) {
                return Err(format!("duplicate plan index {}", update.index));
            }
            let item = next
                .plan
                .get_mut(update.index)
                .ok_or_else(|| format!("plan index {} is out of range", update.index))?;
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
        Ok(Self::commit(&mut current, next))
    }

    fn commit(current: &mut Option<UpdatePlanArgs>, next: UpdatePlanArgs) -> PlanStoreUpdate {
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
    left.step == right.step
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
