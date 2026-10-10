use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use crate::NestedCancellation;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::cell_actor::CellState;
use crate::cell_actor::CompletionCommit;
use crate::runtime::RuntimeCommand;
use crate::session_runtime::CellEvent;
use crate::session_runtime::ToolKind;
use crate::session_runtime::ToolName;

struct PanickingCallbackHost;

#[derive(Default)]
struct NonCooperativeCallbackHost {
    notify_token: std::sync::Mutex<Option<CancellationToken>>,
}

struct SlowSetupCallbackHost {
    setup: Duration,
    ready: std_mpsc::Sender<Option<std::time::Instant>>,
    completed: Option<JsonValue>,
}

impl CellHost for SlowSetupCallbackHost {
    fn invoke_tool(
        &self,
        invocation: CellToolCall,
        _cancellation: NestedCancellation,
    ) -> impl std::future::Future<Output = Result<JsonValue, String>> + Send {
        // Future construction can do synchronous work before either select
        // branch is polled. It must not start a second timeout budget.
        std::thread::sleep(self.setup);
        self.ready.send(invocation.deadline).unwrap();
        let completed = self.completed.clone();
        async move {
            match completed {
                Some(value) => Ok(value),
                None => std::future::pending().await,
            }
        }
    }

    async fn notify(&self, _: String, _: String, _: CancellationToken) -> Result<(), String> {
        panic!("unexpected notification");
    }

    async fn commit_completion(
        &self,
        _: HashMap<String, crate::runtime::StoredValue>,
        _: CellEvent,
        _: Option<Vec<crate::session_runtime::OutputItem>>,
        _: Arc<CellState>,
    ) -> CompletionCommit {
        panic!("unexpected completion commit");
    }

    async fn closed(&self, _: Option<CellEvent>) {}
}

#[tokio::test]
async fn callback_setup_does_not_restart_the_published_deadline() {
    for timeout in [Duration::from_millis(150), Duration::ZERO] {
        let mut tasks = JoinSet::new();
        let (runtime_tx, runtime_rx) = std_mpsc::channel();
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let cancellation = NestedCancellation::new(CancellationToken::new());
        let setup = Duration::from_millis(200);
        let started = std::time::Instant::now();
        spawn_tool(
            &mut tasks,
            Arc::new(SlowSetupCallbackHost {
                setup,
                ready: ready_tx,
                completed: timeout.is_zero().then(|| serde_json::json!({"retained": "done"})),
            }),
            CellToolCall {
                id: "slow-setup".into(),
                name: ToolName { name: "fixture".into(), namespace: None },
                kind: ToolKind::Function,
                input: None,
                timeout,
                deadline: None,
                buffered_output_bytes: 0,
            },
            runtime_tx,
            cancellation.clone(),
            None,
        );
        tasks.join_next().await.unwrap().unwrap();
        let elapsed = started.elapsed();
        let deadline = ready_rx.try_recv().unwrap();
        let command = runtime_rx.try_recv().unwrap();
        assert!(tasks.is_empty());
        assert!(runtime_rx.try_recv().is_err(), "one invocation must yield only one receipt");
        if timeout.is_zero() {
            assert!(deadline.is_none());
            assert!(!cancellation.token().is_cancelled());
            assert!(matches!(command, RuntimeCommand::ToolResponse { result, .. }
                if result == serde_json::json!({"retained": "done"})));
        } else {
            assert!(matches!(command, RuntimeCommand::ToolError { error_text, .. }
                if error_text.contains("150ms timeout")));
            assert!(cancellation.token().is_cancelled());
            let excess = elapsed.saturating_sub(setup);
            eprintln!("callback_setup_deadline wall_us={} setup_us={} extra_wait_us={} deadline_overrun_us={}",
                elapsed.as_micros(), setup.as_micros(), excess.as_micros(), deadline.unwrap().elapsed().as_micros());
            // Synchronous setup itself cannot be preempted, but after it yields
            // an expired deadline must not add another full 150 ms wait.
            assert!(excess < Duration::from_millis(100), "restarted callback timeout: {elapsed:?}");
        }
    }
}

