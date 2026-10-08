use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use crate::NestedCancellation;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::cell_actor::CompletionCommit;

struct RecordingDelegate;

#[tokio::test]
async fn inline_replay_is_ready_without_blocking_pool_and_retains_exact_admission() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let id = CellId::new("inline");
    let event = CellEvent::Completed {content_items:vec![OutputItem::Text{text:"λ😀\n".repeat(1000)}],
        error_text:None,output_loss:None};
    let bytes = cell_event_bytes(&event);
    runtime.inner.terminal_cells.lock().unwrap().insert(id.clone(), event.clone());
    assert_eq!(runtime.inner.terminal_cells.lock().unwrap().lookup(&id).unwrap().1, bytes);
    let observation = runtime.begin_observe(&id, ObserveMode::Decision);
    tokio::pin!(observation);
    let Poll::Ready(Ok(pending)) = futures::poll!(&mut observation) else {panic!("inline replay was offloaded")};
    assert_eq!(runtime.inner.replay_bytes.available_permits(), TERMINAL_REPLAY_MAX_BYTES-bytes);
    assert_eq!(pending.event().await.unwrap(), event);
    assert_eq!(runtime.inner.replay_bytes.available_permits(), TERMINAL_REPLAY_MAX_BYTES);
    let dropped = runtime.begin_observe(&id, ObserveMode::Decision).await.unwrap();
    drop(dropped);
    assert_eq!(runtime.inner.replay_bytes.available_permits(), TERMINAL_REPLAY_MAX_BYTES);
    runtime.shutdown().await.unwrap();
    assert!(matches!(runtime.cached_terminal_observation(&id).await, Err(Error::ShuttingDown)));
}

