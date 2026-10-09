use std::time::Duration;

use pretty_assertions::assert_eq;

use super::*;
use crate::protocol::ExecOutputStream;
use crate::protocol::ProcessOutputChunk;

fn registry_error(status: http::StatusCode, code: Option<&str>) -> ExecServerError {
    ExecServerError::EnvironmentRegistryHttp {
        status,
        code: code.map(str::to_string),
        message: "registry unavailable".to_string(),
    }
}

#[tokio::test]
async fn recovery_reads_healthy_process_while_another_reply_is_held() {
    use codex_exec_server_protocol::JSONRPCMessage;
    use codex_exec_server_protocol::JSONRPCResponse;
    use crate::connection::JsonRpcConnection;
    use crate::connection::JsonRpcConnectionEvent;
    use crate::connection::JsonRpcTransport;

    let (outgoing_tx, mut requests) = tokio::sync::mpsc::channel(16);
    let (replies, incoming_rx) = tokio::sync::mpsc::channel(16);
    let (_connected, disconnected_rx) = tokio::sync::watch::channel(false);
    let connecting = ExecServerClient::connect(JsonRpcConnection {
        outgoing_tx, incoming_rx, disconnected_rx,
        task_handles: Vec::new(), transport: JsonRpcTransport::Plain,
    }, super::super::ExecServerClientConnectOptions::default());
    tokio::pin!(connecting);
    assert!(futures::poll!(&mut connecting).is_pending());
    let JSONRPCMessage::Request(initialize) = requests.recv().await.unwrap() else {
        panic!("expected initialize request");
    };
    replies.send(JsonRpcConnectionEvent::Message(JSONRPCMessage::Response(JSONRPCResponse {
        id: initialize.id,
        result: serde_json::json!({"sessionId": "concurrent-recovery"}),
    }))).await.unwrap();
    let client = connecting.await.unwrap();
    assert!(matches!(requests.recv().await, Some(JSONRPCMessage::Notification(_))));

    let mut observations = std::collections::HashMap::new();
    for name in ["one", "two"] {
        let state = Arc::new(SessionState::new(true));
        observations.insert(name.to_string(), state.subscribe_events());
        client.inner.insert_session(&crate::ProcessId::from(name.to_string()), state).unwrap();
    }
    let rpc = client.rpc_client_without_recovery().unwrap();
    let recovery = client.inner.recover_processes(&rpc);
    tokio::pin!(recovery);
    assert!(futures::poll!(&mut recovery).is_pending());
    let mut reads = Vec::new();
    for _ in 0..2 {
        let message = tokio::time::timeout(Duration::from_secs(1), requests.recv()).await
            .expect("both recovery reads must dispatch before either reply").unwrap();
        let JSONRPCMessage::Request(request) = message else { panic!("expected read request") };
        assert_eq!(request.method, EXEC_READ_METHOD);
        reads.push(request);
    }
    let held = reads.remove(0);
    let healthy = reads.remove(0);
    let params: ReadParams = serde_json::from_value(healthy.params.clone().unwrap()).unwrap();
    assert_eq!(params.after_seq, Some(0));
    let response = ReadResponse {
        chunks: vec![ProcessOutputChunk { seq: 1, stream: ExecOutputStream::Stdout,
            chunk: b"recovered".to_vec().into() }],
        next_seq: 2, exited: false, exit_code: None, closed: false,
        failure: None, output_gap: None, sandbox_denied: false,
    };
    replies.send(JsonRpcConnectionEvent::Message(JSONRPCMessage::Response(JSONRPCResponse {
        id: healthy.id, result: serde_json::to_value(&response).unwrap(),
    }))).await.unwrap();
    let events = observations.get_mut(&params.process_id.to_string()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            _ = &mut recovery => panic!("held reply must keep recovery open"),
            event = events.recv() => assert!(matches!(event.unwrap(),
                ExecProcessEvent::Output(chunk) if chunk.seq == 1 && chunk.chunk.0 == b"recovered")),
        }
    }).await.expect("healthy process publishes its prefix without waiting for held process");
    replies.send(JsonRpcConnectionEvent::Message(JSONRPCMessage::Response(JSONRPCResponse {
        id: held.id, result: serde_json::to_value(response).unwrap(),
    }))).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), recovery).await.unwrap().unwrap();
}

