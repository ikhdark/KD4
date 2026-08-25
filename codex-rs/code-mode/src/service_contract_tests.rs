use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_protocol::ToolName;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::CodeModeToolKind;
use crate::ToolDefinition;

#[derive(Debug, PartialEq)]
enum DelegateEvent {
    NotificationStarted,
    NotificationDelivered,
    NotificationCancelled,
    ToolStarted,
    ToolCancelled,
    CellClosed(CellId),
}

struct BlockingDelegate {
    events_tx: mpsc::UnboundedSender<DelegateEvent>,
    notification_finished: AtomicBool,
    tool_finished: AtomicBool,
    tool_release: Notify,
}

struct HeldNotificationDelegate {
    events_tx: mpsc::UnboundedSender<DelegateEvent>,
    notification_release: Notify,
}

struct RendezvousDelegate {
    started: AtomicUsize,
    both_started: Notify,
}

impl CodeModeSessionDelegate for RendezvousDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            let started = self.started.fetch_add(1, Ordering::AcqRel) + 1;
            if started >= 2 {
                self.both_started.notify_waiters();
            }
            while self.started.load(Ordering::Acquire) < 2 {
                self.both_started.notified().await;
            }
            Ok(serde_json::json!({"started": started}))
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

impl HeldNotificationDelegate {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<DelegateEvent>) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                events_tx,
                notification_release: Notify::new(),
            }),
            events_rx,
        )
    }

    fn release_notification(&self) {
        self.notification_release.notify_one();
    }
}

impl CodeModeSessionDelegate for HeldNotificationDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            let _ = self.events_tx.send(DelegateEvent::ToolStarted);
            cancellation_token.cancelled().await;
            let _ = self.events_tx.send(DelegateEvent::ToolCancelled);
            Err("cancelled".to_string())
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            let _ = self.events_tx.send(DelegateEvent::NotificationStarted);
            tokio::select! {
                _ = self.notification_release.notified() => {
                    let _ = self.events_tx.send(DelegateEvent::NotificationDelivered);
                    Ok(())
                }
                _ = cancellation_token.cancelled() => {
                    let _ = self.events_tx.send(DelegateEvent::NotificationCancelled);
                    Err("cancelled".to_string())
                }
            }
        })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        let _ = self
            .events_tx
            .send(DelegateEvent::CellClosed(cell_id.clone()));
    }
}

impl BlockingDelegate {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<DelegateEvent>) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                events_tx,
                notification_finished: AtomicBool::new(false),
                tool_finished: AtomicBool::new(false),
                tool_release: Notify::new(),
            }),
            events_rx,
        )
    }
}

impl CodeModeSessionDelegate for BlockingDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            let _ = self.events_tx.send(DelegateEvent::ToolStarted);
            tokio::select! {
                _ = self.tool_release.notified() => {
                    self.tool_finished.store(true, Ordering::Release);
                    Ok(serde_json::Value::Null)
                }
                _ = cancellation_token.cancelled() => {
                    self.tool_finished.store(true, Ordering::Release);
                    let _ = self.events_tx.send(DelegateEvent::ToolCancelled);
                    Err("cancelled".to_string())
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            let _ = self.events_tx.send(DelegateEvent::NotificationStarted);
            cancellation_token.cancelled().await;
            self.notification_finished.store(true, Ordering::Release);
            let _ = self.events_tx.send(DelegateEvent::NotificationCancelled);
            Err("cancelled".to_string())
        })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        let _ = self
            .events_tx
            .send(DelegateEvent::CellClosed(cell_id.clone()));
    }
}

fn cell_id(value: &str) -> CellId {
    CellId::new(value.to_string())
}

fn execute_request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "call-1".to_string(),
        enabled_tools: Vec::new(),
        source: source.to_string(),
        yield_time_ms: Some(1),
        max_output_tokens: None,
    }
}

fn blocking_tool() -> ToolDefinition {
    ToolDefinition {
        name: "block".to_string(),
        tool_name: ToolName::plain("block"),
        description: String::new(),
        kind: CodeModeToolKind::Function,
        input_schema: None,
        output_schema: None,
    }
}

async fn next_event(events_rx: &mut mpsc::UnboundedReceiver<DelegateEvent>) -> DelegateEvent {
    tokio::time::timeout(Duration::from_secs(2), events_rx.recv())
        .await
        .expect("delegate event timeout")
        .expect("delegate event channel closed")
}

#[tokio::test]
async fn yields_and_resumes() {
    let service = InProcessCodeModeSession::new();
    let cell = service
        .execute(ExecuteRequest {
            source: r#"text("before"); yield_control(); text("after");"#.to_string(),
            yield_time_ms: Some(60_000),
            ..execute_request("")
        })
        .await
        .unwrap();

    assert_eq!(
        cell.initial_response().await.unwrap(),
        RuntimeResponse::Yielded {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "before".to_string(),
            }],
        }
    );
    assert_eq!(
        service
            .wait(WaitRequest {
                cell_id: cell_id("1"),
                yield_time_ms: 60_000,
            })
            .await
            .unwrap(),
        WaitOutcome::LiveCell(RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "after".to_string(),
            }],
            error_text: None,
        })
    );
}

