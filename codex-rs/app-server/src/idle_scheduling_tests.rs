//! Cancellation must make newly admissible requests runnable, not just remove work.
use super::*;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::oneshot;
use tokio::time::timeout;

struct CancelledWriterFixture {
    queues: RequestSerializationQueues,
    reader_gate: Arc<ConnectionRpcGate>,
    writer_gate: Arc<ConnectionRpcGate>,
    release: oneshot::Sender<()>,
    response: oneshot::Receiver<u32>,
}

impl CancelledWriterFixture {
    async fn new() -> Self {
        let queues = RequestSerializationQueues::with_limits(8, 8, 2);
        let key = RequestSerializationQueueKey::Global("cancelled-writer");
        let reader_gate = Arc::new(ConnectionRpcGate::new());
        let writer_gate = Arc::new(ConnectionRpcGate::new());
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release, released) = oneshot::channel();
        assert_eq!(queues.enqueue(
            key.clone(), RequestSerializationAccess::SharedRead,
            QueuedInitializedRequest::new(Arc::clone(&reader_gate), async move {
                entered_tx.send(()).unwrap();
                let _ = released.await;
            }),
        ).await, RequestAdmission::Accepted);
        timeout(Duration::from_secs(1), entered_rx).await.unwrap().unwrap();
        assert_eq!(queues.enqueue(
            key.clone(), RequestSerializationAccess::Exclusive,
            QueuedInitializedRequest::new(Arc::clone(&writer_gate), async {
                panic!("cancelled queued writer must never execute");
            }),
        ).await, RequestAdmission::Accepted);
        let (response_tx, mut response) = oneshot::channel();
        assert_eq!(queues.enqueue(
            key, RequestSerializationAccess::SharedRead,
            QueuedInitializedRequest::new(Arc::clone(&reader_gate), async move {
                response_tx.send(42).unwrap();
            }),
        ).await, RequestAdmission::Accepted);
        // Let the drain consume enqueue notifications and park behind the writer.
        // A current-thread runtime cannot advance this timer before runnable work.
        assert!(timeout(Duration::from_millis(10), &mut response).await.is_err());
        Self { queues, reader_gate, writer_gate, release, response }
    }

    async fn cancel_writer(&self) {
        self.writer_gate.close().await;
        assert_eq!(self.queues.cancel_for_gate(&self.writer_gate).await, 1);
        assert_eq!(self.writer_gate.shutdown().await, 0);
        let state = self.queues.inner.lock().await;
        assert_eq!(state.total_queued_bytes, 0);
        assert_eq!(state.total_control, 0);
    }
}

#[tokio::test]
async fn cancelled_writer_wakes_compatible_reader_without_another_enqueue() {
    let mut fixture = CancelledWriterFixture::new().await;
    fixture.cancel_writer().await;
    let response = timeout(Duration::from_millis(25), &mut fixture.response).await;
    fixture.release.send(()).unwrap();
    // Join before asserting so the failing baseline also cleans up every task.
    if response.is_err() {
        assert_eq!(timeout(Duration::from_secs(1), &mut fixture.response).await.unwrap().unwrap(), 42);
    }
    assert_eq!(fixture.reader_gate.shutdown().await, 0);
    assert_eq!(response.expect("reader waited for unrelated active work").unwrap(), 42);
}

#[tokio::test]
#[ignore = "narrow wall-clock scheduling benchmark"]
async fn cancelled_writer_wall_clock_benchmark() {
    let mut samples = Vec::new();
    for round in 0..8 {
        let mut fixture = CancelledWriterFixture::new().await;
        let start = Instant::now();
        fixture.cancel_writer().await;
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            fixture.release.send(()).unwrap();
        });
        assert_eq!(timeout(Duration::from_secs(1), &mut fixture.response).await.unwrap().unwrap(), 42);
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        release.await.unwrap();
        assert_eq!(fixture.reader_gate.shutdown().await, 0);
        if round > 0 { samples.push(elapsed_ms); }
    }
    let mut sorted = samples.clone();
    sorted.sort_by(f64::total_cmp);
    eprintln!("{}", serde_json::json!({
        "scenario": "cancelled-writer-to-compatible-reader-response",
        "samples_ms": samples, "median_ms": sorted[sorted.len()/2],
        "unrelated_reader_hold_ms": 120,
        "scope": "native queue, connection gate and handler response; no model or transport"
    }));
}