#[tokio::test]
#[ignore = "narrow timing probe"]
async fn critical_path_terminal_replay_report_benchmark() {
    for (label, size) in [("inline-replay-small", 1024), ("inline-replay-large", 1024*1024)] {
        let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
        let id = CellId::new("cached");
        let event = CellEvent::Completed {content_items:vec![OutputItem::Text{text:"x".repeat(size)}],error_text:None,output_loss:None};
        runtime.inner.terminal_cells.lock().unwrap().insert(id.clone(), event.clone());
        let mut samples = Vec::new();
        for _ in 0..7 {
            let start = std::time::Instant::now();
            for _ in 0..100 {
                assert_eq!(runtime.cached_terminal_event(&id).await.unwrap(), event);
            }
            samples.push(start.elapsed().as_secs_f64()*1000.0);
        }
        crate::runtime::critical_path_tests::report(label, &samples);
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn critical_path_terminal_replay_benchmark() {
    for size in [64, 512 * 1024] {
        let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
        let event = CellEvent::Completed { content_items: vec![OutputItem::Text {
            text: "x".repeat(size),
        }], error_text: None, output_loss: None };
        let id = CellId::new("cached-benchmark");
        runtime.inner.terminal_cells.lock().unwrap().insert(id.clone(), event.clone());
        let started = std::time::Instant::now();
        for _ in 0..100 {
            assert_eq!(runtime.cached_terminal_event(&id).await.unwrap(), event);
        }
        eprintln!("critical_path_terminal size={size} calls=100 elapsed_us={}", started.elapsed().as_micros());
        assert_eq!(runtime.inner.replay_bytes.available_permits(), TERMINAL_REPLAY_MAX_BYTES);
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn critical_path_inline_replay_retains_charge_until_delivery() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let event = CellEvent::Completed { content_items: vec![OutputItem::Text { text: "λ\n".into() }],
        error_text: None, output_loss: None };
    let id = CellId::new("inline-admission");
    let bytes = cell_event_bytes(&event);
    runtime.inner.terminal_cells.lock().unwrap().insert(id.clone(), event.clone());
    let pending = runtime.begin_observe(&id, ObserveMode::Decision).await.unwrap();
    assert_eq!(runtime.inner.replay_bytes.available_permits(), TERMINAL_REPLAY_MAX_BYTES - bytes);
    drop(pending);
    assert_eq!(runtime.inner.replay_bytes.available_permits(), TERMINAL_REPLAY_MAX_BYTES);
    assert_eq!(runtime.cached_terminal_event(&id).await.unwrap(), event);
    runtime.shutdown().await.unwrap();
    assert!(matches!(runtime.cached_terminal_event(&id).await, Err(Error::ShuttingDown)));
}

#[test]
fn terminal_accounting_charges_empty_items_and_escaping_and_spills_exactly() {
    for text in ["", "\u{0000}\"\\\n"] {
        let event = CellEvent::Completed { content_items: (0..200_000).map(|_| OutputItem::Text { text: text.into() }).collect(), error_text: None, output_loss: None };
        assert!(cell_event_bytes(&event) >= serde_json::to_vec(&event).unwrap().len());
        assert!(cell_event_bytes(&event) > TERMINAL_CELL_CACHE_MAX_BYTES);
        let (cached, retained) = CachedCellEvent::new(event.clone());
        assert_eq!(retained, 0);
        assert_eq!(cached.read().unwrap(), event);
    }
}

#[tokio::test]
async fn terminal_replay_holds_admission_until_handoff_or_cancellation() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let event = CellEvent::Completed { content_items: vec![OutputItem::Text { text: "x".repeat(9 * 1024 * 1024) }], error_text: None, output_loss: None };
    let bytes = cell_event_bytes(&event);
    let id = CellId::new("cached");
    runtime.inner.terminal_cells.lock().unwrap().insert(id.clone(), event.clone());
    let withheld = Arc::clone(&runtime.inner.replay_bytes).acquire_many_owned((TERMINAL_REPLAY_MAX_BYTES - bytes) as u32).await.unwrap();
    let first = runtime.begin_observe(&id, ObserveMode::Decision).await.unwrap();
    let second = runtime.begin_observe(&id, ObserveMode::Decision);
    tokio::pin!(second);
    assert!(futures::poll!(&mut second).is_pending());
    assert_eq!(runtime.inner.replay_bytes.available_permits(), 0);
    assert_eq!(first.event().await.unwrap(), event);
    let second = second.await.unwrap();
    assert_eq!(runtime.inner.replay_bytes.available_permits(), 0);
    drop(second);
    assert_eq!(runtime.inner.replay_bytes.available_permits(), bytes);
    drop(withheld);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn immutable_catalog_reuse_preserves_revision_identity_and_short_cell_results() {
    use codex_code_mode_protocol::{ToolDefinition, CodeModeToolKind};
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let definitions: Arc<[ToolDefinition]> = (0..300).map(|index| ToolDefinition {
        name: format!("tool_{index}"), tool_name: codex_protocol::ToolName::plain(format!("tool_{index}")),
        description: "description of arguments and return shape ".repeat(80).into(),
        kind: CodeModeToolKind::Function, input_schema: None, output_schema: None, default_timeout_ms: None,
    }).collect();
    let mut durations = [std::time::Duration::ZERO; 2];
    // Warm the runtime first, then alternate order to avoid attributing V8
    // startup or monotonic host load to immutable-catalog reuse.
    let warm = runtime.execute(execute_request("text('warm');"), ObserveMode::Decision).await.unwrap();
    warm.initial_event().await.unwrap();
    for reuse in [false, true, true, false, false, true] {
        let start = std::time::Instant::now();
        for _ in 0..30 {
            let mut request = execute_request("text('ok');");
            request.enabled_tools = if reuse { Arc::clone(&definitions) } else { definitions.iter().cloned().collect() };
            let cell = runtime.execute(request, ObserveMode::Decision).await.unwrap();
            let event = cell.initial_event().await.unwrap();
            assert!(matches!(event, CellEvent::Completed { error_text: None, .. }));
            while runtime.inner.active_cell_permits.available_permits() != runtime.inner.active_cell_capacity {
                tokio::task::yield_now().await;
            }
        }
        durations[usize::from(reuse)] += start.elapsed();
    }
    eprintln!("short cells: rebuilt catalog {:?}; shared catalog {:?}; cells=90 each; tools=300", durations[0], durations[1]);
    let cached = runtime.inner.catalog.lock().unwrap();
    assert!(Arc::ptr_eq(&cached.as_ref().unwrap().0, &definitions));
    assert!(cached.as_ref().unwrap().1.input_bytes() > 300 * 80 * 30);
    drop(cached);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn durable_completion_retains_unobserved_initial_yield() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    runtime.restore_durable_state(Some(&path)).await.unwrap();
    let host = RuntimeCellHost {
        cell_id: CellId::new("1"), parent_tool_call_id: "yielded".into(),
        snapshot: HashMap::new(), inner: runtime.inner.clone(), cell_permit: Mutex::new(None),
    };
    let event = CellEvent::Completed {
        content_items: vec![OutputItem::Text { text: "after".into() }],
        error_text: None, output_loss: None,
    };
    let pending = Some(vec![OutputItem::Text { text: "before".into() }]);
    let expected = crate::cell_actor::prepend_initial_yield(event.clone(), pending.clone());
    assert_eq!(host.commit_completion(HashMap::new(), event, pending,
        Arc::new(CellState::new(CancellationToken::new()))).await, CompletionCommit::Committed);
    runtime.shutdown().await.unwrap();
    drop(host);
    drop(runtime);
    let restored = SessionRuntime::new(Arc::new(RecordingDelegate));
    restored.restore_durable_state(Some(&path)).await.unwrap();
    assert_eq!(restored.cached_terminal_event(&CellId::new("1")).await.unwrap(), expected);
    restored.shutdown().await.unwrap();
}

#[tokio::test]
async fn slow_snapshot_io_does_not_hold_readers_or_hide_late_publication() {
    for publishing in [false, true] {
        let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        runtime.restore_durable_state(Some(&path)).await.unwrap();
        let durable = runtime.inner.durable_state.get().unwrap().clone();
        let (release, receiver) = std::sync::mpsc::channel();
        let gate = Arc::new(StorageTestGate { entered: tokio::sync::Notify::new(), release: StdMutex::new(receiver) });
        *durable.io_gate.lock().unwrap() = Some((publishing, gate.clone()));
        let state = Arc::new(CellState::new(CancellationToken::new()));
        let host = Arc::new(RuntimeCellHost {
            cell_id: CellId::new("1"), parent_tool_call_id: "writer".into(),
            snapshot: HashMap::new(), inner: runtime.inner.clone(), cell_permit: Mutex::new(None),
        });
        let event = CellEvent::Completed { content_items: Vec::new(), error_text: None, output_loss: None };
        let commit = tokio::spawn({
            let state = state.clone();
            let event = event.clone();
            async move { host.commit_completion(HashMap::from([("key".into(), StoredValue::new("key", JsonValue::Bool(true)))]),
                event, None, state).await }
        });
        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified()).await.unwrap();
        let values = tokio::time::timeout(Duration::from_millis(100), runtime.inner.stored_values.lock()).await
            .expect("filesystem must not lock shared values");
        assert_eq!(values.contains_key("key"), publishing);
        drop(values);
        let termination = state.request_termination();
        let outcome = tokio::time::timeout(Duration::from_secs(2), commit).await.unwrap().unwrap();
        if publishing {
            assert_eq!(outcome, CompletionCommit::Committed);
            let completed = termination.await.unwrap();
            assert!(matches!(&completed, CellEvent::Completed { error_text: Some(error), .. }
                if error.contains("unconfirmed")));
            runtime.inner.terminal_cells.lock().unwrap()
                .insert(CellId::new("1"), completed);
        } else {
            assert_eq!(outcome, CompletionCommit::Rejected(event));
            state.finish_termination(CellEvent::Terminated { content_items: Vec::new() });
            assert!(matches!(termination.await.unwrap(), CellEvent::Terminated { .. }));
        }
        release.send(()).unwrap();
        // The owned transaction retains this gate until all late I/O has settled.
        let _settled = runtime.inner.commit_gate.lock().await;
        let disk: JsonValue = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk["values"].get("key").is_some(), publishing,
            "rejected staging cannot publish late; admitted publication remains real");
        if publishing {
            let confirmed = runtime.begin_observe(&CellId::new("1"), ObserveMode::Decision)
                .await.unwrap().event().await.unwrap();
            assert_eq!(confirmed, CellEvent::Completed {
                content_items: Vec::new(), error_text: None, output_loss: None,
            }, "late publication must refine the cached receipt without a runtime restart");
        }
    }
}

struct PanickingClosedDelegate;

#[tokio::test]
async fn snapshot_staging_overlap_measures_complete_independent_cell_work() {
    let mut totals = [Duration::ZERO; 2];
    for baseline in [true, false] {
        for _ in 0..3 {
            let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("state.json");
            runtime.restore_durable_state(Some(&path)).await.unwrap();
            let durable = runtime.inner.durable_state.get().unwrap().clone();
            let (release, receiver) = std::sync::mpsc::channel();
            let gate = Arc::new(StorageTestGate {
                entered: tokio::sync::Notify::new(), release: StdMutex::new(receiver),
            });
            *durable.io_gate.lock().unwrap() = Some((false, gate.clone()));
            let host = RuntimeCellHost {
                cell_id: CellId::new("writer"), parent_tool_call_id: "writer".into(),
                snapshot: HashMap::new(), inner: runtime.inner.clone(), cell_permit: Mutex::new(None),
            };
            let commit = tokio::spawn(async move {
                host.commit_completion(
                    HashMap::from([("writer".into(), StoredValue::new("writer", JsonValue::Bool(true)))]),
                    CellEvent::Completed { content_items: Vec::new(), error_text: None, output_loss: None },
                    None, Arc::new(CellState::new(CancellationToken::new())),
                ).await
            });
            tokio::time::timeout(Duration::from_secs(5), gate.entered.notified()).await.unwrap();
            // Emulate only the former lock lifetime, with identical staging,
            // publication, independent work and receipt checks in both modes.
            let old_read_lock = if baseline { Some(runtime.inner.stored_values.lock().await) } else { None };
            let started = std::time::Instant::now();
            let release_io = async {
                tokio::time::sleep(Duration::from_millis(150)).await;
                *durable.io_gate.lock().unwrap() = None;
                drop(old_read_lock);
                release.send(()).unwrap();
            };
            let independent = async {
                let cell = runtime.execute(execute_request(
                    "await new Promise(resolve => setTimeout(resolve, 150)); store('independent', true); text('done');"
                ), ObserveMode::Decision).await.unwrap();
                let id = cell.cell_id.clone();
                let event = cell.initial_event().await.unwrap();
                assert!(matches!(event, CellEvent::Completed { error_text: None, .. }), "{event:?}");
                id
            };
            let (_, id, committed) = tokio::join!(release_io, independent, commit);
            assert_eq!(committed.unwrap(), CompletionCommit::Committed);
            totals[usize::from(!baseline)] += started.elapsed();
            let disk: JsonValue = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert!(disk["values"].get("writer").is_some());
            assert!(disk["values"].get("independent").is_some());
            tokio::time::timeout(Duration::from_secs(5), async {
                while runtime.inner.active_cell_permits.available_permits() != runtime.inner.active_cell_capacity {
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
            assert!(runtime.cached_terminal_event(&id).await.is_ok());
            runtime.shutdown().await.unwrap();
        }
    }
    eprintln!("snapshot+independent-cell completion, 3 runs: old lock {:?}; unlocked {:?}; staging delay=150ms; independent work=150ms; retries=0", totals[0], totals[1]);
    assert!(totals[1] < totals[0], "retain only a net critical-path improvement in this controlled fixture");
}

#[tokio::test]
async fn held_terminal_spill_releases_execution_capacity_and_keeps_inline_receipt() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let (release, receiver) = std::sync::mpsc::channel();
    let gate = Arc::new(StorageTestGate { entered: tokio::sync::Notify::new(), release: StdMutex::new(receiver) });
    *runtime.inner.spill_gate.lock().unwrap() = Some(gate.clone());
    let cell = runtime.execute(execute_request("for (let i = 0; i < 9; i++) text('x'.repeat(1024 * 1024));"),
        ObserveMode::YieldAfter(Duration::from_secs(10))).await.unwrap();
    let id = cell.cell_id.clone();
    let initial = cell.initial_event().await.unwrap();
    assert!(matches!(initial, CellEvent::Completed { error_text: None, .. }), "{initial:?}");
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified()).await.unwrap();
    assert_eq!(runtime.inner.active_cell_permits.available_permits(), runtime.inner.active_cell_capacity);
    let receipt = runtime.cached_terminal_event(&id).await.unwrap();
    assert!(matches!(receipt, CellEvent::Completed { content_items, error_text: None, .. }
        if content_items.len() == 9 && content_items.iter().all(|item|
            matches!(item, OutputItem::Text { text } if text.len() == 1024 * 1024))));
    let next = runtime.execute(execute_request("text('next');"), ObserveMode::YieldAfter(Duration::from_secs(1))).await.unwrap();
    assert!(matches!(next.initial_event().await.unwrap(), CellEvent::Completed { error_text: None, .. }));
    tokio::time::timeout(Duration::from_secs(1), runtime.shutdown()).await.unwrap().unwrap();
    release.send(()).unwrap();
}

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
        snapshot: HashMap::new(),
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
            StoredValue::new("candidate", JsonValue::String("lost".to_string())),
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
                state_path: None,
                tool_call_id: "reader".to_string(),
                enabled_tools: Vec::new().into(),
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
            (key, stored)
        })
        .collect();
    let cell_state = Arc::new(CellState::new(CancellationToken::new()));
    let host = RuntimeCellHost {
        cell_id: CellId::new("oversized-writer"),
        parent_tool_call_id: "parent-call".to_string(),
        snapshot: runtime.inner.stored_values.lock().await.clone(),
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
        state_path: None,
        tool_call_id: "call-1".to_string(),
        enabled_tools: Vec::new().into(),
        source: source.to_string(),
        default_tool_timeout_ms: 60_000,
    }
}

