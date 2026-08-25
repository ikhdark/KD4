use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::UpdatePlanArgs;
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
    left.explanation == right.explanation
        && left.plan.len() == right.plan.len()
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
        && left.requires_desktop_activation == right.requires_desktop_activation
        && left.validation_route == right.validation_route
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
