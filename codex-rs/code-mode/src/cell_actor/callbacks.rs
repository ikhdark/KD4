use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing::warn;

use super::CellHost;
use super::CellToolCall;
use crate::TaskFailureHandler;
use crate::runtime::RuntimeCommand;

const CALLBACK_CANCELLATION_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, Copy)]
pub(super) enum CallbackCompletion {
    DrainNotifications,
    Cancel,
}

pub(super) fn spawn_notification<H: CellHost>(
    tasks: &mut JoinSet<()>,
    host: Arc<H>,
    call_id: String,
    text: String,
    cancellation_token: CancellationToken,
    task_failure_handler: Option<TaskFailureHandler>,
) {
    tasks.spawn(async move {
        let callback =
            AssertUnwindSafe(async move { host.notify(call_id, text, cancellation_token).await })
                .catch_unwind()
                .await;
        match callback {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!("failed to deliver code mode notification: {err}"),
            Err(_) => report_task_failure(
                task_failure_handler.as_ref(),
                "code mode notification task panicked".to_string(),
            ),
        }
    });
}

pub(super) fn spawn_tool<H: CellHost>(
    tasks: &mut JoinSet<()>,
    host: Arc<H>,
    invocation: CellToolCall,
    runtime_tx: std::sync::mpsc::Sender<RuntimeCommand>,
    cancellation_token: CancellationToken,
    task_failure_handler: Option<TaskFailureHandler>,
) {
    tasks.spawn(async move {
        let id = invocation.id.clone();
        let tool_name = invocation.name.name.clone();
        let callback =
            AssertUnwindSafe(async move { host.invoke_tool(invocation, cancellation_token).await })
                .catch_unwind()
                .await;
        let wrapper_received_at = std::time::Instant::now();
        let outcome = match &callback {
            Ok(Ok(_)) => "completed",
            Ok(Err(_)) => "failed",
            Err(_) => "panicked",
        };
        info!(
            event.name = "codex.code_mode_nested_tool.wrapper_receipt",
            runtime_tool_call_id = %id,
            tool_name = %tool_name,
            outcome,
            "code mode wrapper received nested tool result"
        );
        let (command, failure_reason) = match callback {
            Ok(Ok(result)) => (
                RuntimeCommand::ToolResponse {
                    id: id.clone(),
                    result,
                },
                None,
            ),
            Ok(Err(error_text)) => (
                RuntimeCommand::ToolError {
                    id: id.clone(),
                    error_text,
                },
                None,
            ),
            Err(_) => {
                let failure_reason = "code mode tool task panicked".to_string();
                (
                    RuntimeCommand::ToolError {
                        id: id.clone(),
                        error_text: failure_reason.clone(),
                    },
                    Some(failure_reason),
                )
            }
        };
        let delivered = runtime_tx.send(command).is_ok();
        info!(
            event.name = "codex.code_mode_nested_tool.tool_result_delivery",
            runtime_tool_call_id = %id,
            tool_name = %tool_name,
            outcome,
            wrapper_to_delivery_ms = u64::try_from(wrapper_received_at.elapsed().as_millis())
                .unwrap_or(u64::MAX),
            delivered,
            "code mode nested tool result delivered to runtime"
        );
        if let Some(failure_reason) = failure_reason {
            report_task_failure(task_failure_handler.as_ref(), failure_reason);
        }
    });
}

pub(super) async fn finish_callbacks(
    cancellation_token: &CancellationToken,
    notification_tasks: &mut JoinSet<()>,
    tool_tasks: &mut JoinSet<()>,
    completion: CallbackCompletion,
    task_failure_handler: Option<&TaskFailureHandler>,
) {
    if matches!(completion, CallbackCompletion::Cancel) {
        cancellation_token.cancel();
    }
    drain_tasks_bounded(notification_tasks, "notification", task_failure_handler).await;
    cancellation_token.cancel();
    drain_tasks_bounded(tool_tasks, "tool", task_failure_handler).await;
}

pub(super) fn report_task_result(
    task_result: Option<Result<(), tokio::task::JoinError>>,
    description: &str,
    task_failure_handler: Option<&TaskFailureHandler>,
) {
    if let Some(Err(err)) = task_result
        && !err.is_cancelled()
    {
        report_task_failure(
            task_failure_handler,
            format!("code mode {description} task failed: {err}"),
        );
    }
}

fn report_task_failure(task_failure_handler: Option<&TaskFailureHandler>, failure_reason: String) {
    warn!("{failure_reason}");
    if let Some(task_failure_handler) = task_failure_handler {
        task_failure_handler(failure_reason);
    }
}

async fn drain_tasks(
    tasks: &mut JoinSet<()>,
    description: &str,
    task_failure_handler: Option<&TaskFailureHandler>,
) {
    while let Some(result) = tasks.join_next().await {
        report_task_result(Some(result), description, task_failure_handler);
    }
}

async fn drain_tasks_bounded(
    tasks: &mut JoinSet<()>,
    description: &str,
    task_failure_handler: Option<&TaskFailureHandler>,
) {
    if tasks.is_empty() {
        return;
    }
    if tokio::time::timeout(
        CALLBACK_CANCELLATION_GRACE,
        drain_tasks(tasks, description, task_failure_handler),
    )
    .await
    .is_ok()
    {
        return;
    }

    let failure_reason = format!(
        "code mode {description} callback cleanup exceeded {}ms; aborting remaining callbacks",
        CALLBACK_CANCELLATION_GRACE.as_millis(),
    );
    warn!("{failure_reason}");
    if let Some(task_failure_handler) = task_failure_handler {
        task_failure_handler(failure_reason);
    }
    tasks.abort_all();
    drain_tasks(tasks, description, task_failure_handler).await;
}

#[cfg(test)]
#[path = "callbacks_tests.rs"]
mod tests;
