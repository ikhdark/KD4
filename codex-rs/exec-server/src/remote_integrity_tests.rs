use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_utils_path_uri::PathUri;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::DuplexStream;
use tokio::io::Lines;
use tokio::time::Instant;
use tokio::time::timeout;

use crate::ExecParams;
use crate::ExecProcessEvent;
use crate::ExecServerClient;
use crate::ExecServerClientConnectOptions;
use crate::ExecServerError;
use crate::ProcessCleanupStatus;
use crate::ProcessExecutionKnowledge;
use crate::ProcessOutputChunk;
use crate::ReadResponse;
use crate::connection::JsonRpcConnection;

struct Peer {
    lines: Lines<BufReader<DuplexStream>>,
    writer: DuplexStream,
}

impl Peer {
    async fn next(&mut self) -> Option<Value> {
        timeout(Duration::from_secs(2), self.lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .map(|line| serde_json::from_str(&line).unwrap())
    }
    async fn send(&mut self, message: Value) {
        self.writer
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({"jsonrpc":"2.0", "id":request["id"], "result":result}))
            .await;
    }
}

fn pair() -> (JsonRpcConnection, Peer) {
    let (client_stdin, server_stdin) = tokio::io::duplex(1 << 20);
    let (server_stdout, client_stdout) = tokio::io::duplex(1 << 20);
    (
        JsonRpcConnection::from_stdio(client_stdout, client_stdin, "integrity-test".into()),
        Peer {
            lines: BufReader::new(server_stdin).lines(),
            writer: server_stdout,
        },
    )
}

async fn connected() -> (ExecServerClient, Peer) {
    let (connection, mut peer) = pair();
    let connecting = tokio::spawn(ExecServerClient::connect(
        connection,
        ExecServerClientConnectOptions::default(),
    ));
    let request = peer.next().await.unwrap();
    peer.reply(&request, json!({"sessionId":"integrity-session"}))
        .await;
    assert_eq!(peer.next().await.unwrap()["method"], "initialized");
    (connecting.await.unwrap().unwrap(), peer)
}

fn params() -> ExecParams {
    ExecParams {
        process_id: "integrity-process".into(),
        argv: vec!["unused-by-peer".into()],
        cwd: PathUri::from_host_native_path(std::env::current_dir().unwrap()).unwrap(),
        env_policy: None,
        env: HashMap::new(),
        tty: false,
        pipe_stdin: false,
        arg0: None,
        sandbox: None,
        enforce_managed_network: false,
        managed_network: None,
    }
}

#[tokio::test]
async fn native_lossy_name_cannot_become_a_complete_neighbor_inventory() {
    use crate::ExecutorFileSystem;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let temp = tempfile::tempdir().unwrap();
    let raw = OsString::from_wide(&[0x61, 0xd800, 0x2e, 0x74, 0x78, 0x74]);
    std::fs::write(temp.path().join(&raw), b"raw").unwrap();
    std::fs::write(temp.path().join("a\u{fffd}.txt"), b"neighbor").unwrap();
    assert_eq!(std::fs::read(temp.path().join(&raw)).unwrap(), b"raw");
    let fs = crate::LocalFileSystem::default();
    let root = PathUri::from_host_native_path(temp.path()).unwrap();
    let raw_uri = PathUri::from_host_native_path(temp.path().join(&raw)).unwrap();
    let neighbor_uri = root.join("a\u{fffd}.txt").unwrap();
    assert!(fs.read_directory(&root, None).await.is_err());
    let walk = fs
        .walk(
            &root,
            crate::WalkOptions {
                max_depth: 0,
                max_directories: 1,
                max_entries: 10,
                follow_directory_symlinks: false,
                prune_hidden_directories: false,
                filters: Default::default(),
            },
            None,
        )
        .await
        .unwrap();
    // Native walks can carry an exact opaque identity, unlike string-only
    // read_directory. Completeness is valid only if both identities survive.
    assert!(walk.errors.is_empty());
    assert_eq!(walk.entries.len(), 2);
    assert_ne!(raw_uri, neighbor_uri);
    assert!(walk.entries.iter().any(|entry| entry.path == raw_uri));
    assert!(walk.entries.iter().any(|entry| entry.path == neighbor_uri));
    assert_eq!(
        std::fs::read(raw_uri.to_abs_path().unwrap().as_path()).unwrap(),
        b"raw"
    );
    assert_eq!(
        std::fs::read(neighbor_uri.to_abs_path().unwrap().as_path()).unwrap(),
        b"neighbor"
    );
}

