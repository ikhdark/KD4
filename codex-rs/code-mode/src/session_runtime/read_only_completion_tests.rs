use super::*;

struct ReadOnlyDelegate;

impl SessionRuntimeDelegate for ReadOnlyDelegate {
    async fn invoke_tool(
        &self,
        _invocation: NestedToolCall,
        _cancellation: NestedCancellation,
    ) -> Result<JsonValue, String> {
        Ok(JsonValue::Null)
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

fn host(runtime: &SessionRuntime<ReadOnlyDelegate>) -> RuntimeCellHost<ReadOnlyDelegate> {
    RuntimeCellHost {
        cell_id: CellId::new("1"),
        parent_tool_call_id: "read-only".into(),
        snapshot: HashMap::new(),
        inner: Arc::clone(&runtime.inner),
        cell_permit: Mutex::new(None),
    }
}

fn event() -> CellEvent {
    CellEvent::Completed {
        content_items: vec![OutputItem::Text {
            text: "after λ".into(),
        }],
        error_text: None,
        output_loss: None,
    }
}

#[tokio::test]
#[ignore = "narrow contention timing probe"]
async fn read_only_completion_contention_probe() {
    let runtime = SessionRuntime::new(Arc::new(ReadOnlyDelegate));
    let host = host(&runtime);
    let mut samples = Vec::new();
    for _ in 0..7 {
        let gate = Arc::clone(&runtime.inner.commit_gate).lock_owned().await;
        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            drop(gate);
        });
        let started = std::time::Instant::now();
        let committed = host
            .commit_completion(
                HashMap::new(),
                event(),
                None,
                Arc::new(CellState::new(CancellationToken::new())),
            )
            .await;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(committed, CompletionCommit::Committed);
        release.await.unwrap();
    }
    crate::runtime::critical_path_tests::report("read-only-completion-contention", &samples);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn read_only_completion_does_not_wait_on_unrelated_transaction() {
    let runtime = SessionRuntime::new(Arc::new(ReadOnlyDelegate));
    let host = host(&runtime);
    let gate = runtime.inner.commit_gate.lock().await;
    let values = runtime.inner.stored_values.lock().await;
    let state = Arc::new(CellState::new(CancellationToken::new()));
    let before = Some(vec![OutputItem::Text {
        text: "before\r\n".into(),
    }]);
    let expected = crate::cell_actor::prepend_initial_yield(event(), before.clone());
    let committed = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        host.commit_completion(HashMap::new(), event(), before, Arc::clone(&state)),
    )
    .await
    .expect("read-only completion must not acquire shared transaction locks");
    assert_eq!(committed, CompletionCommit::Committed);
    assert_eq!(state.request_termination().await.unwrap(), expected);
    assert!(values.is_empty());
    drop(values);
    drop(gate);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn read_only_completion_still_rejects_cancellation() {
    let runtime = SessionRuntime::new(Arc::new(ReadOnlyDelegate));
    let host = host(&runtime);
    let token = CancellationToken::new();
    token.cancel();
    assert_eq!(
        host.commit_completion(
            HashMap::new(),
            event(),
            None,
            Arc::new(CellState::new(token))
        )
        .await,
        CompletionCommit::Rejected(event())
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn durable_read_only_completion_still_owns_publication() {
    let runtime = SessionRuntime::new(Arc::new(ReadOnlyDelegate));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    runtime.restore_durable_state(Some(&path)).await.unwrap();
    let host = host(&runtime);
    let gate = runtime.inner.commit_gate.lock().await;
    let completion = host.commit_completion(
        HashMap::new(),
        event(),
        None,
        Arc::new(CellState::new(CancellationToken::new())),
    );
    tokio::pin!(completion);
    assert!(futures::poll!(&mut completion).is_pending());
    drop(gate);
    assert_eq!(completion.await, CompletionCommit::Committed);
    assert_eq!(
        runtime
            .inner
            .durable_state
            .get()
            .unwrap()
            .completed_cell("1"),
        Some(event())
    );
    runtime.shutdown().await.unwrap();
}
