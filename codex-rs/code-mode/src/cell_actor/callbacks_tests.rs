use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::cell_actor::CellState;
use crate::cell_actor::CompletionCommit;
use crate::runtime::RuntimeCommand;
use crate::session_runtime::CellEvent;
use crate::session_runtime::ToolKind;
use crate::session_runtime::ToolName;

struct PanickingCallbackHost;

struct NonCooperativeCallbackHost;

impl CellHost for PanickingCallbackHost {
    async fn invoke_tool(
        &self,
        _invocation: CellToolCall,
        _cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        panic!("tool callback panic probe");
    }

    async fn notify(
        &self,
        _call_id: String,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> Result<(), String> {
        panic!("notification callback panic probe");
    }

    async fn commit_completion(
        &self,
        _stored_value_writes: HashMap<String, JsonValue>,
        _event: CellEvent,
        _pending_initial_yield_items: Option<Vec<crate::session_runtime::OutputItem>>,
        _cell_state: Arc<CellState>,
    ) -> CompletionCommit {
        panic!("unexpected completion commit");
    }

    async fn closed(&self, _event: Option<CellEvent>) {}
}

impl CellHost for NonCooperativeCallbackHost {
    async fn invoke_tool(
        &self,
        _invocation: CellToolCall,
        _cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        std::future::pending().await
    }