#[tokio::test]
async fn fresh_explicit_resume_rejects_a_replacement_before_acknowledging() {
    let (connection, mut peer) = pair();
    let connecting = tokio::spawn(ExecServerClient::connect(
        connection,
        ExecServerClientConnectOptions {
            resume_session_id: Some("session-old".into()),
            ..Default::default()
        },
    ));
    let request = peer.next().await.unwrap();
    assert_eq!(request["params"]["resumeSessionId"], "session-old");
    peer.reply(&request, json!({"sessionId":"session-new"}))
        .await;
    assert!(matches!(
        connecting.await.unwrap(),
        Err(ExecServerError::Protocol(_))
    ));
    // EOF or a still-open idle reader are both valid; `initialized` is not.
    if let Ok(Ok(Some(line))) = timeout(Duration::from_millis(50), peer.lines.next_line()).await {
        panic!("mismatched session was acknowledged: {line}");
    }
}

#[tokio::test]
async fn refused_close_discards_the_exact_owning_transport() {
    let (client, mut peer) = connected().await;
    let close_client = client.clone();
    let closing = tokio::spawn(async move {
        close_client
            .fs_close(crate::FsCloseParams {
                handle_id: "file-id".into(),
            })
            .await
    });
    let request = peer.next().await.unwrap();
    assert_eq!(request["method"], "fs/close");
    peer.send(json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":-32600,"message":"overloaded"}})).await;
    assert!(closing.await.unwrap().is_err());
    assert!(
        peer.next().await.is_none(),
        "custody must not be released on a live refused-close connection"
    );
}

#[tokio::test]
async fn expired_start_is_definitely_unsent_and_does_not_send_cleanup() {
    let (client, mut peer) = connected().await;
    let error = match client
        .start_process_before(params(), Instant::now() - Duration::from_millis(1))
        .await
    {
        Ok(_) => panic!("expired start succeeded"),
        Err(error) => error,
    };
    let ExecServerError::ProcessStart { ticket, .. } = error else {
        panic!("missing start evidence")
    };
    assert_eq!(
        ticket.snapshot().execution,
        ProcessExecutionKnowledge::DefinitelyNotSubmitted
    );
    assert_eq!(ticket.snapshot().cleanup, ProcessCleanupStatus::NotRequired);
    assert!(
        timeout(Duration::from_millis(25), peer.lines.next_line())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn ambiguous_legacy_start_refusal_never_terminates_a_preexisting_id() {
    let (client, mut peer) = connected().await;
    let start_client = client.clone();
    let starting = tokio::spawn(async move {
        start_client
            .start_process_before(params(), Instant::now() + Duration::from_secs(2))
            .await
    });
    let request = peer.next().await.unwrap();
    peer.send(
        json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32600,"message":"refused"}}),
    )
    .await;
    let error = match starting.await.unwrap() {
        Ok(_) => panic!("refused start succeeded"),
        Err(error) => error,
    };
    let ExecServerError::ProcessStart { ticket, .. } = error else {
        panic!("missing start evidence")
    };
    assert_eq!(
        ticket.snapshot().execution,
        ProcessExecutionKnowledge::PossiblyExecuted
    );
    assert!(matches!(
        ticket.snapshot().cleanup,
        ProcessCleanupStatus::Unconfirmed(_)
    ));
    assert!(
        timeout(Duration::from_millis(25), peer.lines.next_line())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn lost_start_reply_retains_identity_and_known_exit_without_replaying_start() {
    let (client, mut peer) = connected().await;
    let start_client = client.clone();
    let starting = tokio::spawn(async move {
        start_client
            .start_process_before(params(), Instant::now() + Duration::from_secs(2))
            .await
    });
    let request = peer.next().await.unwrap();
    assert_eq!(request["method"], "process/startOwned");
    peer.send(json!({"jsonrpc":"2.0","method":"process/exited","params":{"processId":"integrity-process","seq":1,"exitCode":7,"sandboxDenied":false}})).await;
    drop(peer);
    let error = match starting.await.unwrap() {
        Ok(_) => panic!("lost reply succeeded"),
        Err(error) => error,
    };
    let ExecServerError::ProcessStart { ticket, .. } = error else {
        panic!("missing start knowledge")
    };
    assert_eq!(ticket.snapshot().process_id.as_str(), "integrity-process");
    assert_eq!(
        ticket.snapshot().session_id.as_deref(),
        Some("integrity-session")
    );
    assert_eq!(
        ticket.snapshot().execution,
        ProcessExecutionKnowledge::ObservedExit(7)
    );
    timeout(Duration::from_secs(1), async {
        while matches!(ticket.snapshot().cleanup, ProcessCleanupStatus::Pending) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        ticket.snapshot().cleanup,
        ProcessCleanupStatus::Unconfirmed(_)
    ));
    assert_eq!(client.process_cleanup_records().0.len(), 1);
}

fn retained(chunks: Vec<ProcessOutputChunk>) -> ReadResponse {
    ReadResponse {
        chunks,
        next_seq: 5,
        exited: true,
        exit_code: Some(7),
        closed: true,
        failure: None,
        output_gap: None,
        sandbox_denied: false,
    }
}

#[test]
fn shared_replay_preserves_the_exit_hole_and_rejects_ambiguous_output_loss() {
    let chunk = |seq, bytes: &[u8]| ProcessOutputChunk {
        seq,
        stream: crate::ExecOutputStream::Stdout,
        chunk: bytes.to_vec().into(),
    };
    let events = crate::replay_process_events(
        0,
        false,
        retained(vec![chunk(1, b"alpha\n"), chunk(3, b"omega\n")]),
        None,
    )
    .unwrap();
    assert_eq!(events.len(), 4);
    assert!(matches!(
        events[1],
        ExecProcessEvent::Exited {
            seq: 2,
            exit_code: 7,
            ..
        }
    ));
    assert!(matches!(events[3], ExecProcessEvent::Closed { seq: 4, .. }));
    let bytes: Vec<u8> = events
        .into_iter()
        .flat_map(|event| match event {
            ExecProcessEvent::Output(chunk) => chunk.chunk.into_inner(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(bytes, b"alpha\nomega\n");
    assert!(
        crate::replay_process_events(0, false, retained(vec![chunk(3, b"omega\n")]), None).is_err()
    );
    assert!(
        crate::replay_process_events(
            0,
            false,
            retained(vec![chunk(3, b"omega"), chunk(1, b"alpha")]),
            None
        )
        .is_err()
    );
}

#[test]
fn shared_replay_keeps_every_declared_gap_around_capture_loss_markers() {
    let chunk = |seq, bytes: &[u8]| ProcessOutputChunk {
        seq,
        stream: crate::ExecOutputStream::Stderr,
        chunk: bytes.to_vec().into(),
    };
    // Sequence 1 was evicted, 3 is the exit, 4 and 6 are stdout and stderr
    // capture-loss markers around late stderr output, and 7 is the close.
    let mut response = retained(vec![chunk(2, b"a"), chunk(5, b"b")]);
    response.next_seq = 8;
    response.output_gap = Some(crate::ProcessOutputGap {
        through_seq: 6,
        exit_seq: Some(3),
    });
    let events = crate::replay_process_events(0, false, response, None).unwrap();
    assert_eq!(events.len(), 7, "{events:?}");
    assert_eq!(events[0], ExecProcessEvent::OutputGap { through_seq: 1 });
    assert_eq!(events[3], ExecProcessEvent::OutputGap { through_seq: 4 });
    assert_eq!(events[5], ExecProcessEvent::OutputGap { through_seq: 6 });
    assert!(matches!(events[6], ExecProcessEvent::Closed { seq: 7, .. }));
}

struct CountingRead {
    bytes: &'static [u8],
    read: Arc<AtomicUsize>,
}
impl tokio::io::AsyncRead for CountingRead {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let count = buffer.remaining().min(self.bytes.len());
        buffer.put_slice(&self.bytes[..count]);
        self.bytes = &self.bytes[count..];
        self.read.fetch_add(count, Ordering::SeqCst);
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn capped_read_rejects_metadata_without_payload_io_and_bounds_growth() {
    for size in 0..=5 {
        let bytes = &b"abcde"[..size];
        let read = Arc::new(AtomicUsize::new(0));
        let result = crate::local_file_system::read_file_payload_capped(
            CountingRead {
                bytes,
                read: read.clone(),
            },
            size as u64,
            4,
        )
        .await;
        if size <= 4 {
            assert_eq!(result.unwrap(), bytes);
            assert_eq!(read.load(Ordering::SeqCst), size);
        } else {
            assert!(result.is_err());
            assert_eq!(read.load(Ordering::SeqCst), 0);
        }
    }
    let read = Arc::new(AtomicUsize::new(0));
    assert!(
        crate::local_file_system::read_file_payload_capped(
            CountingRead {
                bytes: b"abcdefghij",
                read: read.clone()
            },
            1,
            4
        )
        .await
        .is_err()
    );
    assert_eq!(
        read.load(Ordering::SeqCst),
        5,
        "growth must stop at cap+1, not the local read allowance"
    );
}