#[tokio::test]
async fn concurrent_store_conflict_preserves_winner_and_rejects_entire_write_set() {
    // A new key, identical concurrent increments, and ABA writes must all
    // reject the stale transaction, including its otherwise disjoint writes.
    for (snapshot, winner, proposed) in [(None, 1, 2), (Some(0), 1, 1), (Some(0), 0, 2)] {
        let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
        let host = RuntimeCellHost {
            cell_id: CellId::new("stale-writer"),
            parent_tool_call_id: "parent-call".into(),
            snapshot: snapshot.into_iter().map(|value| {
                ("key".into(), StoredValue::new("key", JsonValue::from(value)))
            }).collect(),
            inner: Arc::clone(&runtime.inner),
            cell_permit: Mutex::new(None),
        };
        runtime.inner.stored_values.lock().await.insert(
            "key".into(), StoredValue::new("key", JsonValue::from(winner)),
        );
        let state = Arc::new(CellState::new(CancellationToken::new()));
        host.commit_completion(
            HashMap::from([
                ("key".into(), StoredValue::new("key", JsonValue::from(proposed))),
                ("other".into(), StoredValue::new("other", JsonValue::from(true))),
            ]),
            CellEvent::Completed { content_items: Vec::new(), error_text: None, output_loss: None },
            None,
            state,
        ).await;
        let values = runtime.inner.stored_values.lock().await;
        assert_eq!(values["key"].value.as_ref(), &JsonValue::from(winner));
        assert!(!values.contains_key("other"));
        drop(values);
        runtime.shutdown().await.unwrap();
    }
    // A cell can write a disjoint key while depending on a stale read, including
    // a previously absent key. Write/write conflict checks alone miss this.
    for snapshot in [None, Some(0)] {
        let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
        let host = RuntimeCellHost {
            cell_id: CellId::new("stale-reader"),
            parent_tool_call_id: "parent-call".into(),
            snapshot: snapshot.into_iter().map(|value| {
                ("input".into(), StoredValue::new("input", JsonValue::from(value)))
            }).collect(),
            inner: Arc::clone(&runtime.inner),
            cell_permit: Mutex::new(None),
        };
        runtime.inner.stored_values.lock().await.insert(
            "input".into(), StoredValue::new("input", JsonValue::from(1)),
        );
        let mut output = StoredValue::new("derived", JsonValue::from(0));
        output.read_dependencies = Some(Arc::new(std::collections::HashSet::from(["input".into()])));
        let state = Arc::new(CellState::new(CancellationToken::new()));
        let commit = host.commit_completion(
            HashMap::from([("derived".into(), output)]),
            CellEvent::Completed { content_items: Vec::new(), error_text: None, output_loss: None },
            None,
            Arc::clone(&state),
        ).await;
        assert_eq!(commit, CompletionCommit::Committed);
        let CellEvent::Completed { error_text: Some(error), .. } = state.request_termination().await.unwrap() else {
            panic!("expected a rejected transaction receipt");
        };
        let receipt: JsonValue = serde_json::from_str(&error).unwrap();
        assert_eq!(receipt["conflicting_keys"], serde_json::json!([
            {"key":"input", "key_truncated":false, "read":true, "write":false}
        ]));
        assert_eq!(receipt["external_effects_rolled_back"], false);
        assert_eq!(receipt["automatic_replay_allowed"], false);
        let values = runtime.inner.stored_values.lock().await;
        assert_eq!(values["input"].value.as_ref(), &JsonValue::from(1));
        assert!(!values.contains_key("derived"), "stale reads cannot authorize derived writes");
        drop(values);
        runtime.shutdown().await.unwrap();
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
fn expired_terminal_evidence_is_bounded_and_never_inferred_from_ids() {
    let mut cache = TerminalCellCache::default();
    cache.insert(CellId::new("completed"), CellEvent::Completed {
        content_items: Vec::new(), error_text: None, output_loss: None,
    });
    cache.insert(CellId::new("interrupted"), CellEvent::Terminated { content_items: Vec::new() });
    for id in 0..TERMINAL_CELL_CACHE_CAPACITY {
        cache.insert(CellId::new(id.to_string()), CellEvent::Completed {
            content_items: Vec::new(), error_text: None, output_loss: None,
        });
    }
    assert!(matches!(cache.lookup(&CellId::new("completed")), Err(Error::ExpiredResult { completed: true, .. })));
    assert!(matches!(cache.lookup(&CellId::new("interrupted")), Err(Error::ExpiredResult { completed: false, .. })));
    for id in ["reserved-but-unstarted", "never-allocated"] {
        assert!(matches!(cache.lookup(&CellId::new(id)), Err(Error::MissingCell(_))));
    }
    for id in TERMINAL_CELL_CACHE_CAPACITY..3 * TERMINAL_CELL_CACHE_CAPACITY {
        cache.insert(CellId::new(id.to_string()), CellEvent::Completed {
            content_items: Vec::new(), error_text: None, output_loss: None,
        });
    }
    assert_eq!(cache.expired.len(), TERMINAL_CELL_CACHE_CAPACITY);
    assert!(matches!(cache.lookup(&CellId::new("completed")), Err(Error::MissingCell(_))),
        "once lifecycle evidence expires, completion is unknown rather than inferred");
}

#[tokio::test]
async fn durable_cells_restore_values_and_terminal_receipts_without_reexecution() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let mut request = execute_request(
        r#"store("artifact", {artifact_id: "retained-evidence"}); text("completed once");"#,
    );
    request.state_path = Some(path.clone());
    let started = runtime.execute(request, ObserveMode::Decision).await.unwrap();
    let old_id = started.cell_id.clone();
    let original = started.initial_event().await.unwrap();
    runtime.shutdown().await.unwrap();
    drop(runtime);

    let restored = SessionRuntime::new(Arc::new(RecordingDelegate));
    let mut request = execute_request(r#"text(load("artifact").artifact_id);"#);
    request.state_path = Some(path);
    let started = restored.execute(request, ObserveMode::Decision).await.unwrap();
    assert_ne!(started.cell_id, old_id, "restored IDs must never be reused");
    assert_eq!(started.initial_event().await.unwrap(), CellEvent::Completed {
        content_items: vec![OutputItem::Text { text: "retained-evidence".into() }],
        error_text: None,
        output_loss: None,
    });
    assert_eq!(
        restored.begin_observe(&old_id, ObserveMode::Decision).await.unwrap().event().await.unwrap(),
        original,
        "observing a recovered terminal cell must return its original receipt, not run it",
    );
    restored.shutdown().await.unwrap();
}

#[tokio::test]
async fn late_persistence_opt_in_runs_cell_without_replacing_live_state() {
    for active_cell in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
        let permit = if active_cell {
            Some(Arc::clone(&runtime.inner.active_cell_permits).acquire_owned().await.unwrap())
        } else {
            // A genuine collision remains memory-only; promotion is tested below.
            let (existing, _) = snapshot::DurableState::open(path.clone()).unwrap();
            drop(existing);
            runtime.inner.stored_values.lock().await.insert(
                "existing".into(), StoredValue::new("existing", JsonValue::from("kept")),
            );
            None
        };
        let mut request = execute_request(r#"store("ran", true); text("executed");"#);
        request.state_path = Some(path.clone());
        let event = runtime.execute(request, ObserveMode::Decision).await.unwrap()
            .initial_event().await.unwrap();
        let CellEvent::Completed { content_items, error_text, .. } = event else {
            panic!("cell must complete");
        };
        assert_eq!(error_text, None);
        assert!(matches!(&content_items[0], OutputItem::Text { text } if text.contains("in-memory state only")));
        assert!(matches!(&content_items[1], OutputItem::Text { text } if text == "executed"));
        assert!(runtime.inner.durable_state.get().is_none());
        assert_eq!(path.exists(), !active_cell);
        let values = runtime.inner.stored_values.lock().await;
        assert_eq!(values["ran"].value.as_ref(), &JsonValue::Bool(true));
        if !active_cell {
            assert_eq!(values["existing"].value.as_ref(), &JsonValue::from("kept"));
        }
        drop(values);
        drop(permit);
        runtime.shutdown().await.unwrap();
    }
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
    let half = TERMINAL_CELL_CACHE_MAX_BYTES / 2 - cell_event_bytes(&completed(0));
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
    assert_eq!(cache.order.len(), 3);
    assert_eq!(cache.retained_bytes, cell_event_bytes(&completed(half)) + cell_event_bytes(&completed(1)));
    assert!(matches!(cache.entry(&CellId::new("4")).as_deref(), Some(CachedCellEvent::Spilled(..))));

    for index in 0..TERMINAL_CELL_CACHE_CAPACITY + 10 {
        cache.insert(CellId::new(format!("small-{index}")), completed(0));
    }
    assert_eq!(cache.order.len(), TERMINAL_CELL_CACHE_CAPACITY);
    assert_eq!(cache.events.len(), TERMINAL_CELL_CACHE_CAPACITY);
    assert_eq!(cache.get(&CellId::new("4")), None);
    assert_eq!(cache.retained_bytes, TERMINAL_CELL_CACHE_CAPACITY * cell_event_bytes(&completed(0)));
}

#[tokio::test]
async fn named_state_promotes_discovers_and_retires_without_reexecuting_producers() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let event = runtime.execute(execute_request(
        r#"for (let i=0;i<256;i++) store('batch-'+String(i).padStart(3,'0'), {evidence:i});"#,
    ), ObserveMode::Decision).await.unwrap().initial_event().await.unwrap();
    assert!(matches!(event, CellEvent::Completed { error_text: None, .. }));
    // Wait for the completed actor to release its admission permit.
    tokio::time::timeout(Duration::from_secs(5), async {
        while runtime.inner.active_cell_permits.available_permits() != runtime.inner.active_cell_capacity {
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let mut request = execute_request(r#"
        let keys=[], after;
        do { const page=listKeys({after,limit:7}); keys.push(...page.keys); after=page.next_after; } while(after);
        if(keys.length!==256 || load(keys[0]).evidence!==0) throw Error('discovery');
        if(!deleteStored(keys[0]) || load(keys[0])!==undefined) throw Error('retirement');
        store('replacement', {evidence:'retained'});
    "#);
    request.state_path = Some(path.clone());
    let event = runtime.execute(request, ObserveMode::Decision).await.unwrap().initial_event().await.unwrap();
    assert!(matches!(event, CellEvent::Completed { error_text: None, .. }), "{event:?}");
    runtime.shutdown().await.unwrap();
    drop(runtime);
    let restored = SessionRuntime::new(Arc::new(RecordingDelegate));
    let mut request = execute_request(r#"
        if(load('batch-000')!==undefined || load('replacement').evidence!=='retained' ||
            load('batch-255').evidence!==255) throw Error('durability');
    "#);
    request.state_path = Some(path);
    let event = restored.execute(request, ObserveMode::Decision).await.unwrap().initial_event().await.unwrap();
    assert!(matches!(event, CellEvent::Completed { error_text: None, .. }), "{event:?}");
    restored.shutdown().await.unwrap();
}

#[tokio::test]
async fn named_state_deletion_rejects_concurrent_replacement() {
    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let original = StoredValue::new("key", JsonValue::from(1));
    let host = RuntimeCellHost {
        cell_id: CellId::new("delete"),
        parent_tool_call_id: "parent".into(),
        snapshot: HashMap::from([("key".into(), original)]),
        inner: Arc::clone(&runtime.inner),
        cell_permit: Mutex::new(None),
    };
    runtime.inner.stored_values.lock().await.insert("key".into(), StoredValue::new("key", JsonValue::from(2)));
    let state = Arc::new(CellState::new(CancellationToken::new()));
    host.commit_completion(HashMap::from([("key".into(), StoredValue::deletion("key"))]),
        CellEvent::Completed { content_items: Vec::new(), error_text: None, output_loss: None },
        None, Arc::clone(&state)).await;
    assert!(matches!(state.request_termination().await.unwrap(), CellEvent::Completed { error_text: Some(_), .. }));
    assert_eq!(*runtime.inner.stored_values.lock().await["key"].value, JsonValue::from(2));
    runtime.shutdown().await.unwrap();
}

#[test]
fn durable_restart_never_reuses_uncompleted_cell_ids() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let (first, _) = snapshot::DurableState::open(path.clone()).unwrap();
    let interrupted_id = first.first_cell_id;
    let reserved_limit = first.cell_id_limit;
    drop(first);
    let (restarted, values) = snapshot::DurableState::open(path).unwrap();
    assert!(values.is_empty());
    assert!(restarted.completed_cells().is_empty());
    assert!(restarted.first_cell_id >= reserved_limit);
    assert!(restarted.first_cell_id > interrupted_id);
}

#[test]
fn buffered_json_batches_writes_and_reports_flush_failures() {
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        writes: usize,
        fail_write: bool,
        fail_flush: bool,
    }
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            if self.fail_write {
                return Err(std::io::Error::other("write failed"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail_flush {
                return Err(std::io::Error::other("flush failed"));
            }
            Ok(())
        }
    }

    let value = serde_json::json!({"rows": vec!["line\nλ😀\"\\"; 2_000]});
    let mut writer = Writer::default();
    write_buffered_json(&mut writer, &value).unwrap();
    assert_eq!(writer.bytes, serde_json::to_vec(&value).unwrap());
    assert!(writer.writes < 10, "JSON tokens must share buffered writes");

    // Small values stay buffered until flush. Dropping the buffer would hide
    // this error and make publication of incomplete evidence look successful.
    for (fail_write, fail_flush, expected) in [
        (true, false, "write failed"),
        (false, true, "flush failed"),
    ] {
        let mut writer = Writer { fail_write, fail_flush, ..Writer::default() };
        assert_eq!(write_buffered_json(&mut writer, &true), Err(expected.into()));
    }
}

#[test]
fn buffered_snapshot_flushes_before_publication_and_restores_exact_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let (state, _) = snapshot::DurableState::open(path.clone()).unwrap();
    let original = std::fs::read(&path).unwrap();
    let value = serde_json::json!({"rows": vec!["line\nλ😀\"\\"; 2_000]});
    let event = CellEvent::Completed {
        content_items: vec![OutputItem::Text { text: "receipt\nλ😀".repeat(2_000) }],
        error_text: Some("original error".into()),
        output_loss: Some(codex_code_mode_protocol::OutputLoss {
            discarded_items: 2,
            discarded_bytes_lower_bound: 3,
        }),
    };
    let staged = state.stage(
        "call".into(),
        HashMap::from([("evidence".into(), StoredValue::new("evidence", value.clone()))]),
        "1".into(),
        event.clone(),
    ).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original, "staging must not publish");
    state.publish(staged).unwrap();
    drop(state);
    let (restored, values) = snapshot::DurableState::open(path).unwrap();
    assert_eq!(values["evidence"].value.as_ref(), &value);
    assert_eq!(restored.completed_cell("1"), Some(event));
}

#[test]
fn buffered_spill_read_preserves_receipts_and_rejects_corruption() {
    let event = CellEvent::Completed {
        content_items: vec![OutputItem::Text { text: "receipt\nλ😀\"\\".repeat(2_000) }],
        error_text: Some("original error".into()),
        output_loss: Some(codex_code_mode_protocol::OutputLoss {
            discarded_items: 2,
            discarded_bytes_lower_bound: 3,
        }),
    };
    let bytes = serde_json::to_vec(&event).unwrap();
    for suffix in [None, Some(b" ".as_slice()), Some(b"!".as_slice()), Some(b"".as_slice())] {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut payload = bytes.clone();
        match suffix {
            Some([]) => { payload.pop(); }
            Some(suffix) => payload.extend_from_slice(suffix),
            None => {}
        }
        std::fs::write(file.path(), payload).unwrap();
        let cached = CachedCellEvent::Spilled(file, cell_event_bytes(&event), terminal_completion(&event));
        let result = cached.read();
        if suffix == Some(b"!".as_slice()) || suffix == Some(b"".as_slice()) {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("cannot be decoded") && error.contains("do not replay"));
        } else {
            assert_eq!(result.unwrap(), event);
            assert_eq!(cached.read().unwrap(), event, "recovery must reopen at the start");
        }
    }
}

#[tokio::test]
async fn capacity_rejects_until_a_terminal_cell_releases_its_permit() {
    let runtime = Arc::new(SessionRuntime::new(Arc::new(RecordingDelegate)));
    let mut active_cell_ids = Vec::new();
    for slot in 0..runtime.inner.active_cell_capacity {
        if slot + 1 == runtime.inner.active_cell_capacity {
            let mut heavy = execute_request("store('rejected', true);");
            heavy.source.push_str(&" ".repeat(CELL_INPUT_BYTES_PER_SLOT));
            assert!(matches!(
                runtime.execute(heavy, ObserveMode::YieldAfter(Duration::from_millis(1))).await,
                Err(Error::ActiveCellLimit(_))
            ));
            assert_eq!(runtime.inner.active_cell_permits.available_permits(), 1);
        }
        let started = runtime
            .execute(
                execute_request("await new Promise(resolve => setTimeout(resolve, 600_000));"),
                ObserveMode::YieldAfter(Duration::from_millis(1)),
            )
            .await
            .expect("cell should be admitted");
        active_cell_ids.push(started.cell_id);
    }

    let excess = tokio::time::timeout(
        Duration::from_secs(1),
        runtime.execute(
            execute_request("store('rejected', true);"),
            ObserveMode::YieldAfter(Duration::from_millis(1)),
        ),
    )
    .await
    .expect("full capacity must return control to the caller");
    let Err(Error::ActiveCellLimit(reported)) = excess else {
        panic!("excess cell must be rejected at the active cell limit");
    };
    assert_eq!(reported, active_cell_ids);
    assert_eq!(runtime.inner.cells.lock().await.len(), runtime.inner.active_cell_capacity);

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
        runtime.inner.active_cell_capacity
    );
    assert!(runtime.inner.cell_tasks.is_empty());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.exited.notified())
        .await
        .unwrap();
    assert!(runtime.inner.stored_values.lock().await.is_empty());
}