#[tokio::test]
async fn output_gap_recovers_live_suffix_and_authoritative_exit() {
    let state = SessionState::new(true);
    let mut events = state.subscribe_events();
    assert!(!state.recover_events(ReadResponse {
        chunks: vec![ProcessOutputChunk { seq: 5, stream: ExecOutputStream::Stdout,
            chunk: b"suffix".to_vec().into() }],
        next_seq: 6, exited: false, exit_code: None, closed: false, failure: None,
        output_gap: Some(crate::protocol::ProcessOutputGap { through_seq: 4, exit_seq: None }),
        sandbox_denied: false,
    }).unwrap());
    assert_eq!(events.recv().await.unwrap(), ExecProcessEvent::OutputGap { through_seq: 4 });
    assert!(matches!(events.recv().await.unwrap(), ExecProcessEvent::Output(chunk) if chunk.chunk.0 == b"suffix"));
    assert!(state.recoverable.load(Ordering::Acquire));
    assert!(state.failed_response().is_none());

    assert!(state.recover_events(ReadResponse {
        chunks: vec![ProcessOutputChunk { seq: 9, stream: ExecOutputStream::Stdout,
            chunk: b"tail".to_vec().into() }],
        next_seq: 11, exited: true, exit_code: Some(7), closed: true, failure: None,
        output_gap: Some(crate::protocol::ProcessOutputGap { through_seq: 8, exit_seq: Some(7) }),
        sandbox_denied: false,
    }).unwrap());
    assert_eq!(events.recv().await.unwrap(), ExecProcessEvent::OutputGap { through_seq: 6 });
    assert!(matches!(events.recv().await.unwrap(), ExecProcessEvent::Exited { seq: 7, exit_code: 7, .. }));
    assert_eq!(events.recv().await.unwrap(), ExecProcessEvent::OutputGap { through_seq: 8 });
    assert!(matches!(events.recv().await.unwrap(), ExecProcessEvent::Output(chunk) if chunk.seq == 9));
    assert!(matches!(events.recv().await.unwrap(), ExecProcessEvent::Closed { seq: 10, .. }));
}

#[test]
fn registry_recovery_retry_delay_exponentially_backs_off_and_caps() {
    let cases = [
        (0, Duration::from_millis(500)),
        (1, Duration::from_secs(1)),
        (2, Duration::from_secs(2)),
        (3, Duration::from_secs(4)),
        (4, Duration::from_secs(5)),
        (20, Duration::from_secs(5)),
    ];

    for (attempt, base) in cases {
        let delay = registry_recovery_retry_delay("session-1", attempt);
        assert!(delay >= base, "delay {delay:?} for attempt {attempt}");
        assert!(
            delay <= base + base / 2,
            "delay {delay:?} for attempt {attempt}"
        );
    }
}

