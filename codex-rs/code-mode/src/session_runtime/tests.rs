use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::cell_actor::CompletionCommit;

struct RecordingDelegate;

struct PanickingClosedDelegate;

impl SessionRuntimeDelegate for RecordingDelegate {
    async fn invoke_tool(
        &self,
        _invocation: NestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        Ok(JsonValue::Null)
    }

    async fn notify(
        &self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> Result<(), String> {
        Ok(())
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

impl SessionRuntimeDelegate for PanickingClosedDelegate {
    async fn invoke_tool(
        &self,
        _invocation: NestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        Ok(JsonValue::Null)
    }

    async fn notify(
        &self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> Result<(), String> {
        Ok(())
    }

    fn cell_closed(&self, _cell_id: &CellId) {
        panic!("cell close panic probe");
    }
}

#[tokio::test]
async fn reports_cell_actor_panics_to_the_owner() {
    let (failure_tx, mut failure_rx) = tokio::sync::mpsc::unbounded_channel();
    let runtime = SessionRuntime::new_with_task_failure_handler(
        Arc::new(PanickingClosedDelegate),
        Some(Arc::new(move |reason| {
            let _ = failure_tx.send(reason);
        })),
    );
    let started = runtime
        .execute(
            execute_request(r#"text("done");"#),
            ObserveMode::YieldAfter(Duration::from_secs(1)),
        )
        .await
        .expect("start cell");
    assert_eq!(
        started.initial_event().await,
        Ok(CellEvent::Completed {
            content_items: vec![OutputItem::Text {
                text: "done".to_string(),
            }],
            error_text: None,
        })
    );
    runtime.shutdown().await.expect("shutdown runtime");
    let failure = failure_rx
        .try_recv()
        .expect("shutdown should wait for the cell failure watcher");
    assert!(failure.contains("code-mode cell 1 task failed"));
}

#[tokio::test]
async fn termination_rejects_a_waiting_store_commit_before_the_next_cell_can_load_it() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let cell_state = Arc::new(CellState::new(CancellationToken::new()));
    let host = RuntimeCellHost {
        cell_id: CellId::new("terminating-writer"),
        parent_tool_call_id: "parent-call".to_string(),
        inner: Arc::clone(&runtime.inner),
        cell_permit: Mutex::new(None),
    };
    let completion = CellEvent::Completed {
        content_items: vec![OutputItem::Text {
            text: "uncommitted output".to_string(),
        }],
        error_text: None,
    };

    let stored_values = runtime.inner.stored_values.lock().await;
    let commit = host.commit_completion(
        HashMap::from([(
            "candidate".to_string(),
            JsonValue::String("lost".to_string()),
        )]),
        completion.clone(),
        /*pending_initial_yield_items*/ None,
        Arc::clone(&cell_state),
    );
    tokio::pin!(commit);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    assert!(matches!(commit.as_mut().poll(&mut context), Poll::Pending));

    let termination = cell_state.request_termination();
    drop(stored_values);
    assert_eq!(commit.await, CompletionCommit::Rejected(completion));
    let terminated = CellEvent::Terminated {
        content_items: Vec::new(),
    };
    assert_eq!(
        cell_state.finish_termination(terminated.clone()),
        Some(terminated.clone())
    );
    assert_eq!(termination.await, Ok(terminated));
    assert!(
        !runtime
            .inner
            .stored_values
            .lock()
            .await
            .contains_key("candidate")
    );

    let reader = runtime
        .execute(
            CreateCellRequest {
                tool_call_id: "reader".to_string(),
                enabled_tools: Vec::new(),
                source: r#"text(String(load("candidate")));"#.to_string(),
                default_tool_timeout_ms: 60_000,
            },
            ObserveMode::YieldAfter(Duration::from_secs(1)),
        )
        .await
        .unwrap();
    assert_eq!(
        reader.initial_event().await,
        Ok(CellEvent::Completed {
            content_items: vec![OutputItem::Text {
                text: "undefined".to_string(),
            }],
            error_text: None,
        })
    );
    runtime.shutdown().await.unwrap();
}

fn execute_request(source: &str) -> CreateCellRequest {
    CreateCellRequest {
        tool_call_id: "call-1".to_string(),
        enabled_tools: Vec::new(),
        source: source.to_string(),
        default_tool_timeout_ms: 60_000,
    }
}

#[tokio::test]
async fn terminal_result_remains_observable_after_active_cell_removal() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let started = runtime
        .execute(
            execute_request(r#"text("done");"#),
            ObserveMode::YieldAfter(Duration::from_secs(1)),
        )
        .await
        .unwrap();
    let cell_id = started.cell_id.clone();
    let completed = started.initial_event().await.unwrap();
    assert!(matches!(completed, CellEvent::Completed { .. }));

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !runtime.inner.cells.lock().await.contains_key(&cell_id) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cell should leave the active registry");

    let observed = runtime
        .begin_observe(&cell_id, ObserveMode::YieldAfter(Duration::ZERO))
        .await
        .unwrap()
        .event()
        .await;
    assert_eq!(observed, Ok(completed.clone()));
    assert_eq!(runtime.terminate(&cell_id).await, Ok(completed));
}

#[tokio::test]
async fn ninth_cell_waits_until_a_terminal_cell_releases_its_permit() {
    let runtime = Arc::new(SessionRuntime::new(Arc::new(RecordingDelegate)));
    let mut active_cell_ids = Vec::new();
    for _ in 0..MAX_ACTIVE_CELLS {
        let started = runtime
            .execute(
                execute_request("await new Promise(() => {});"),
                ObserveMode::YieldAfter(Duration::from_millis(1)),
            )
            .await
            .expect("cell should be admitted");
        active_cell_ids.push(started.cell_id);
    }

    let ninth_runtime = Arc::clone(&runtime);
    let ninth = tokio::spawn(async move {
        ninth_runtime
            .execute(
                execute_request("await new Promise(() => {});"),
                ObserveMode::YieldAfter(Duration::from_millis(1)),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!ninth.is_finished());

    runtime
        .terminate(&active_cell_ids[0])
        .await
        .expect("terminal cell should release its permit");
    let ninth_cell_id = tokio::time::timeout(Duration::from_secs(2), ninth)
        .await
        .expect("ninth cell should be admitted after permit release")
        .expect("ninth task should not panic")
        .expect("ninth cell should start")
        .cell_id;

    for cell_id in active_cell_ids.into_iter().skip(1).chain([ninth_cell_id]) {
        runtime
            .terminate(&cell_id)
            .await
            .expect("test cell should terminate");
    }
}

#[tokio::test]
async fn cell_id_allocation_fails_before_wrapping() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    runtime
        .inner
        .next_cell_id
        .store(u64::MAX, Ordering::Relaxed);

    assert_eq!(
        runtime
            .execute(
                execute_request(r#"text("unreachable");"#),
                ObserveMode::YieldAfter(Duration::from_secs(1)),
            )
            .await
            .err(),
        Some(Error::CellIdSpaceExhausted)
    );
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "test holds the registry lock to force admission ahead of shutdown"
)]
async fn shutdown_rejects_cell_admission_queued_before_the_registry_lock() {
    let runtime = Arc::new(SessionRuntime::new(Arc::new(RecordingDelegate)));
    let cells = runtime.inner.cells.lock().await;

    let execution = runtime.execute(
        execute_request("while (true) {}"),
        ObserveMode::YieldAfter(Duration::from_millis(/*millis*/ 1)),
    );
    tokio::pin!(execution);
    std::future::poll_fn(|context| match execution.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(Ok(_)) => panic!("execution completed before the registry lock was released"),
        Poll::Ready(Err(error)) => {
            panic!("execution failed before the registry lock was released: {error}")
        }
    })
    .await;

    let shutdown = runtime.shutdown();
    tokio::pin!(shutdown);
    std::future::poll_fn(|context| match shutdown.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(Ok(())) => panic!("shutdown completed before acquiring the registry lock"),
        Poll::Ready(Err(error)) => {
            panic!("shutdown failed before acquiring the registry lock: {error}")
        }
    })
    .await;

    drop(cells);
    assert!(matches!(execution.await, Err(Error::ShuttingDown)));
    assert_eq!(shutdown.await, Ok(()));
}

#[tokio::test]
async fn drop_terminates_cells_when_the_registry_is_locked() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let started = runtime
        .execute(
            execute_request("while (true) {}"),
            ObserveMode::YieldAfter(Duration::from_millis(/*millis*/ 1)),
        )
        .await
        .unwrap();
    assert_eq!(started.cell_id, CellId::new("1"));
    assert_eq!(
        started.initial_event().await,
        Ok(CellEvent::Yielded {
            content_items: Vec::new(),
        })
    );

    let inner = Arc::clone(&runtime.inner);
    let cells = inner.cells.lock().await;
    drop(runtime);
    drop(cells);

    tokio::time::timeout(Duration::from_secs(/*secs*/ 1), inner.cell_tasks.wait())
        .await
        .unwrap();
    assert!(inner.cell_tasks.is_empty());
}
