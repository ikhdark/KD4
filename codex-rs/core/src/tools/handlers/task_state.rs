use crate::function_tool::FunctionCallError;
use crate::task_evidence::ClosureSubmission;
use crate::task_evidence::InvestigationCheckpoint;
use crate::task_evidence::TaskClassification;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::task_state_spec::create_task_state_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::Value;

pub struct TaskStateHandler;

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum TaskStateArgs {
    Classify {
        #[serde(flatten)]
        classification: TaskClassification,
    },
    SubmitInvestigationCheckpoint {
        #[serde(flatten)]
        checkpoint: InvestigationCheckpoint,
    },
    SubmitClosure {
        #[serde(flatten)]
        closure: ClosureSubmission,
    },
    InspectStatus,
}

struct TaskStateOutput(Value);

impl ToolOutput for TaskStateOutput {
    fn log_preview(&self) -> String {
        "task lifecycle state updated".to_string()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        let mut output = FunctionCallOutputPayload::from_text(self.0.to_string());
        output.success = Some(true);
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output,
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> Value {
        self.0.clone()
    }
}

impl ToolExecutor<ToolInvocation> for TaskStateHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("task_state")
    }

    fn spec(&self) -> ToolSpec {
        create_task_state_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let arguments = match &invocation.payload {
                ToolPayload::Function { arguments } => arguments,
                _ => {
                    return Err(FunctionCallError::RespondToModel(
                        "task_state received an unsupported payload".to_string(),
                    ));
                }
            };
            let args: TaskStateArgs = parse_arguments(arguments)?;
            let review_delegate = matches!(
                &invocation.turn.session_source,
                codex_protocol::protocol::SessionSource::SubAgent(
                    codex_protocol::protocol::SubAgentSource::Review
                )
            );
            if review_delegate && !matches!(&args, TaskStateArgs::InspectStatus) {
                return Err(FunctionCallError::RespondToModel(
                    "independent review delegates may only inspect task lifecycle state"
                        .to_string(),
                ));
            }
            let ledger = &invocation.session.services.task_evidence;
            let status = match args {
                TaskStateArgs::Classify { classification } => ledger.classify(classification).await,
                TaskStateArgs::SubmitInvestigationCheckpoint { checkpoint } => {
                    ledger.submit_investigation_checkpoint(checkpoint).await
                }
                TaskStateArgs::SubmitClosure { closure } => ledger.submit_closure(closure).await,
                TaskStateArgs::InspectStatus => ledger
                    .inspect_status()
                    .await
                    .ok_or_else(|| "task evidence is disabled".to_string()),
            }
            .map_err(FunctionCallError::RespondToModel)?;
            let value = serde_json::to_value(status).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to serialize task lifecycle status: {err}"
                ))
            })?;
            Ok(boxed_tool_output(TaskStateOutput(value)))
        })
    }
}

impl CoreToolRuntime for TaskStateHandler {}
