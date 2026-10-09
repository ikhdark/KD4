//! Wall-clock split of the turn/start dispatch path: transport-loop occupancy,
//! queue hand-off, response, and mock-model completion.
use super::*;
use crate::connection_rpc_gate::ConnectionRpcGate;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingMessage;
use crate::request_serialization::QueuedInitializedRequest;
use crate::request_serialization::RequestAdmission;
use crate::request_serialization::RequestSerializationAccess;
use crate::request_serialization::RequestSerializationQueueKey;
use crate::request_serialization::RequestSerializationQueues;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::ServerNotification;
use pretty_assertions::assert_eq;
use std::time::Duration;
use std::time::Instant;

const PAYLOADS: [(&str, usize); 3] = [("small", 64), ("64KiB", 64 * 1024), ("512KiB", 512 * 1024)];
const WARMUP_ROUNDS: usize = 2;
const MEASURED_ROUNDS: usize = 10;

fn turn_line(id: i64, thread_id: &str, text: &str) -> String {
    json!({
        "id": id,
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [{"type": "text", "text": text, "textElements": []}],
        },
    })
    .to_string()
}

fn parse_request(line: &str) -> JSONRPCRequest {
    match serde_json::from_str::<JSONRPCMessage>(line).expect("parse turn/start line") {
        JSONRPCMessage::Request(request) => request,
        other => panic!("expected request, got {other:?}"),
    }
}

fn median_us(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn run_multi_thread_with_stack<F>(name: &str, future: F) -> Result<()>
where
    F: Future<Output = Result<()>> + Send + 'static,
{
    let handle = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || -> Result<()> {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()?
                .block_on(Box::pin(future))
        })?;
    handle
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("{name} thread panicked")))
}

struct TurnTiming {
    inline_us: f64,
    response_us: f64,
    started_us: f64,
    completed_us: f64,
}

async fn timed_turn(harness: &mut TracingHarness, id: i64, line: &str) -> TurnTiming {
    let request = parse_request(line);
    let start = Instant::now();
    harness
        .processor
        .process_request(
            TEST_CONNECTION_ID,
            request,
            &AppServerTransport::Stdio,
            Arc::clone(&harness.session),
        )
        .await;
    let inline = start.elapsed();
    let (mut response, mut started, mut completed) = (None, None, None);
    while response.is_none() || started.is_none() || completed.is_none() {
        let envelope = tokio::time::timeout(Duration::from_secs(20), harness.outgoing_rx.recv())
            .await
            .expect("turn deadline")
            .expect("outgoing open");
        let message = match envelope {
            OutgoingEnvelope::ToConnection { message, .. } => message,
            OutgoingEnvelope::Broadcast { message } => message,
        };
        match message {
            OutgoingMessage::Response(r) if r.id == RequestId::Integer(id) => {
                response = Some(start.elapsed());
            }
            OutgoingMessage::Error(error) => panic!("turn/start failed: {error:?}"),
            OutgoingMessage::AppServerNotification(ServerNotification::TurnStarted(_)) => {
                started = Some(start.elapsed());
            }
            OutgoingMessage::AppServerNotification(ServerNotification::TurnCompleted(_)) => {
                completed = Some(start.elapsed());
            }
            OutgoingMessage::AppServerNotification(ServerNotification::Error(error)) => {
                panic!("turn failed: {error:?}");
            }
            _ => {}
        }
    }
    let us = |duration: Option<Duration>| duration.unwrap().as_secs_f64() * 1e6;
    TurnTiming {
        inline_us: inline.as_secs_f64() * 1e6,
        response_us: us(response),
        started_us: us(started),
        completed_us: us(completed),
    }
}