    async fn notify(
        &self,
        _call_id: String,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> Result<(), String> {
        std::future::pending().await
    }

    async fn commit_completion(
        &self,
        _stored_value_writes: HashMap<String, JsonValue>,
        _event: CellEvent,
        _pending_initial_yield_items: Option<Vec<crate::session_runtime::OutputItem>>,
        _cell_state: Arc<CellState>,
    ) -> CompletionCommit {
        panic!("unexpected completion commit");
    }

    async fn closed(&self, _event: Option<CellEvent>) {}
}

#[tokio::test]
async fn tool_callback_panic_rejects_the_js_promise_and_reports_failure() {
    let mut tasks = JoinSet::new();
    let (runtime_tx, runtime_rx) = std_mpsc::channel();
    let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
    spawn_tool(
        &mut tasks,
        Arc::new(PanickingCallbackHost),
        CellToolCall {
            id: "tool-1".to_string(),
            name: ToolName {
                name: "panic".to_string(),
                namespace: None,
            },
            kind: ToolKind::Function,
            input: None,
            timeout: Duration::from_secs(1),
        },
        runtime_tx,
        CancellationToken::new(),
        Some(Arc::new(move |reason| {
            let _ = failure_tx.send(reason);
        })),
    );

    tasks
        .join_next()
        .await
        .expect("tool callback task")
        .expect("tool callback wrapper");
    let command = runtime_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("tool error command");
    let RuntimeCommand::ToolError { id, error_text } = command else {
        panic!("expected a tool error command");
    };
    assert_eq!(id, "tool-1");
    assert_eq!(error_text, "code mode tool task panicked");
    assert_eq!(failure_rx.recv().await, Some(error_text));
}

#[tokio::test(start_paused = true)]
async fn tool_callback_timeout_rejects_the_js_promise_and_cancels_the_delegate() {
    let mut tasks = JoinSet::new();
    let (runtime_tx, runtime_rx) = std_mpsc::channel();
    let cancellation_token = CancellationToken::new();
    spawn_tool(
        &mut tasks,
        Arc::new(NonCooperativeCallbackHost),
        CellToolCall {
            id: "tool-timeout".to_string(),
            name: ToolName {
                name: "stuck".to_string(),
                namespace: None,
            },
            kind: ToolKind::Function,
            input: None,
            timeout: Duration::from_secs(1),
        },
        runtime_tx,
        cancellation_token.clone(),
        None,
    );

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    tasks
        .join_next()
        .await
        .expect("tool callback task")
        .expect("tool callback wrapper");

    let RuntimeCommand::ToolError { id, error_text } = runtime_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("tool timeout command")
    else {
        panic!("expected a tool error command");
    };
    assert_eq!(id, "tool-timeout");
    assert_eq!(
        error_text,
        "nested tool `stuck` exceeded its 1000ms timeout"
    );
    assert!(cancellation_token.is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn notification_timeout_rejects_the_js_promise_and_cancels_the_delegate() {
    let mut tasks = JoinSet::new();
    let (runtime_tx, runtime_rx) = std_mpsc::channel();
    let cancellation_token = CancellationToken::new();
    spawn_notification(
        &mut tasks,
        Arc::new(NonCooperativeCallbackHost),
        NotificationInvocation {
            id: Some("notify-timeout".to_string()),
            call_id: "call-1".to_string(),
            text: "hello".to_string(),
        },
        runtime_tx,
        cancellation_token.clone(),
        None,
    );

    tokio::task::yield_now().await;
    tokio::time::advance(NOTIFICATION_DELIVERY_TIMEOUT).await;
    tasks
        .join_next()
        .await
        .expect("notification callback task")
        .expect("notification callback wrapper");

    let RuntimeCommand::NotificationError { id, error_text } = runtime_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("notification timeout command")
    else {
        panic!("expected a notification error command");
    };
    assert_eq!(id, "notify-timeout");
    assert_eq!(
        error_text,
        "code mode notification exceeded its 60000ms timeout"
    );
    assert!(cancellation_token.is_cancelled());
}

#[tokio::test]
async fn notification_callback_panic_reports_failure() {
    let mut tasks = JoinSet::new();
    let (runtime_tx, runtime_rx) = std_mpsc::channel();
    let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
    spawn_notification(
        &mut tasks,
        Arc::new(PanickingCallbackHost),
        NotificationInvocation {
            id: Some("notify-1".to_string()),
            call_id: "call-1".to_string(),
            text: "hello".to_string(),
        },
        runtime_tx,
        CancellationToken::new(),
        Some(Arc::new(move |reason| {
            let _ = failure_tx.send(reason);
        })),
    );

    tasks
        .join_next()
        .await
        .expect("notification callback task")
        .expect("notification callback wrapper");
    let failure_reason = failure_rx.recv().await.expect("notification failure");
    assert_eq!(failure_reason, "code mode notification task panicked");
    let RuntimeCommand::NotificationError { id, error_text } = runtime_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("notification error command")
    else {
        panic!("expected a notification error command");
    };
    assert_eq!(id, "notify-1");
    assert_eq!(error_text, failure_reason);
}

#[tokio::test]
async fn callback_wrapper_join_error_reports_failure() {
    let task_result = tokio::spawn(async {
        panic!("callback wrapper panic probe");
    })
    .await;
    let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
    let task_failure_handler: TaskFailureHandler = Arc::new(move |reason| {
        let _ = failure_tx.send(reason);
    });

    report_task_result(Some(task_result), "tool", Some(&task_failure_handler));

    let failure_reason = failure_rx.recv().await.expect("wrapper failure");
    assert!(failure_reason.contains("code mode tool task failed"));
}

#[tokio::test(start_paused = true)]
async fn cancellation_aborts_non_cooperative_callback_after_bounded_grace() {
    let mut notification_tasks = JoinSet::new();
    let mut tool_tasks = JoinSet::new();
    let notification_cancellation_token = CancellationToken::new();
    let tool_cancellation_token = CancellationToken::new();
    let (runtime_tx, _runtime_rx) = std_mpsc::channel();
    let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
    let task_failure_handler: TaskFailureHandler = Arc::new(move |reason| {
        let _ = failure_tx.send(reason);
    });

    spawn_tool(
        &mut tool_tasks,
        Arc::new(NonCooperativeCallbackHost),
        CellToolCall {
            id: "tool-stuck".to_string(),
            name: ToolName {
                name: "stuck".to_string(),
                namespace: None,
            },
            kind: ToolKind::Function,
            input: None,
            timeout: Duration::from_secs(60),
        },
        runtime_tx,
        tool_cancellation_token.child_token(),
        Some(task_failure_handler.clone()),
    );

    let cleanup = tokio::spawn(async move {
        finish_callbacks(
            &notification_cancellation_token,
            &tool_cancellation_token,
            &mut notification_tasks,
            &mut tool_tasks,
            CallbackCompletion::Cancel,
            Some(&task_failure_handler),
        )
        .await;
    });
    tokio::task::yield_now().await;
    tokio::time::advance(CALLBACK_CANCELLATION_GRACE).await;
    tokio::task::yield_now().await;
    cleanup
        .await
        .expect("bounded callback cleanup should finish");

    let failure = failure_rx.recv().await.expect("timeout diagnostic");
    assert!(failure.contains("callback cleanup exceeded"));
}
