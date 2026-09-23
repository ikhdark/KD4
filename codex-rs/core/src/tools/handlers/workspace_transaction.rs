use crate::FunctionCallError;
use crate::tools::context::{ToolInvocation, ToolPayload, boxed_tool_output};
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::{CoreToolRuntime, ToolExecutor};
use codex_tools::{JsonSchema, JsonToolOutput, ResponsesApiTool, ToolName, ToolSpec};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) struct WorkspaceTransactionHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: Action,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Begin,
    Status,
    Reconcile,
}

impl ToolExecutor<ToolInvocation> for WorkspaceTransactionHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("workspace_transaction")
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "workspace_transaction".into(),
            description: "Isolate a local Git editing task. Call begin before reading source for edits: it captures tracked and nonignored untracked regular files, including uncommitted changes, in a task-owned checkout. Read/edit/exec tools then use that checkout; use its paths inside shell text. Builds/tests through exec_command capture a separate input snapshot with their own output directory. Call reconcile after edits and validation to three-way merge back, preserving independent changes; conflicts publish nothing and retain the task workspace. Reconciliation never stages or commits in the original checkout. Status recovers paths after resume. Requires local unrestricted filesystem access; links/submodules and snapshots over 1 GiB are rejected. Ignored files and external dependencies are not captured. Snapshots are retained for recovery.".into(),
            strict: false, defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("action".into(), JsonSchema::string(Some("begin, status, or reconcile".into()))),
            ]), Some(vec!["action".into()]), Some(false.into())),
            output_schema: None,
        })
    }
    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "workspace_transaction requires function arguments".into(),
                ));
            };
            let args: Args = parse_arguments(arguments)?;
            let turn = &invocation.step_context.turn;
            if !matches!(
                turn.sandbox_policy(),
                codex_protocol::protocol::SandboxPolicy::DangerFullAccess
            ) || invocation
                .step_context
                .environments
                .primary()
                .is_none_or(|env| env.environment.is_remote())
            {
                return Err(FunctionCallError::RespondToModel(
                    "workspace transactions require a local unrestricted filesystem environment"
                        .into(),
                ));
            }
            if invocation.cancellation_token.is_cancelled() {
                return Err(FunctionCallError::RespondToModel(
                    "workspace transaction cancelled before execution".into(),
                ));
            }
            let home = turn.config.codex_home.clone();
            let thread = invocation.session.thread_id.to_string();
            let cwd = turn.config.cwd.to_path_buf();
            let session = invocation.session.clone();
            let tracker = invocation.tracker.clone();
            // Own the mutation and its invalidation through completion even if
            // the calling tool waiter is interrupted.
            invocation.session.terminal_tasks.spawn(async move {
                let mutation = matches!(args.action, Action::Reconcile);
                let result = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
                    use crate::workspace_transaction as workspace;
                    match args.action {
                        Action::Begin => {
                            let tx = workspace::begin(&home, &thread, &cwd)?;
                            Ok(json!({"active": true, "origin": tx.origin, "workdir": tx.workdir, "source_revision": tx.revision, "input_files": tx.files.len()}))
                        }
                        Action::Status => Ok(match workspace::load(&home, &thread)? {
                            Some(tx) => json!({"active": !tx.reconciled, "origin": tx.origin, "workdir": tx.workdir, "source_revision": tx.revision}),
                            None => json!({"active": false}),
                        }),
                        Action::Reconcile => {
                            let mut value = serde_json::to_value(workspace::reconcile(&home, &thread)?)?;
                            if value["merged"] == true {
                                value["next_action"] = json!("Run the affected checks against the integrated original checkout before reporting completion. Pre-merge test results do not validate the combined source.");
                            }
                            Ok(value)
                        }
                    }
                }).await.map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
                if mutation {
                    // Includes partial I/O failure: absence of a complete receipt
                    // must never make old source/test evidence current again.
                    session.services.git_workspace.note_host_workspace_mutation();
                    tracker.lock().await.record_unknown_mutation();
                }
                let value = result.map_err(|e| FunctionCallError::RespondToModel(format!("workspace transaction: {e:#}")))?;
                if value.get("merged") == Some(&json!(false)) {
                    return Err(FunctionCallError::RespondToModel(format!("Reconciliation conflicts; no origin files changed. Resolve in the retained task workspace and retry: {value}")));
                }
                Ok(boxed_tool_output(JsonToolOutput::new(value)))
            }).await.map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?
        })
    }
}
impl CoreToolRuntime for WorkspaceTransactionHandler {}
