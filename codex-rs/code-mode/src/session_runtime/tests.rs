use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

use crate::NestedCancellation;
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
        _cancellation_token: NestedCancellation,
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
        _cancellation_token: NestedCancellation,
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
            output_loss: None,
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
        output_loss: None,
        content_items: vec![OutputItem::Text {
            text: "uncommitted output".to_string(),
        }],
        error_text: None,
    };

    let stored_values = runtime.inner.stored_values.lock().await;
    let commit = host.commit_completion(
        HashMap::from([(
            "candidate".to_string(),
            StoredValueWrite {
                expected: None,
                value: Some(StoredValue::new(
                    "candidate",
                    JsonValue::String("lost".to_string()),
                )),
            },
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
            output_loss: None,
            content_items: vec![OutputItem::Text {
                text: "undefined".to_string(),
            }],
            error_text: None,
        })
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn storage_limit_rejects_the_complete_cell_write_set() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    runtime.inner.stored_values.lock().await.insert(
        "stable".to_string(),
        StoredValue::new("stable", JsonValue::String("preserved".to_string())),
    );
    let writes = (0..crate::runtime::MAX_SESSION_STORED_VALUES)
        .map(|index| {
            let key = format!("new-{index}");
            let stored = StoredValue::new(&key, JsonValue::Bool(true));
            (
                key,
                StoredValueWrite {
                    expected: None,
                    value: Some(stored),
                },
            )
        })
        .collect();
    let cell_state = Arc::new(CellState::new(CancellationToken::new()));
    let host = RuntimeCellHost {
        cell_id: CellId::new("oversized-writer"),
        parent_tool_call_id: "parent-call".to_string(),
        inner: Arc::clone(&runtime.inner),
        cell_permit: Mutex::new(None),
    };

    assert_eq!(
        host.commit_completion(
            writes,
            CellEvent::Completed {
                output_loss: None,
                content_items: Vec::new(),
                error_text: None,
            },
            /*pending_initial_yield_items*/ None,
            cell_state,
        )
        .await,
        CompletionCommit::Committed
    );

    let stored_values = runtime.inner.stored_values.lock().await;
    assert_eq!(stored_values.len(), 1);
    assert_eq!(
        stored_values
            .get("stable")
            .map(|stored| stored.value.as_ref()),
        Some(&JsonValue::String("preserved".to_string()))
    );
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

#[test]
fn terminal_cache_bounds_retained_output_bytes_and_keeps_the_newest_event() {
    let completed = |bytes: usize| CellEvent::Completed {
        content_items: vec![OutputItem::Text {
            text: "x".repeat(bytes),
        }],
        error_text: None,
        output_loss: None,
    };
    let half = TERMINAL_CELL_CACHE_MAX_BYTES / 2;
    let mut cache = TerminalCellCache::default();
    cache.insert(CellId::new("1"), completed(half));
    cache.insert(CellId::new("2"), completed(half));
    assert!(
        cache.get(&CellId::new("1")).is_some(),
        "the exact budget fits"
    );
    cache.insert(CellId::new("3"), completed(1));
    assert_eq!(
        cache.get(&CellId::new("1")),
        None,
        "the oldest event is evicted"
    );
    assert_eq!(cache.get(&CellId::new("2")), Some(completed(half)));

    let oversized = completed(TERMINAL_CELL_CACHE_MAX_BYTES + 1);
    cache.insert(CellId::new("4"), oversized.clone());
    assert_eq!(cache.get(&CellId::new("4")), Some(oversized));
    assert_eq!(cache.order.len(), 1);
    assert_eq!(cache.retained_bytes, TERMINAL_CELL_CACHE_MAX_BYTES + 1);

    for index in 0..TERMINAL_CELL_CACHE_CAPACITY + 10 {
        cache.insert(CellId::new(format!("small-{index}")), completed(0));
    }
    assert_eq!(cache.order.len(), TERMINAL_CELL_CACHE_CAPACITY);
    assert_eq!(cache.events.len(), TERMINAL_CELL_CACHE_CAPACITY);
    assert_eq!(cache.get(&CellId::new("4")), None);
    assert_eq!(cache.retained_bytes, 0);
}

#[tokio::test]
async fn ninth_cell_is_rejected_until_a_terminal_cell_releases_its_permit() {
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

    let ninth = tokio::time::timeout(
        Duration::from_secs(1),
        runtime.execute(
            execute_request("store('rejected', true);"),
            ObserveMode::YieldAfter(Duration::from_millis(1)),
        ),
    )
    .await
    .expect("full capacity must return control to the caller");
    assert!(matches!(ninth, Err(Error::ActiveCellLimit)));
    assert_eq!(runtime.inner.cells.lock().await.len(), MAX_ACTIVE_CELLS);

    runtime
        .terminate(&active_cell_ids[0])
        .await
        .expect("terminal cell should release its permit");
    let next = tokio::time::timeout(
        Duration::from_secs(2),
        runtime.execute(
            execute_request("text(load('rejected') === undefined);"),
            ObserveMode::YieldAfter(Duration::from_secs(1)),
        ),
    )
    .await
    .expect("cell should be admitted after permit release")
    .expect("cell should start");
    assert_eq!(
        next.initial_event().await,
        Ok(CellEvent::Completed {
            output_loss: None,
            content_items: vec![OutputItem::Text {
                text: "true".to_string()
            }],
            error_text: None,
        })
    );
    assert!(
        !runtime
            .inner
            .stored_values
            .lock()
            .await
            .contains_key("rejected")
    );

    for cell_id in active_cell_ids.into_iter().skip(1) {
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
async fn shutdown_cancels_native_runtime_startup_without_registering_or_running_the_cell() {
    use crate::runtime::STARTUP_TEST_GATE;
    use crate::runtime::StartupTestGate;

    let (release, receiver) = std::sync::mpsc::channel();
    let gate = Arc::new(StartupTestGate {
        entered: tokio::sync::Notify::new(),
        release: std::sync::Mutex::new(receiver),
        exited: tokio::sync::Notify::new(),
    });
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let execution = STARTUP_TEST_GATE.scope(
        Arc::clone(&gate),
        runtime.execute(
            execute_request("store('unexpected', true); while (true) {}"),
            ObserveMode::YieldAfter(Duration::from_millis(1)),
        ),
    );
    tokio::pin!(execution);
    std::future::poll_fn(|cx| {
        assert!(execution.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    tokio::time::timeout(Duration::from_secs(1), gate.entered.notified())
        .await
        .unwrap();

    let (shutdown, execution) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(runtime.shutdown(), execution)
    })
    .await
    .expect("shutdown must finish while native startup is still held");
    assert_eq!(shutdown, Ok(()));
    assert!(matches!(execution, Err(Error::ShuttingDown)));
    assert!(runtime.inner.cells.lock().await.is_empty());
    assert_eq!(
        runtime.inner.active_cell_permits.available_permits(),
        MAX_ACTIVE_CELLS
    );
    assert!(runtime.inner.cell_tasks.is_empty());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.exited.notified())
        .await
        .unwrap();
    assert!(runtime.inner.stored_values.lock().await.is_empty());
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

async fn completed_test_cell<D: SessionRuntimeDelegate>(
    runtime: &SessionRuntime<D>,
    source: &str,
) -> CellEvent {
    runtime
        .execute(execute_request(source), ObserveMode::Decision)
        .await
        .unwrap()
        .initial_event()
        .await
        .unwrap()
}

fn assert_completed_text(event: CellEvent, expected: &str) {
    assert_eq!(
        event,
        CellEvent::Completed {
            content_items: vec![OutputItem::Text {
                text: expected.to_string()
            }],
            error_text: None,
            output_loss: None,
        }
    );
}

#[tokio::test]
async fn removal_reclaims_keys_and_bytes_without_changing_null_or_termination_semantics() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    assert_completed_text(
        completed_test_cell(
            &runtime,
            "for(let i=0;i<256;i++) store('key-'+i, i); text('full');",
        )
        .await,
        "full",
    );
    assert_completed_text(
        completed_test_cell(
            &runtime,
            "text(remove_store('key-0')); store('replacement', null);",
        )
        .await,
        "true",
    );
    assert_completed_text(completed_test_cell(&runtime,
        "text([load('key-0') === undefined, load('replacement') === null, load('key-255'), remove_store('absent')]);").await,
        "[true,true,255,false]");

    let started = runtime
        .execute(
            execute_request(
                "remove_store('replacement'); text('uncommitted'); await new Promise(()=>{});",
            ),
            ObserveMode::StateChange,
        )
        .await
        .unwrap();
    let id = started.cell_id.clone();
    assert!(matches!(
        started.initial_event().await.unwrap(),
        CellEvent::Yielded { .. }
    ));
    assert!(matches!(
        runtime.terminate(&id).await.unwrap(),
        CellEvent::Terminated { .. }
    ));
    assert_completed_text(
        completed_test_cell(&runtime, "text(load('replacement') === null);").await,
        "true",
    );

    assert_completed_text(completed_test_cell(&runtime,
        "for(let i=1;i<256;i++) remove_store('key-'+i); remove_store('replacement'); store('large','x'.repeat(6*1024*1024)); text('saved');").await,
        "saved");
    assert_completed_text(completed_test_cell(&runtime,
        "remove_store('large'); store('other','y'.repeat(6*1024*1024)); text(load('other').length);").await,
        "6291456");
    assert_completed_text(completed_test_cell(&runtime,
        "remove_store('other'); for(let i=0;i<1024;i++){store('temporary-'+i,true);remove_store('temporary-'+i);} text('reclaimed');").await,
        "reclaimed");
    assert!(runtime.inner.stored_values.lock().await.is_empty());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn caught_store_limit_preserves_valid_evidence_and_allows_recovery() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    assert_completed_text(completed_test_cell(&runtime,
        "store('evidence',{file:'known.rs',hash:'valid'}); try {store('too-large','x'.repeat(8*1024*1024));} catch(error) {text(error instanceof TypeError);} store('recovery',true);").await,
        "true");
    assert_completed_text(
        completed_test_cell(
            &runtime,
            "text([load('evidence').hash, load('recovery'), load('too-large') === undefined]);",
        )
        .await,
        "[\"valid\",true,true]",
    );
    runtime.shutdown().await.unwrap();
}

struct StoreBarrierDelegate {
    started: Semaphore,
    release: Semaphore,
}

impl SessionRuntimeDelegate for StoreBarrierDelegate {
    async fn invoke_tool(
        &self,
        _invocation: NestedToolCall,
        cancellation: NestedCancellation,
    ) -> Result<JsonValue, String> {
        self.started.add_permits(1);
        tokio::select! {
            permit = self.release.acquire() => { permit.unwrap().forget(); Ok(JsonValue::Null) }
            _ = cancellation.token().cancelled() => Err("cancelled".to_string()),
        }
    }
    async fn notify(
        &self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation: CancellationToken,
    ) -> Result<(), String> {
        Ok(())
    }
    fn cell_closed(&self, _cell_id: &CellId) {}
}

#[tokio::test]
async fn concurrent_store_conflicts_reject_only_the_losing_transaction_not_disjoint_keys() {
    for (shared_key, remove_second) in [(false, false), (true, false), (true, true)] {
        let delegate = Arc::new(StoreBarrierDelegate {
            started: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        let runtime = SessionRuntime::new(Arc::clone(&delegate));
        assert_completed_text(
            completed_test_cell(&runtime, "store('shared',[]); text('ready');").await,
            "ready",
        );
        let mut cells = Vec::new();
        for index in 0..2 {
            let key = if shared_key {
                "shared".to_string()
            } else {
                format!("file-{index}")
            };
            let update = if remove_second && index == 1 {
                format!("remove_store('{key}');")
            } else {
                format!(
                    "const entries=load('{key}') ?? []; entries.push({index}); store('{key}',entries);"
                )
            };
            let mut request = execute_request(&format!(
                "{update} store('side-{index}',true); await tools.barrier();"
            ));
            request.enabled_tools.push(ToolDefinition {
                name: "barrier".to_string(),
                tool_name: ToolName {
                    name: "barrier".to_string(),
                    namespace: None,
                },
                description: String::new(),
                kind: ToolKind::Function,
                default_timeout_ms: None,
            });
            cells.push(
                runtime
                    .execute(request, ObserveMode::Decision)
                    .await
                    .unwrap(),
            );
        }
        tokio::time::timeout(Duration::from_secs(5), delegate.started.acquire_many(2))
            .await
            .unwrap()
            .unwrap()
            .forget();
        delegate.release.add_permits(2);
        let mut successes = Vec::new();
        for (index, cell) in cells.into_iter().enumerate() {
            let CellEvent::Completed { error_text, .. } = cell.initial_event().await.unwrap()
            else {
                panic!("completion");
            };
            if let Some(error) = error_text {
                assert!(shared_key && error.contains("storage conflict"), "{error}");
                assert!(error.contains("do not replay external side effects"));
            } else {
                successes.push(index);
            }
        }
        {
            let stored = runtime.inner.stored_values.lock().await;
            assert_eq!(successes.len(), if shared_key { 1 } else { 2 });
            for index in 0..2 {
                assert_eq!(
                    stored.contains_key(&format!("side-{index}")),
                    successes.contains(&index)
                );
            }
            if shared_key {
                if remove_second && successes[0] == 1 {
                    assert!(!stored.contains_key("shared"));
                } else {
                    assert_eq!(
                        stored["shared"].value.as_ref(),
                        &serde_json::json!([successes[0]])
                    );
                }
            } else {
                assert_eq!(stored["file-0"].value.as_ref(), &serde_json::json!([0]));
                assert_eq!(stored["file-1"].value.as_ref(), &serde_json::json!([1]));
            }
        }
        runtime.shutdown().await.unwrap();
    }
}

async fn wait_for_execution_slots<D: SessionRuntimeDelegate>(runtime: &SessionRuntime<D>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while runtime.inner.active_cell_permits.available_permits() != MAX_ACTIVE_CELLS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("finished work must release execution capacity before observation");
}

#[tokio::test]
async fn completed_unobserved_cells_release_execution_slots_without_losing_output() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let mut ids = Vec::new();
    for index in 0..MAX_ACTIVE_CELLS {
        let cell = runtime
            .execute(
                execute_request(&format!("yield_control(); text('finished-{index}');")),
                ObserveMode::Decision,
            )
            .await
            .unwrap();
        ids.push(cell.cell_id.clone());
        assert!(matches!(
            cell.initial_event().await.unwrap(),
            CellEvent::ExplicitYield { .. }
        ));
    }
    wait_for_execution_slots(&runtime).await;
    assert_completed_text(completed_test_cell(&runtime, "text('next');").await, "next");
    for (index, id) in ids.into_iter().enumerate() {
        let event = runtime
            .begin_observe(&id, ObserveMode::Decision)
            .await
            .unwrap()
            .event()
            .await
            .unwrap();
        assert_completed_text(event, &format!("finished-{index}"));
    }
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn unobserved_output_budget_applies_backpressure_without_discarding_results() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let mut ids = Vec::new();
    for _ in 0..2 {
        let cell = runtime
            .execute(
                execute_request("yield_control(); text('x'.repeat(4*1024*1024));"),
                ObserveMode::Decision,
            )
            .await
            .unwrap();
        ids.push(cell.cell_id.clone());
        assert!(matches!(
            cell.initial_event().await.unwrap(),
            CellEvent::ExplicitYield { .. }
        ));
    }
    wait_for_execution_slots(&runtime).await;
    let rejected = runtime
        .execute(
            execute_request("store('must-not-run', true);"),
            ObserveMode::Decision,
        )
        .await;
    let Err(Error::Runtime(error)) = rejected else {
        panic!("unobserved output must remain bounded");
    };
    assert!(error.contains("unobserved result limit") && error.contains(ids[0].as_str()));
    for id in ids {
        let event = runtime
            .begin_observe(&id, ObserveMode::Decision)
            .await
            .unwrap()
            .event()
            .await
            .unwrap();
        assert_completed_text(event, &"x".repeat(4 * 1024 * 1024));
    }
    assert_completed_text(
        completed_test_cell(&runtime, "text(load('must-not-run') === undefined);").await,
        "true",
    );
    runtime.shutdown().await.unwrap();
}