#[tokio::test]
async fn bounded_parallel_nested_tool_timeout_rejects_only_the_expired_call() {
    let (delegate, mut events_rx) = BlockingDelegate::new();
    let service = InProcessCodeModeSession::with_delegate(delegate);
    let cell = service
        .execute(ExecuteRequest {
            enabled_tools: vec![blocking_tool()],
            source: r#"
const outcome = await tools.block({}, { timeout_ms: 10 }).then(
  () => "unexpected success",
  (error) => String(error),
);
text(outcome);
"#
            .to_string(),
            yield_time_ms: Some(60_000),
            ..execute_request("")
        })
        .await
        .unwrap();

    assert_eq!(next_event(&mut events_rx).await, DelegateEvent::ToolStarted);
    assert_eq!(
        cell.initial_response().await.unwrap(),
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "nested tool `block` exceeded its 10ms timeout".to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn two_independent_nested_calls_start_before_either_completes() {
    let delegate = Arc::new(RendezvousDelegate {
        started: AtomicUsize::new(0),
        both_started: Notify::new(),
    });
    let service = InProcessCodeModeSession::with_delegate(delegate.clone());
    let cell = service
        .execute(ExecuteRequest {
            enabled_tools: vec![blocking_tool()],
            source: r#"
const first = tools.block({id: 1});
const second = tools.block({id: 2});
const results = await Promise.all([first, second]);
text(results.length);
"#
            .to_string(),
            yield_time_ms: Some(60_000),
            ..execute_request("")
        })
        .await
        .unwrap();

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), cell.initial_response())
            .await
            .expect("independent nested calls should rendezvous")
            .unwrap(),
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "2".to_string(),
            }],
            error_text: None,
        }
    );
    assert_eq!(delegate.started.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn observed_natural_completion_wins_over_termination() {
    let service = InProcessCodeModeSession::new();
    let cell = service
        .execute(execute_request(
            r#"yield_control(); store("finished", true); text("done");"#,
        ))
        .await
        .unwrap();

    assert_eq!(
        cell.initial_response().await.unwrap(),
        RuntimeResponse::Yielded {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
        }
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let response = service
                .execute(ExecuteRequest {
                    yield_time_ms: Some(60_000),
                    ..execute_request(r#"text(String(load("finished")));"#)
                })
                .await
                .unwrap()
                .initial_response()
                .await
                .unwrap();
            let RuntimeResponse::Result { content_items, .. } = response else {
                panic!("expected stored-value probe to complete");
            };
            if content_items
                == vec![FunctionCallOutputContentItem::InputText {
                    text: "true".to_string(),
                }]
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        service.terminate(cell_id("1")).await.unwrap(),
        WaitOutcome::LiveCell(RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "done".to_string(),
            }],
            error_text: None,
        })
    );
}

#[tokio::test]
async fn bounded_parallel_termination_delivers_pending_notification_before_responding() {
    let (delegate, mut events_rx) = HeldNotificationDelegate::new();
    let service = InProcessCodeModeSession::with_delegate(delegate.clone());
    let cell = service
        .execute(ExecuteRequest {
            enabled_tools: vec![blocking_tool()],
            source: r#"
const reported = Promise.resolve("fast").then(async (value) => {
  await notify(`fast:${value}`);
  return value;
});
const stalled = tools.block({}, { timeout_ms: 30_000 });
await Promise.allSettled([reported, stalled]);
"#
            .to_string(),
            ..execute_request("")
        })
        .await
        .unwrap();

    assert_eq!(next_event(&mut events_rx).await, DelegateEvent::ToolStarted);
    assert_eq!(
        next_event(&mut events_rx).await,
        DelegateEvent::NotificationStarted
    );
    assert_eq!(
        cell.initial_response().await.unwrap(),
        RuntimeResponse::Yielded {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
        }
    );
    let mut termination = Box::pin(service.terminate(cell_id("1")));
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut termination)
            .await
            .is_err(),
        "termination must wait for an issued notification to finish"
    );
    assert_eq!(
        next_event(&mut events_rx).await,
        DelegateEvent::ToolCancelled
    );
    delegate.release_notification();
    assert_eq!(
        next_event(&mut events_rx).await,
        DelegateEvent::NotificationDelivered
    );
    assert_eq!(
        termination.await.unwrap(),
        WaitOutcome::LiveCell(RuntimeResponse::Terminated {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
        })
    );
    assert_eq!(
        next_event(&mut events_rx).await,
        DelegateEvent::CellClosed(cell_id("1"))
    );
}