#[tokio::test]
async fn output_gap_rejects_missing_or_overlapping_terminal_exit() {
    for exit_seq in [None, Some(0), Some(6)] {
        let state = SessionState::new(true);
        let mut events = state.subscribe_events();
        state.recover_events(ReadResponse {
            chunks: Vec::new(), next_seq: 7, exited: exit_seq.is_some(),
            exit_code: exit_seq.map(|_| 7), closed: true, failure: None,
            output_gap: Some(crate::protocol::ProcessOutputGap { through_seq: 5, exit_seq }),
            sandbox_denied: false,
        }).expect_err("close cannot replace or omit the authoritative exit");
        {
            let next_event = events.recv();
            tokio::pin!(next_event);
            assert!(futures::poll!(&mut next_event).is_pending());
        }
        assert_eq!(state.last_published_seq(), 0);
        assert!(state.ordered_events.lock().unwrap().pending.is_empty());

        // A rejected replay must not poison a later valid replay. The terminal
        // events occupy distinct positions after the declared evicted prefix.
        assert!(state.recover_events(ReadResponse {
            chunks: Vec::new(), next_seq: 7, exited: true, exit_code: Some(7),
            closed: true, failure: None,
            output_gap: Some(crate::protocol::ProcessOutputGap {
                through_seq: 4, exit_seq: Some(5),
            }),
            sandbox_denied: false,
        }).unwrap());
        assert_eq!(events.recv().await.unwrap(), ExecProcessEvent::OutputGap { through_seq: 4 });
        assert!(matches!(events.recv().await.unwrap(),
            ExecProcessEvent::Exited { seq: 5, exit_code: 7, .. }));
        assert!(matches!(events.recv().await.unwrap(), ExecProcessEvent::Closed { seq: 6, .. }));
    }
}

#[test]
fn recovery_classifies_registry_status_and_conflict_code() {
    for (status, code, expected) in [
        (http::StatusCode::TOO_MANY_REQUESTS, None, true),
        (http::StatusCode::SERVICE_UNAVAILABLE, None, true),
        (http::StatusCode::REQUEST_TIMEOUT, None, true),
        (http::StatusCode::CONFLICT, Some("environment_offline"), true),
        (http::StatusCode::CONFLICT, Some("registration_conflict"), false),
        (http::StatusCode::CONFLICT, None, false),
        (http::StatusCode::UNAUTHORIZED, None, false),
        (http::StatusCode::FORBIDDEN, Some("environment_offline"), false),
        (http::StatusCode::NOT_FOUND, None, false),
    ] {
        let error = registry_error(status, code);
        assert_eq!(is_retryable_registry_error(&error), expected, "{status} {code:?}");
        assert_eq!(is_retryable_recovery_error(&error), expected, "{status} {code:?}");
    }
}
#[test]
fn process_event_reorder_rejects_oversized_output() {
    let state = SessionState::new(/*recoverable*/ true);

    let error = state
        .publish_ordered_event(ExecProcessEvent::Output(ProcessOutputChunk {
            seq: 1,
            stream: ExecOutputStream::Stdout,
            chunk: vec![0; super::super::MAX_PENDING_PROCESS_EVENT_BYTES + 1].into(),
        }))
        .expect_err("oversized pending process output should be rejected");

    assert!(error.contains("bytes"));
}

#[test]
fn process_event_reorder_accepts_gap_closing_event_at_limits() {
    let state = SessionState::new(/*recoverable*/ true);
    let chunk_size =
        super::super::MAX_PENDING_PROCESS_EVENT_BYTES / super::super::MAX_PENDING_PROCESS_EVENTS;
    let last_seq = super::super::MAX_PENDING_PROCESS_EVENTS as u64 + 1;

    for seq in 2..=last_seq {
        assert!(
            !state
                .publish_ordered_event(ExecProcessEvent::Output(ProcessOutputChunk {
                    seq,
                    stream: ExecOutputStream::Stdout,
                    chunk: vec![0; chunk_size].into(),
                }))
                .expect("future output should fit within reorder limits")
        );
    }
    assert!(
        !state
            .publish_ordered_event(ExecProcessEvent::Output(ProcessOutputChunk {
                seq: 1,
                stream: ExecOutputStream::Stdout,
                chunk: b"x".to_vec().into(),
            }))
            .expect("gap-closing output should drain the reorder buffer")
    );

    let ordered_events = state
        .ordered_events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        (
            ordered_events.last_published_seq,
            ordered_events.pending.len(),
            ordered_events.pending_bytes,
        ),
        (last_seq, 0, 0)
    );
}

