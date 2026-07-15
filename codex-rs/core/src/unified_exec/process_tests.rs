use super::process::UnifiedExecProcess;
use super::process::merge_recovery_gap;
use super::process::recovery_incomplete_detail;
use crate::unified_exec::UnifiedExecError;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ExecServerError;
use codex_exec_server::OutputGap;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_exec_server::WriteStatus;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::watch;

struct MockExecProcess {
    process_id: ProcessId,
    write_response: WriteResponse,
    read_responses: Mutex<VecDeque<ReadResponse>>,
    read_calls: Mutex<Vec<(Option<u64>, Option<usize>, Option<usize>, Option<u64>)>>,
    terminate_error: Option<String>,
    wake_tx: watch::Sender<u64>,
}

impl MockExecProcess {
    async fn read_with_limits(
        &self,
        after_seq: Option<u64>,
        max_bytes: Option<usize>,
        max_chunks: Option<usize>,
        wait_ms: Option<u64>,
    ) -> Result<ReadResponse, ExecServerError> {
        self.read_calls
            .lock()
            .await
            .push((after_seq, max_bytes, max_chunks, wait_ms));
        Ok(self
            .read_responses
            .lock()
            .await
            .pop_front()
            .unwrap_or(ReadResponse {
                chunks: Vec::new(),
                output_gaps: Vec::new(),
                earliest_retained_seq: Some(1),
                complete: Some(true),
                next_seq: 1,
                exited: false,
                exit_code: None,
                closed: false,
                failure: None,
                sandbox_denied: false,
            }))
    }

    async fn terminate(&self) -> Result<(), ExecServerError> {
        if let Some(message) = &self.terminate_error {
            return Err(ExecServerError::Protocol(message.clone()));
        }
        Ok(())
    }
}

impl ExecProcess for MockExecProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake_tx.subscribe()
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        after_seq: Option<u64>,
        max_bytes: Option<usize>,
        wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(MockExecProcess::read_with_limits(
            self, after_seq, max_bytes, None, wait_ms,
        ))
    }

    fn read_with_limits(
        &self,
        after_seq: Option<u64>,
        max_bytes: Option<usize>,
        max_chunks: Option<usize>,
        wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(MockExecProcess::read_with_limits(
            self, after_seq, max_bytes, max_chunks, wait_ms,
        ))
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async { Ok(self.write_response.clone()) })
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(MockExecProcess::terminate(self))
    }
}

async fn remote_process(
    write_status: WriteStatus,
    terminate_error: Option<String>,
) -> UnifiedExecProcess {
    let (wake_tx, _wake_rx) = watch::channel(0);
    let started = StartedExecProcess {
        process: Arc::new(MockExecProcess {
            process_id: "test-process".to_string().into(),
            write_response: WriteResponse {
                status: write_status,
            },
            read_responses: Mutex::new(VecDeque::new()),
            read_calls: Mutex::new(Vec::new()),
            terminate_error,
            wake_tx,
        }),
    };

    UnifiedExecProcess::from_exec_server_started(started, None)
        .await
        .expect("remote process should start")
}

#[tokio::test]
async fn tail_only_recovery_gap_is_observed_from_the_wake_channel() {
    let (wake_tx, _wake_rx) = watch::channel(0);
    let mock = Arc::new(MockExecProcess {
        process_id: "gap-process".to_string().into(),
        write_response: WriteResponse {
            status: WriteStatus::Accepted,
        },
        read_responses: Mutex::new(VecDeque::from([ReadResponse {
            chunks: Vec::new(),
            output_gaps: vec![OutputGap {
                first_missing_seq: 1,
                last_missing_seq: 2,
            }],
            earliest_retained_seq: Some(3),
            complete: Some(true),
            next_seq: 3,
            exited: false,
            exit_code: None,
            closed: false,
            failure: None,
            sandbox_denied: false,
        }])),
        read_calls: Mutex::new(Vec::new()),
        terminate_error: None,
        wake_tx,
    });
    let process = UnifiedExecProcess::from_exec_server_started(
        StartedExecProcess {
            process: Arc::clone(&mock) as Arc<dyn ExecProcess>,
        },
        None,
    )
    .await
    .expect("remote process should start");

    mock.wake_tx.send(2).expect("wake recovery reader");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let rendered = process
                .output_handles()
                .output_buffer
                .lock()
                .await
                .render_bytes();
            if String::from_utf8_lossy(&rendered).contains("missing sequences 1-2") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("tail-only recovery gap should become durable evidence");
}