#[test]
#[ignore = "narrow wall-clock dispatch benchmark"]
#[serial(app_server_tracing)]
#[expect(clippy::print_stderr, reason = "this dispatch benchmark emits machine-readable latency measurements")]
fn turn_start_dispatch_wall_clock_benchmark() -> Result<()> {
    run_multi_thread_with_stack("turn_start_dispatch_wall_clock_benchmark", async {
        let mut harness = TracingHarness::new().await?;
        let thread_id = harness.start_thread(2, None).await.thread.id;
        let mut next_id = 100;
        for (label, bytes) in PAYLOADS {
            let text = "x".repeat(bytes);
            let mut rows: Vec<TurnTiming> = Vec::new();
            for round in 0..WARMUP_ROUNDS + MEASURED_ROUNDS {
                next_id += 1;
                let line = turn_line(next_id, &thread_id, &text);
                let timing = timed_turn(&mut harness, next_id, &line).await;
                if round >= WARMUP_ROUNDS {
                    rows.push(timing);
                }
            }
            let pick = |f: fn(&TurnTiming) -> f64| {
                median_us(&mut rows.iter().map(f).collect::<Vec<_>>())
            };
            eprintln!(
                "{}",
                json!({
                    "scenario": "turn-start-end-to-end",
                    "payload": label,
                    "rounds": MEASURED_ROUNDS,
                    "median_inline_transport_loop_us": pick(|t| t.inline_us),
                    "median_request_to_response_us": pick(|t| t.response_us),
                    "median_request_to_turn_started_us": pick(|t| t.started_us),
                    "median_request_to_turn_completed_us": pick(|t| t.completed_us),
                })
            );
        }
        harness.shutdown().await;
        Ok(())
    })
}

#[test]
#[ignore = "narrow CPU split of inline turn/start dispatch work"]
#[expect(clippy::print_stderr, reason = "this dispatch benchmark emits machine-readable CPU timing measurements")]
fn turn_start_inline_cpu_split_benchmark() {
    for (label, bytes) in PAYLOADS {
        let line = turn_line(1, "00000000-0000-0000-0000-000000000000", &"x".repeat(bytes));
        let (mut parse, mut typed, mut estimate) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..WARMUP_ROUNDS + MEASURED_ROUNDS * 3 {
            let start = Instant::now();
            let request = parse_request(&line);
            parse.push(start.elapsed().as_secs_f64() * 1e6);
            let start = Instant::now();
            let client_request = super::super::deserialize_client_request(request).unwrap();
            typed.push(start.elapsed().as_secs_f64() * 1e6);
            let start = Instant::now();
            let estimated = serialized_request_queue_bytes(true, &client_request);
            estimate.push(start.elapsed().as_secs_f64() * 1e6);
            assert!(estimated >= bytes);
        }
        eprintln!(
            "{}",
            json!({
                "scenario": "turn-start-inline-cpu",
                "payload": label,
                "line_bytes": line.len(),
                "median_transport_parse_us": median_us(&mut parse),
                "median_typed_deserialize_us": median_us(&mut typed),
                "median_queue_byte_estimate_us": median_us(&mut estimate),
            })
        );
    }
}

#[test]
#[ignore = "narrow wall-clock queue hand-off benchmark"]
#[expect(clippy::print_stderr, reason = "this queue benchmark emits machine-readable hand-off timing measurements")]
fn idle_thread_queue_handoff_wall_clock_benchmark() -> Result<()> {
    run_multi_thread_with_stack("idle_thread_queue_handoff_wall_clock_benchmark", async {
        let queues = RequestSerializationQueues::default();
        let gate = Arc::new(ConnectionRpcGate::new());
        let mut enqueue = Vec::new();
        let mut handoff = Vec::new();
        for round in 0..200 {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let start = Instant::now();
            let admission = queues
                .enqueue(
                    RequestSerializationQueueKey::Thread {
                        thread_id: format!("thread-{}", round % 4),
                    },
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(Arc::clone(&gate), async move {
                        let _ = entered_tx.send(Instant::now());
                    }),
                )
                .await;
            let enqueued = start.elapsed();
            assert_eq!(admission, RequestAdmission::Accepted);
            let entered = entered_rx.await.expect("handler entered");
            if round >= 20 {
                enqueue.push(enqueued.as_secs_f64() * 1e6);
                handoff.push(entered.duration_since(start).as_secs_f64() * 1e6);
            }
            // Let the drain retire its key so every round measures an idle key.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(gate.shutdown().await, 0);
        eprintln!(
            "{}",
            json!({
                "scenario": "idle-thread-key-enqueue-to-handler",
                "rounds": enqueue.len(),
                "median_enqueue_us": median_us(&mut enqueue),
                "median_enqueue_to_handler_entry_us": median_us(&mut handoff),
            })
        );
        Ok(())
    })
}