#[test]
fn recovery_handles_dense_tail_output_and_newer_notification() {
    let state = SessionState::new(/*recoverable*/ true);
    let last_seq = super::super::MAX_PENDING_PROCESS_EVENTS as u64 + 2;
    let live_seq = last_seq + 1;
    assert!(
        !state
            .publish_ordered_event(ExecProcessEvent::Output(ProcessOutputChunk {
                seq: live_seq,
                stream: ExecOutputStream::Stdout,
                chunk: b"live".to_vec().into(),
            }))
            .expect("live output should remain bounded while recovery fills the gap")
    );
    let chunks = (2..=last_seq)
        .map(|seq| ProcessOutputChunk {
            seq,
            stream: ExecOutputStream::Stdout,
            chunk: b"x".to_vec().into(),
        })
        .collect();

    assert!(
        !state
            .recover_events(ReadResponse {
                chunks,
                next_seq: last_seq + 1,
                exited: true,
                exit_code: Some(17),
                closed: false,
                failure: None,
                output_gap: None,
                sandbox_denied: false,
            })
            .expect("dense retained output should recover")
    );

    let ordered_events = state
        .ordered_events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        (
            ordered_events.last_published_seq,
            ordered_events.pending.len(),
            ordered_events.pending_bytes,
        ),
        (live_seq, 0, 0)
    );
}

#[test]
fn recovery_rejects_output_at_closed_sequence() {
    let state = SessionState::new(/*recoverable*/ true);

    let error = state
        .recover_events(ReadResponse {
            chunks: vec![ProcessOutputChunk {
                seq: 1,
                stream: ExecOutputStream::Stdout,
                chunk: b"output".to_vec().into(),
            }],
            next_seq: 2,
            exited: false,
            exit_code: None,
            closed: true,
            failure: None,
            output_gap: None,
            sandbox_denied: false,
        })
        .expect_err("output should not occupy the closed sequence");

    assert!(
        error
            .to_string()
            .contains("conflicts with recovered output")
    );
}

#[tokio::test]
async fn recovery_adds_sandbox_denial_to_pending_exit_event() {
    let state = SessionState::new(/*recoverable*/ true);
    assert!(
        !state
            .publish_ordered_event(ExecProcessEvent::Exited {
                seq: 2,
                exit_code: 1,
                sandbox_denied: None,
            })
            .expect("pending exit should fit within reorder limits")
    );

    state
        .recover_events(ReadResponse {
            chunks: vec![ProcessOutputChunk {
                seq: 1,
                stream: ExecOutputStream::Stderr,
                chunk: b"sandbox denied".to_vec().into(),
            }],
            next_seq: 3,
            exited: true,
            exit_code: Some(1),
            closed: false,
            failure: None,
            output_gap: None,
            sandbox_denied: true,
        })
        .expect("recovery should publish the pending exit");

    let mut events = state.subscribe_events();
    assert!(matches!(
        events.recv().await,
        Ok(ExecProcessEvent::Output(_))
    ));
    assert_eq!(
        events.recv().await,
        Ok(ExecProcessEvent::Exited {
            seq: 2,
            exit_code: 1,
            sandbox_denied: Some(true),
        })
    );
}

#[tokio::test]
async fn recovery_rejects_ambiguous_gap_before_publishing_false_exit() {
    let state = SessionState::new(true);
    let mut events = state.subscribe_events();
    let error = state
        .recover_events(ReadResponse {
            chunks: vec![ProcessOutputChunk {
                seq: 2,
                stream: ExecOutputStream::Stdout,
                chunk: b"retained".to_vec().into(),
            }],
            next_seq: 5,
            exited: true,
            exit_code: Some(0),
            closed: true,
            failure: None,
            output_gap: None,
            sandbox_denied: false,
        })
        .expect_err("multiple missing positions cannot identify an exit");
    assert!(error.to_string().contains("no longer retained"));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), events.recv())
            .await
            .is_err(),
        "invalid replay must not publish an invented exit"
    );
}