#[tokio::test]
async fn recovery_pages_use_explicit_completeness_and_preserve_whole_stream_bytes() {
    let (wake_tx, _wake_rx) = watch::channel(0);
    let first = vec![b'a'; 60 * 1024];
    let second = [vec![0_u8, 0xff, b'Z'], vec![b'b'; 8 * 1024]].concat();
    let expected = [first.clone(), second.clone()].concat();
    let mock = Arc::new(MockExecProcess {
        process_id: "paged-process".to_string().into(),
        write_response: WriteResponse {
            status: WriteStatus::Accepted,
        },
        read_responses: Mutex::new(VecDeque::from([
            ReadResponse {
                chunks: vec![codex_exec_server::ProcessOutputChunk {
                    seq: 1,
                    stream: codex_exec_server::ExecOutputStream::Stdout,
                    chunk: first.into(),
                }],
                output_gaps: Vec::new(),
                earliest_retained_seq: Some(1),
                complete: Some(false),
                next_seq: 2,
                exited: false,
                exit_code: None,
                closed: false,
                failure: None,
                sandbox_denied: false,
            },
            ReadResponse {
                chunks: vec![codex_exec_server::ProcessOutputChunk {
                    seq: 2,
                    stream: codex_exec_server::ExecOutputStream::Stdout,
                    chunk: second.into(),
                }],
                output_gaps: Vec::new(),
                earliest_retained_seq: Some(1),
                complete: Some(true),
                next_seq: 3,
                exited: false,
                exit_code: None,
                closed: false,
                failure: None,
                sandbox_denied: false,
            },
        ])),
        read_calls: Mutex::new(Vec::new()),
        terminate_error: None,
        wake_tx,
    });
    let process = UnifiedExecProcess::from_exec_server_started(
        StartedExecProcess {
            process: Arc::clone(&mock) as Arc<dyn ExecProcess>,
        },
        None,
    )
    .await
    .expect("remote process should start");

    mock.wake_tx.send(2).expect("wake recovery reader");
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let rendered = process
                .output_handles()
                .output_buffer
                .lock()
                .await
                .render_bytes();
            if rendered.len() >= expected.len() {
                assert_eq!(rendered, expected);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("paged recovery should complete");

    let expected_page_max_bytes = 64 * 1024;
    let expected_page_max_chunks = 128;
    assert_eq!(
        *mock.read_calls.lock().await,
        vec![
            (
                Some(0),
                Some(expected_page_max_bytes),
                Some(expected_page_max_chunks),
                Some(0),
            ),
            (
                Some(1),
                Some(expected_page_max_bytes),
                Some(expected_page_max_chunks),
                Some(0),
            ),
        ]
    );
}

#[tokio::test]
async fn remote_write_unknown_process_marks_process_exited() {
    let process = remote_process(WriteStatus::UnknownProcess, /*terminate_error*/ None).await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn remote_write_closed_stdin_marks_process_exited() {
    let process = remote_process(WriteStatus::StdinClosed, /*terminate_error*/ None).await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn fail_and_terminate_preserves_failure_message() {
    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;

    process.fail_and_terminate("network denied".to_string());
    process.fail_and_terminate("second failure".to_string());

    assert!(process.has_exited());
    assert_eq!(
        process.failure_message(),
        Some("network denied".to_string())
    );
}

#[tokio::test]
async fn remote_terminate_confirmed_updates_state_on_success_only() {
    let process = remote_process(
        WriteStatus::Accepted,
        Some("terminate unavailable".to_string()),
    )
    .await;

    let err = process
        .terminate_confirmed()
        .await
        .expect_err("expected terminate failure");

    assert!(matches!(err, UnifiedExecError::ProcessFailed { .. }));
    assert!(!process.has_exited());

    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;

    process
        .terminate_confirmed()
        .await
        .expect("terminate should succeed");

    assert!(process.has_exited());
}

#[test]
fn recovery_gaps_merge_only_when_adjacent_or_overlapping() {
    let recovered = BTreeSet::new();
    let mut gaps = Vec::new();

    assert!(merge_recovery_gap(
        &mut gaps,
        OutputGap {
            first_missing_seq: 2,
            last_missing_seq: 3,
        },
        &recovered,
    ));
    assert!(merge_recovery_gap(
        &mut gaps,
        OutputGap {
            first_missing_seq: 4,
            last_missing_seq: 5,
        },
        &recovered,
    ));
    assert!(merge_recovery_gap(
        &mut gaps,
        OutputGap {
            first_missing_seq: 9,
            last_missing_seq: 10,
        },
        &recovered,
    ));

    assert_eq!(
        gaps,
        vec![
            OutputGap {
                first_missing_seq: 2,
                last_missing_seq: 5,
            },
            OutputGap {
                first_missing_seq: 9,
                last_missing_seq: 10,
            },
        ]
    );
}

#[test]
fn recovery_gap_rejects_recovered_output_and_preserves_existing_ranges() {
    let recovered = BTreeSet::from([4]);
    let mut gaps = vec![OutputGap {
        first_missing_seq: 8,
        last_missing_seq: 9,
    }];

    assert!(!merge_recovery_gap(
        &mut gaps,
        OutputGap {
            first_missing_seq: 3,
            last_missing_seq: 5,
        },
        &recovered,
    ));
    assert_eq!(
        gaps,
        vec![OutputGap {
            first_missing_seq: 8,
            last_missing_seq: 9,
        }]
    );
}

#[test]
fn recovery_detail_preserves_disjoint_ranges_without_widening_them() {
    let gaps = vec![
        OutputGap {
            first_missing_seq: 2,
            last_missing_seq: 4,
        },
        OutputGap {
            first_missing_seq: 8,
            last_missing_seq: 9,
        },
    ];
    let reasons = BTreeSet::from(["recovery page limit was exhausted".to_string()]);

    let detail = recovery_incomplete_detail(&gaps, &reasons);

    assert!(detail.contains("2-4, 8-9"));
    assert!(!detail.contains("2-9"));
    assert!(detail.contains("page limit"));
}
