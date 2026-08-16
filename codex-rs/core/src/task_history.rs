use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::plan_tool::UpdatePlanArgs;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashSet;
use std::sync::Arc;

const TASK_CHECKPOINT_VERSION: u16 = 1;

#[derive(Clone, Debug)]
pub(crate) struct TaskHistoryCheckpoint {
    rendered: String,
    sha256: String,
    replaces_plan_history: bool,
    covers_progress_narration: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskHistoryReplacement {
    pub(crate) item_index: usize,
    pub(crate) source_sha256: String,
    pub(crate) class: &'static str,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TaskHistoryProjection {
    pub(crate) items: Arc<[ResponseItem]>,
    pub(crate) checkpoint_sha256: Option<String>,
    pub(crate) replacements: Arc<[TaskHistoryReplacement]>,
}

#[derive(Serialize)]
struct CheckpointPayload<'a> {
    version: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_plan: Option<&'a UpdatePlanArgs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decision_ledger: Option<&'a str>,
}

impl TaskHistoryCheckpoint {
    pub(crate) fn new(
        plan: Option<&UpdatePlanArgs>,
        decision_ledger: Option<&str>,
    ) -> Option<Self> {
        let decision_ledger = decision_ledger.filter(|ledger| !ledger.trim().is_empty());
        if plan.is_none() && decision_ledger.is_none() {
            return None;
        }
        let payload = serde_json::to_string(&CheckpointPayload {
            version: TASK_CHECKPOINT_VERSION,
            current_plan: plan,
            decision_ledger,
        })
        .ok()?;
        let sha256 = sha256(payload.as_bytes());
        let rendered = format!(
            "<task_state_checkpoint version=\"{TASK_CHECKPOINT_VERSION}\" sha256=\"{sha256}\">\n{payload}\n</task_state_checkpoint>"
        );
        Some(Self {
            rendered,
            sha256,
            replaces_plan_history: plan.is_some(),
            covers_progress_narration: plan.is_some() && decision_ledger.is_some(),
        })
    }
}

pub(crate) fn project_task_history(
    items: Arc<[ResponseItem]>,
    checkpoint: Option<&TaskHistoryCheckpoint>,
) -> TaskHistoryProjection {
    let Some(checkpoint) = checkpoint else {
        return TaskHistoryProjection {
            items,
            ..Default::default()
        };
    };

    let completed_plan_call_ids = if checkpoint.replaces_plan_history {
        completed_plan_call_ids(&items)
    } else {
        HashSet::new()
    };
    let latest_plan_receipt = items.iter().enumerate().rev().find_map(|(index, item)| {
        matches!(
            item,
            ResponseItem::FunctionCallOutput { call_id, .. }
                if completed_plan_call_ids.contains(call_id)
        )
        .then_some(index)
    });
    let mut projected = Vec::with_capacity(items.len() + 1);
    let mut replacements = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let class = match item {
            ResponseItem::FunctionCall { call_id, .. }
                if completed_plan_call_ids.contains(call_id) =>
            {
                Some("plan_call")
            }
            ResponseItem::FunctionCallOutput { call_id, .. }
                if completed_plan_call_ids.contains(call_id) =>
            {
                Some("plan_output")
            }
            _ if checkpoint.covers_progress_narration
                && latest_plan_receipt.is_some_and(|receipt| index <= receipt)
                && is_assistant_commentary(item) =>
            {
                Some("progress_narration")
            }
            _ => None,
        };
        if let Some(class) = class {
            replacements.push(TaskHistoryReplacement {
                item_index: index,
                source_sha256: item_sha256(item),
                class,
            });
        } else {
            projected.push(item.clone());
        }
    }
    projected.push(ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: checkpoint.rendered.clone(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    TaskHistoryProjection {
        items: projected.into(),
        checkpoint_sha256: Some(checkpoint.sha256.clone()),
        replacements: replacements.into(),
    }
}

fn completed_plan_call_ids(items: &[ResponseItem]) -> HashSet<String> {
    let output_call_ids = items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCallOutput { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCall { name, call_id, .. }
                if name == "update_plan" && output_call_ids.contains(call_id) =>
            {
                Some(call_id.clone())
            }
            _ => None,
        })
        .collect()
}

fn is_assistant_commentary(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::Message {
            role,
            phase: Some(MessagePhase::Commentary),
            ..
        } if role == "assistant"
    )
}

pub(crate) fn items_sha256(items: &[ResponseItem]) -> String {
    serde_json::to_vec(items)
        .map(|bytes| sha256(&bytes))
        .unwrap_or_default()
}