impl CellHost for PanickingCallbackHost {
    async fn invoke_tool(
        &self,
        _invocation: CellToolCall,
        _cancellation_token: NestedCancellation,
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
        _stored_value_writes: HashMap<String, crate::runtime::StoredValue>,
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
        _cancellation_token: NestedCancellation,
    ) -> Result<JsonValue, String> {
        std::future::pending().await
    }

    async fn notify(
        &self,
        _call_id: String,
        _text: String,
        cancellation_token: CancellationToken,
    ) -> Result<(), String> {
        *self.notify_token.lock().unwrap() = Some(cancellation_token);
        std::future::pending().await
    }

    async fn commit_completion(
        &self,
        _stored_value_writes: HashMap<String, crate::runtime::StoredValue>,
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
            deadline: None,
            buffered_output_bytes: 0,
        },
        runtime_tx,
        NestedCancellation::new(CancellationToken::new()),
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
    for namespace in [None, Some("agents".to_string())] {
        let mut tasks = JoinSet::new();
        let (runtime_tx, runtime_rx) = std_mpsc::channel();
        let cancellation_token = CancellationToken::new();
        spawn_tool(
            &mut tasks,
            Arc::new(NonCooperativeCallbackHost::default()),
            CellToolCall {
                id: "tool-timeout".to_string(),
                name: ToolName {
                    name: "stuck".to_string(),
                    namespace: namespace.clone(),
                },
                kind: ToolKind::Function,
                input: None,
                timeout: Duration::from_secs(1),
                deadline: None,
                buffered_output_bytes: 0,
            },
            runtime_tx,
            NestedCancellation::new(cancellation_token.clone()),
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
            format!("nested tool `{}` exceeded its 1000ms timeout",
                namespace.map_or("stuck".to_string(), |namespace| format!("{namespace}__stuck")))
        );
        assert!(cancellation_token.is_cancelled());
    }
}