#[tokio::test]
async fn shutdown_delivers_notifications_while_natural_completion_is_draining() {
    let (delegate, mut events_rx) = HeldNotificationDelegate::new();
    let service = Arc::new(InProcessCodeModeSession::with_delegate(delegate.clone()));
    service
        .execute(execute_request(r#"notify("pending");"#))
        .await
        .unwrap();

    assert_eq!(
        next_event(&mut events_rx).await,
        DelegateEvent::NotificationStarted
    );

    let shutdown_service = Arc::clone(&service);
    let shutdown = tokio::spawn(async move { shutdown_service.shutdown().await });

    delegate.release_notification();
    assert_eq!(
        next_event(&mut events_rx).await,
        DelegateEvent::NotificationDelivered
    );

    assert_eq!(shutdown.await.unwrap(), Ok(()));
    assert_eq!(
        next_event(&mut events_rx).await,
        DelegateEvent::CellClosed(cell_id("1"))
    );
}

#[tokio::test]
async fn repeated_termination_is_rejected_while_callback_cleanup_is_pending() {
    let (delegate, mut events_rx) = HeldNotificationDelegate::new();
    let service = Arc::new(InProcessCodeModeSession::with_delegate(delegate.clone()));
    let cell = service
        .execute(execute_request(
            r#"notify("pending"); await new Promise(() => {});"#,
        ))
        .await
        .unwrap();

    assert_eq!(
        next_event(&mut events_rx).await,
        DelegateEvent::NotificationStarted
    );
    assert_eq!(
        cell.initial_response().await.unwrap(),
        RuntimeResponse::Yielded {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
        }
    );

    let first_termination = service.terminate(cell_id("1"));
    tokio::pin!(first_termination);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut first_termination)
            .await
            .is_err(),
        "the first termination remains pending while notification cleanup is held"
    );
    let repeated_termination = service.terminate(cell_id("1")).await;
    delegate.release_notification();
    assert_eq!(
        next_event(&mut events_rx).await,
        DelegateEvent::NotificationDelivered
    );

    assert_eq!(
        repeated_termination.unwrap_err(),
        "exec cell 1 is already terminating"
    );
    assert_eq!(
        first_termination.await.unwrap(),
        WaitOutcome::LiveCell(RuntimeResponse::Terminated {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
        })
    );
    assert_eq!(
        next_event(&mut events_rx).await,
        DelegateEvent::CellClosed(cell_id("1"))
    );
}

#[tokio::test]
async fn second_observer_is_rejected_without_displacing_the_first() {
    let service = InProcessCodeModeSession::new();
    let cell = service
        .execute(execute_request("await new Promise(() => {});"))
        .await
        .unwrap();

    assert_eq!(
        cell.initial_response().await.unwrap(),
        RuntimeResponse::Yielded {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
        }
    );

    let first_observer = service
        .begin_wait(WaitRequest {
            cell_id: cell_id("1"),
            yield_time_ms: 60_000,
        })
        .await;
    assert_eq!(
        service
            .wait(WaitRequest {
                cell_id: cell_id("1"),
                yield_time_ms: 60_000,
            })
            .await
            .unwrap_err(),
        "exec cell 1 already has an active observer"
    );

    let terminated = RuntimeResponse::Terminated {
        cell_id: cell_id("1"),
        content_items: Vec::new(),
    };
    assert_eq!(
        service.terminate(cell_id("1")).await.unwrap(),
        WaitOutcome::LiveCell(terminated.clone())
    );
    assert_eq!(
        first_observer.await.unwrap(),
        WaitOutcome::LiveCell(terminated)
    );
}

#[tokio::test]
async fn dropped_wait_observer_leaves_the_cell_available() {
    let service = InProcessCodeModeSession::new();
    let cell = service
        .execute(execute_request("await new Promise(() => {});"))
        .await
        .unwrap();
    assert!(matches!(
        cell.initial_response().await.unwrap(),
        RuntimeResponse::Yielded { .. }
    ));

    let suspended = service
        .begin_wait(WaitRequest {
            cell_id: cell_id("1"),
            yield_time_ms: 60_000,
        })
        .await;
    drop(suspended);

    assert_eq!(
        service
            .wait(WaitRequest {
                cell_id: cell_id("1"),
                yield_time_ms: 0,
            })
            .await
            .unwrap(),
        WaitOutcome::LiveCell(RuntimeResponse::Yielded {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
        })
    );
    assert!(matches!(
        service.terminate(cell_id("1")).await.unwrap(),
        WaitOutcome::LiveCell(RuntimeResponse::Terminated { .. })
    ));
}

#[tokio::test]
async fn natural_completion_cleans_up_callbacks_before_responding() {
    let (delegate, mut events_rx) = BlockingDelegate::new();
    let service = InProcessCodeModeSession::with_delegate(delegate.clone());
    let cell = service
        .execute(ExecuteRequest {
            enabled_tools: vec![blocking_tool()],
            source: r#"tools.block({}); text("done");"#.to_string(),
            yield_time_ms: Some(60_000),
            ..execute_request("")
        })
        .await
        .unwrap();

    assert_eq!(next_event(&mut events_rx).await, DelegateEvent::ToolStarted);
    assert_eq!(
        cell.initial_response().await.unwrap(),
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "done".to_string(),
            }],
            error_text: None,
        }
    );
    assert!(delegate.tool_finished.load(Ordering::Acquire));
    assert_eq!(
        next_event(&mut events_rx).await,
        DelegateEvent::ToolCancelled
    );
    assert_eq!(
        next_event(&mut events_rx).await,
        DelegateEvent::CellClosed(cell_id("1"))
    );
}