fn item_sha256(item: &ResponseItem) -> String {
    serde_json::to_vec(item)
        .map(|bytes| sha256(&bytes))
        .unwrap_or_default()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::plan_tool::PlanItemArg;
    use codex_protocol::plan_tool::StepStatus;

    fn message(role: &str, text: &str, phase: Option<MessagePhase>) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn call(name: &str, call_id: &str) -> ResponseItem {
        ResponseItem::FunctionCall {
            id: None,
            name: name.to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: call_id.to_string(),
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn output(call_id: &str, text: &str) -> ResponseItem {
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload::from_text(text.to_string()),
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn checkpoint() -> TaskHistoryCheckpoint {
        let plan = UpdatePlanArgs {
            explanation: None,
            plan: vec![PlanItemArg {
                step: "implement projection".to_string(),
                status: StepStatus::InProgress,
                ..Default::default()
            }],
        };
        TaskHistoryCheckpoint::new(Some(&plan), Some("current evidence")).unwrap()
    }

    #[test]
    fn projection_keeps_current_decision_context_and_replaces_completed_plan_history() {
        let items: Arc<[ResponseItem]> = vec![
            message(
                "developer",
                "<collaboration_mode>current settings</collaboration_mode>",
                None,
            ),
            message(
                "assistant",
                "prior completed answer",
                Some(MessagePhase::FinalAnswer),
            ),
            message("user", "first unresolved requirement", None),
            call("inspect", "old-call"),
            output("old-call", "historical output"),
            message(
                "assistant",
                "old progress narration",
                Some(MessagePhase::Commentary),
            ),
            message("user", "latest correction", None),
            message(
                "assistant",
                "current progress narration",
                Some(MessagePhase::Commentary),
            ),
            call("inspect", "current-call"),
            output("current-call", "current evidence"),
            call("update_plan", "plan-call"),
            output("plan-call", "plan accepted"),
        ]
        .into();

        let projection = project_task_history(items, Some(&checkpoint()));
        let rendered = serde_json::to_string(&projection.items).unwrap();

        assert!(rendered.contains("prior completed answer"));
        assert!(rendered.contains("current settings"));
        assert!(rendered.contains("first unresolved requirement"));
        assert!(rendered.contains("latest correction"));
        assert!(rendered.contains("current-call"));
        assert!(rendered.contains("current evidence"));
        assert!(rendered.contains("task_state_checkpoint"));
        assert!(rendered.contains("historical output"));
        assert!(!rendered.contains("old progress narration"));
        assert!(!rendered.contains("current progress narration"));
        assert!(!rendered.contains("plan-call"));
        assert!(
            !projection
                .replacements
                .iter()
                .any(|replacement| replacement.class == "historical_dynamic_history")
        );
        assert!(
            projection
                .replacements
                .iter()
                .any(|replacement| replacement.class == "plan_call")
        );
    }

    #[test]
    fn projection_keeps_commentary_created_after_the_latest_structured_receipt() {
        let items: Arc<[ResponseItem]> = vec![
            message("user", "implement the plan", None),
            message(
                "assistant",
                "progress already captured by the plan",
                Some(MessagePhase::Commentary),
            ),
            call("update_plan", "plan-call"),
            output("plan-call", "plan accepted"),
            message(
                "assistant",
                "new finding after the receipt",
                Some(MessagePhase::Commentary),
            ),
        ]
        .into();

        let projection = project_task_history(items, Some(&checkpoint()));
        let rendered = serde_json::to_string(&projection.items).unwrap();

        assert!(!rendered.contains("progress already captured by the plan"));
        assert!(rendered.contains("new finding after the receipt"));
    }

    #[test]
    fn projection_retains_unresolved_tool_calls_from_older_history() {
        let items: Arc<[ResponseItem]> = vec![
            call("long_running_tool", "pending-call"),
            message("assistant", "completed", Some(MessagePhase::FinalAnswer)),
            message("user", "new requirement", None),
        ]
        .into();

        let projection = project_task_history(items, Some(&checkpoint()));
        let rendered = serde_json::to_string(&projection.items).unwrap();

        assert!(rendered.contains("pending-call"));
        assert!(rendered.contains("new requirement"));
    }

    #[test]
    fn empty_checkpoint_is_not_created() {
        assert!(TaskHistoryCheckpoint::new(None, None).is_none());
    }

    #[test]
    fn plan_without_a_ledger_keeps_progress_narration() {
        let plan = UpdatePlanArgs {
            explanation: None,
            plan: vec![PlanItemArg {
                step: "implement projection".to_string(),
                status: StepStatus::InProgress,
                ..Default::default()
            }],
        };
        let checkpoint = TaskHistoryCheckpoint::new(Some(&plan), None).unwrap();
        let items: Arc<[ResponseItem]> = vec![
            message(
                "assistant",
                "progress not represented by the checkpoint",
                Some(MessagePhase::Commentary),
            ),
            call("update_plan", "plan-call"),
            output("plan-call", "plan accepted"),
        ]
        .into();

        let projection = project_task_history(items, Some(&checkpoint));
        let rendered = serde_json::to_string(&projection.items).unwrap();

        assert!(rendered.contains("progress not represented by the checkpoint"));
        assert!(!rendered.contains("plan-call"));
        assert!(rendered.contains("task_state_checkpoint"));
    }
}