#[tokio::test(start_paused = true)]
async fn notification_timeout_rejects_the_js_promise_and_cancels_the_delegate() {
    let mut tasks = JoinSet::new();
    let (runtime_tx, runtime_rx) = std_mpsc::channel();
    let cancellation_token = CancellationToken::new();
    let host = Arc::new(NonCooperativeCallbackHost::default());
    spawn_notification(
        &mut tasks,
        Arc::clone(&host),
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
    // The delegate holds its own delivery token, not the cell's.
    let delivery_token = host.notify_token.lock().unwrap().clone().expect("delegate was notified");
    assert!(delivery_token.is_cancelled());
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
    let tool_cancellation = NestedCancellation::new(CancellationToken::new());
    let (runtime_tx, runtime_rx) = std_mpsc::channel();
    let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
    let task_failure_handler: TaskFailureHandler = Arc::new(move |reason| {
        let _ = failure_tx.send(reason);
    });

    spawn_tool(
        &mut tool_tasks,
        Arc::new(NonCooperativeCallbackHost::default()),
        CellToolCall {
            id: "tool-stuck".to_string(),
            name: ToolName {
                name: "stuck".to_string(),
                namespace: None,
            },
            kind: ToolKind::Function,
            input: None,
            timeout: Duration::from_secs(60),
            deadline: None,
            buffered_output_bytes: 0,
        },
        runtime_tx.clone(),
        tool_cancellation.child(tool_cancellation.token().child_token()),
        Some(task_failure_handler.clone()),
    );
    spawn_notification(
        &mut notification_tasks,
        Arc::new(NonCooperativeCallbackHost::default()),
        NotificationInvocation {
            id: Some("notify-stuck".to_string()),
            call_id: "call-1".to_string(),
            text: "hello".to_string(),
        },
        runtime_tx,
        notification_cancellation_token.clone(),
        Some(task_failure_handler.clone()),
    );

    let started = tokio::time::Instant::now();
    let cleanup = tokio::spawn(async move {
        finish_callbacks(
            &notification_cancellation_token,
            &tool_cancellation,
            &mut notification_tasks,
            &mut tool_tasks,
            CallbackCompletion::Cancel,
            Some(&task_failure_handler),
        )
        .await;
        assert!(notification_tasks.is_empty());
        assert!(tool_tasks.is_empty());
        assert!(notification_cancellation_token.is_cancelled());
        assert!(tool_cancellation.token().is_cancelled());
    });
    tokio::task::yield_now().await;
    tokio::time::advance(CALLBACK_CANCELLATION_GRACE - Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(!cleanup.is_finished(), "callbacks keep the whole grace");
    tokio::time::advance(Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    cleanup
        .await
        .expect("bounded callback cleanup should finish");
    assert!(started.elapsed() <= CALLBACK_CANCELLATION_GRACE + Duration::from_millis(10));

    // The aborted callback never answers the runtime.
    assert!(matches!(
        runtime_rx.try_recv(),
        Err(std_mpsc::TryRecvError::Disconnected)
    ));
    // A slow callback is not a task failure: owners such as the process host
    // treat reported failures as fatal to every session on the connection.
    assert_eq!(failure_rx.recv().await, None);
}

#[tokio::test(start_paused = true)]
async fn completion_cancels_tools_before_held_notification_delivery() {
    let notification_token = CancellationToken::new();
    let tool = NestedCancellation::new(CancellationToken::new());
    let mut notifications = JoinSet::new();
    let mut tools = JoinSet::new();
    let release = CancellationToken::new();
    notifications.spawn({ let release = release.clone(); async move { release.cancelled().await } });
    tools.spawn({ let token = tool.token().clone(); async move { token.cancelled().await } });
    let cleanup = finish_callbacks(&notification_token, &tool, &mut notifications, &mut tools,
        CallbackCompletion::DrainNotifications, None);
    tokio::pin!(cleanup);
    assert!(futures::poll!(&mut cleanup).is_pending());
    assert!(tool.token().is_cancelled(), "abandoned effects stop before notification acknowledgement");
    assert!(!notification_token.is_cancelled(), "accepted notification keeps its delivery grace");
    tokio::time::advance(CALLBACK_CANCELLATION_GRACE + Duration::from_millis(1)).await;
    assert!(futures::poll!(&mut cleanup).is_pending(), "ordinary completion preserves accepted delivery past cancellation grace");
    assert!(!notification_token.is_cancelled());
    release.cancel();
    cleanup.await;
    assert!(notification_token.is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn ordinary_completion_keeps_notification_delivery_timeout() {
    let notification_token = CancellationToken::new();
    let tool = NestedCancellation::new(CancellationToken::new());
    let mut notifications = JoinSet::new();
    let mut tools = JoinSet::new();
    let (runtime_tx, runtime_rx) = std_mpsc::channel();
    spawn_notification(
        &mut notifications,
        Arc::new(NonCooperativeCallbackHost::default()),
        NotificationInvocation {
            id: Some("accepted".into()),
            call_id: "call-1".into(),
            text: "retained notification".into(),
        },
        runtime_tx,
        notification_token.clone(),
        None,
    );
    tokio::task::yield_now().await;
    let started = tokio::time::Instant::now();
    finish_callbacks(
        &notification_token,
        &tool,
        &mut notifications,
        &mut tools,
        CallbackCompletion::DrainNotifications,
        None,
    ).await;
    assert_eq!(started.elapsed(), NOTIFICATION_DELIVERY_TIMEOUT);
    assert!(matches!(runtime_rx.try_recv(), Ok(RuntimeCommand::NotificationError { id, .. }) if id == "accepted"));
    assert!(notification_token.is_cancelled());
    assert!(tool.token().is_cancelled());
}