#[tokio::test]
async fn native_startup_does_not_block_control_of_a_running_cell() {
    use crate::runtime::STARTUP_TEST_GATE;
    use crate::runtime::StartupTestGate;

    let runtime = SessionRuntime::new(Arc::new(RecordingDelegate));
    let running = runtime
        .execute(
            execute_request("while (true) {}"),
            ObserveMode::YieldAfter(Duration::from_millis(1)),
        )
        .await
        .unwrap();
    let running_id = running.cell_id.clone();
    assert_eq!(
        running.initial_event().await,
        Ok(CellEvent::Yielded {
            content_items: Vec::new(),
        })
    );

    let (release, receiver) = std::sync::mpsc::channel();
    let gate = Arc::new(StartupTestGate {
        entered: tokio::sync::Notify::new(),
        release: std::sync::Mutex::new(receiver),
        exited: tokio::sync::Notify::new(),
    });
    let starting = STARTUP_TEST_GATE.scope(
        Arc::clone(&gate),
        runtime.execute(
            execute_request(r#"text("started");"#),
            ObserveMode::YieldAfter(Duration::from_secs(1)),
        ),
    );
    tokio::pin!(starting);
    std::future::poll_fn(|cx| {
        assert!(starting.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    tokio::time::timeout(Duration::from_secs(1), gate.entered.notified())
        .await
        .unwrap();

    let terminated = tokio::time::timeout(Duration::from_secs(1), runtime.terminate(&running_id))
        .await
        .expect("terminating a running cell must not wait for another cell's startup");
    assert!(matches!(terminated, Ok(CellEvent::Terminated { .. })));

    release.send(()).unwrap();
    let started = starting.await.unwrap();
    assert_eq!(
        started.initial_event().await,
        Ok(CellEvent::Completed {
            output_loss: None,
            content_items: vec![OutputItem::Text {
                text: "started".to_string(),
            }],
            error_text: None,
        })
    );
    runtime.shutdown().await.unwrap();
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

#[test]
fn spilled_terminal_eviction_retains_completion_and_interruption() {
    let mut cache = TerminalCellCache::default();
    for (id, event) in [
        ("completed", CellEvent::Completed { content_items: Vec::new(), error_text: None, output_loss: None }),
        ("interrupted", CellEvent::Terminated { content_items: Vec::new() }),
    ] {
        let spill = CachedCellEvent::Spilled(tempfile::NamedTempFile::new().unwrap(), 1,
            terminal_completion(&event));
        cache.insert_cached(CellId::new(id), (Arc::new(spill), 0));
    }
    for id in 0..TERMINAL_CELL_CACHE_CAPACITY {
        cache.insert(CellId::new(id.to_string()), CellEvent::Completed {
            content_items: Vec::new(), error_text: None, output_loss: None,
        });
    }
    assert!(matches!(cache.lookup(&CellId::new("completed")), Err(Error::ExpiredResult { completed: true, .. })));
    assert!(matches!(cache.lookup(&CellId::new("interrupted")), Err(Error::ExpiredResult { completed: false, .. })));
}

#[test]
fn durable_receipts_preserve_completion_order_across_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let (state, _) = snapshot::DurableState::open(path.clone()).unwrap();
    drop(state);
    // Exercise legacy migration without hundreds of synchronous publications.
    let mut seed: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    seed["version"] = 5.into();
    seed.as_object_mut().unwrap().remove("completed_cell_order");
    let event = CellEvent::Completed { content_items: Vec::new(), error_text: None, output_loss: None };
    seed["completed_cells"] = serde_json::to_value((2..=256).map(|id|
        (id.to_string(), event.clone())).collect::<std::collections::BTreeMap<_, _>>()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&seed).unwrap()).unwrap();
    let (state, _) = snapshot::DurableState::open(path.clone()).unwrap();
    state.publish(state.stage("late".into(), HashMap::new(), "1".into(), event.clone()).unwrap()).unwrap();
    drop(state);
    let (state, _) = snapshot::DurableState::open(path).unwrap();
    assert_eq!(state.completed_cells().last().unwrap().0, "1");
    state.publish(state.stage("next".into(), HashMap::new(), "257".into(), event.clone()).unwrap()).unwrap();
    assert_eq!(state.completed_cell("1"), Some(event));
    assert!(state.completed_cell("2").is_none());
    assert_eq!(state.completed_cells().len(), TERMINAL_CELL_CACHE_CAPACITY);
}
