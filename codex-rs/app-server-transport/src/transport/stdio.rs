use super::CHANNEL_CAPACITY;
use super::ConnectionOrigin;
use super::TransportEvent;
use super::forward_incoming_message;
use super::next_connection_id;
use super::serialize_outgoing_message;
use crate::outgoing_message::QueuedOutgoingMessage;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCRequest;
use std::io::BufRead;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::error;
use tracing::info;

pub async fn start_stdio_connection(
    transport_event_tx: mpsc::Sender<TransportEvent>,
    stdio_handles: &mut Vec<JoinHandle<()>>,
    initialize_client_name_tx: oneshot::Sender<String>,
) -> IoResult<()> {
    start_stdio_connection_with_io(
        transport_event_tx,
        stdio_handles,
        initialize_client_name_tx,
        spawn_stdin_line_reader(),
        NativeStdout::new()?,
    )
    .await
}

enum NativeOutputOperation {
    Write(Vec<u8>),
    Flush,
}

struct PendingNativeOutput {
    completion: oneshot::Receiver<IoResult<()>>,
    length: usize,
    flush: bool,
}

/// One acknowledged native operation at a time, independent of Tokio's blocking pool.
/// Like the stdin thread, an OS write may outlive transport cancellation; process
/// exit must not wait for a peer that stopped reading its pipe.
struct NativeStdout {
    sender: std::sync::mpsc::SyncSender<(NativeOutputOperation, oneshot::Sender<IoResult<()>>)>,
    pending: Option<PendingNativeOutput>,
}

impl NativeStdout {
    fn new() -> IoResult<Self> {
        let (sender, operations) = std::sync::mpsc::sync_channel::<(
            NativeOutputOperation,
            oneshot::Sender<IoResult<()>>,
        )>(1);
        std::thread::Builder::new()
            .name("codex-app-server-stdout".to_string())
            .spawn(move || {
                use std::io::Write as _;
                let mut stdout = std::io::stdout();
                while let Ok((operation, completion)) = operations.recv() {
                    let result = match operation {
                        NativeOutputOperation::Write(bytes) => stdout.write_all(&bytes),
                        NativeOutputOperation::Flush => stdout.flush(),
                    };
                    let failed = result.is_err();
                    // Success is acknowledged only after the physical operation.
                    let _ = completion.send(result);
                    if failed {
                        break;
                    }
                }
            })?;
        Ok(Self {
            sender,
            pending: None,
        })
    }

    fn submit(
        &mut self,
        operation: NativeOutputOperation,
        length: usize,
        flush: bool,
    ) -> IoResult<()> {
        let (completion, receiver) = oneshot::channel();
        self.sender.try_send((operation, completion)).map_err(|_| {
            std::io::Error::new(ErrorKind::BrokenPipe, "native stdout worker unavailable")
        })?;
        self.pending = Some(PendingNativeOutput {
            completion: receiver,
            length,
            flush,
        });
        Ok(())
    }

    fn poll_pending(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<IoResult<(usize, bool)>> {
        let Some(pending) = self.pending.as_mut() else {
            unreachable!("native operation is pending")
        };
        let result = match std::pin::Pin::new(&mut pending.completion).poll(cx) {
            std::task::Poll::Pending => return std::task::Poll::Pending,
            std::task::Poll::Ready(result) => result,
        };
        let length = pending.length;
        let flush = pending.flush;
        self.pending = None;
        std::task::Poll::Ready(
            result
                .unwrap_or_else(|_| {
                    Err(std::io::Error::new(
                        ErrorKind::BrokenPipe,
                        "native stdout worker stopped",
                    ))
                })
                .map(|()| (length, flush)),
        )
    }
}

impl AsyncWrite for NativeStdout {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<IoResult<usize>> {
        loop {
            if self.pending.is_some() {
                match self.poll_pending(cx) {
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                    std::task::Poll::Ready(Err(error)) => {
                        return std::task::Poll::Ready(Err(error));
                    }
                    std::task::Poll::Ready(Ok((length, false))) => {
                        return std::task::Poll::Ready(Ok(length));
                    }
                    std::task::Poll::Ready(Ok((_, true))) => {}
                }
            }
            if bytes.is_empty() {
                return std::task::Poll::Ready(Ok(0));
            }
            if let Err(error) = self.submit(
                NativeOutputOperation::Write(bytes.to_vec()),
                bytes.len(),
                false,
            ) {
                return std::task::Poll::Ready(Err(error));
            }
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<IoResult<()>> {
        loop {
            if self.pending.is_some() {
                match self.poll_pending(cx) {
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                    std::task::Poll::Ready(Err(error)) => {
                        return std::task::Poll::Ready(Err(error));
                    }
                    std::task::Poll::Ready(Ok((_, true))) => return std::task::Poll::Ready(Ok(())),
                    std::task::Poll::Ready(Ok((_, false))) => {}
                }
            }
            if let Err(error) = self.submit(NativeOutputOperation::Flush, 0, true) {
                return std::task::Poll::Ready(Err(error));
            }
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<IoResult<()>> {
        self.poll_flush(cx)
    }
}

fn spawn_stdin_line_reader() -> mpsc::Receiver<IoResult<String>> {
    // Tokio's stdin reader uses an uncancellable blocking read that runtime shutdown waits for.
    // Keep that read on a detached OS thread so closing the async receiver lets this transport and
    // its runtime finish even when the client deliberately leaves stdin open.
    let (line_tx, line_rx) = mpsc::channel(CHANNEL_CAPACITY);
    if let Err(err) = std::thread::Builder::new()
        .name("codex-app-server-stdin".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut stdin = stdin.lock();
            loop {
                let mut line = String::new();
                match stdin.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        while matches!(line.as_bytes().last(), Some(b'\n' | b'\r')) {
                            line.pop();
                        }
                        if line_tx.blocking_send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = line_tx.blocking_send(Err(err));
                        break;
                    }
                }
            }
        })
    {
        error!("Failed to start stdin reader thread: {err}");
    }
    line_rx
}

async fn start_stdio_connection_with_io<W>(
    transport_event_tx: mpsc::Sender<TransportEvent>,
    stdio_handles: &mut Vec<JoinHandle<()>>,
    initialize_client_name_tx: oneshot::Sender<String>,
    mut stdin_lines: mpsc::Receiver<IoResult<String>>,
    mut stdout: W,
) -> IoResult<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let connection_id = next_connection_id();
    let (writer_tx, mut writer_rx) = mpsc::channel::<QueuedOutgoingMessage>(CHANNEL_CAPACITY);
    let writer_tx_for_reader = writer_tx.clone();
    transport_event_tx
        .send(TransportEvent::ConnectionOpened {
            connection_id,
            origin: ConnectionOrigin::Stdio,
            writer: writer_tx,
            disconnect_sender: None,
        })
        .await
        .map_err(|_| std::io::Error::new(ErrorKind::BrokenPipe, "processor unavailable"))?;

    let cancellation = CancellationToken::new();
    let connection_closed = Arc::new(AtomicBool::new(false));
    let transport_event_tx_for_reader = transport_event_tx.clone();
    let cancellation_for_reader = cancellation.clone();
    let connection_closed_for_reader = Arc::clone(&connection_closed);
    stdio_handles.push(tokio::spawn(async move {
        let mut initialize_client_name_tx = Some(initialize_client_name_tx);

        loop {
            let line = tokio::select! {
                _ = cancellation_for_reader.cancelled() => break,
                line = stdin_lines.recv() => line,
            };
            match line {
                Some(Ok(line)) => {
                    if let Some(client_name) = stdio_initialize_client_name(&line)
                        && let Some(initialize_client_name_tx) = initialize_client_name_tx.take()
                    {
                        let _ = initialize_client_name_tx.send(client_name);
                    }
                    let forwarded = tokio::select! {
                        _ = cancellation_for_reader.cancelled() => break,
                        forwarded = forward_incoming_message(
                            &transport_event_tx_for_reader,
                            &writer_tx_for_reader,
                            connection_id,
                            &line,
                        ) => forwarded,
                    };
                    if !forwarded {
                        break;
                    }
                }
                Some(Err(err)) => {
                    error!("Failed reading stdin: {err}");
                    break;
                }
                None => break,
            }
        }

        // Either half may win close publication. Release native input admission
        // before that publication can wait behind an already admitted message.
        drop(stdin_lines);
        close_stdio_connection(
            &transport_event_tx_for_reader,
            connection_id,
            &cancellation_for_reader,
            &connection_closed_for_reader,
        )
        .await;
        debug!("stdin reader finished (EOF)");
    }));

    let cancellation_for_writer = cancellation;
    let connection_closed_for_writer = Arc::clone(&connection_closed);
    stdio_handles.push(tokio::spawn(async move {
        'writer: loop {
            let queued_message = tokio::select! {
                _ = cancellation_for_writer.cancelled() => break,
                queued_message = writer_rx.recv() => queued_message,
            };
            let Some(queued_message) = queued_message else {
                break;
            };
            let Some(mut json) = serialize_outgoing_message(queued_message.message) else {
                continue;
            };
            json.push('\n');
            let write_result = tokio::select! {
                _ = cancellation_for_writer.cancelled() => break 'writer,
                result = stdout.write_all(json.as_bytes()) => result,
            };
            if let Err(err) = write_result {
                error!("Failed to write to stdout: {err}");
                break;
            }
            if queued_message.write_complete_tx.is_some() {
                let flush_result = tokio::select! {
                    _ = cancellation_for_writer.cancelled() => break 'writer,
                    result = stdout.flush() => result,
                };
                if let Err(err) = flush_result {
                    error!("Failed to flush stdout: {err}");
                    break;
                }
            }
            if let Some(write_complete_tx) = queued_message.write_complete_tx {
                let _ = write_complete_tx.send(());
            }
        }
        // Release blocked router sends before close publication waits on ingress.
        // The processor may itself be waiting for that router to consume output.
        drop(writer_rx);
        close_stdio_connection(
            &transport_event_tx,
            connection_id,
            &cancellation_for_writer,
            &connection_closed_for_writer,
        )
        .await;
        info!("stdout writer exited (channel closed)");
    }));

    Ok(())
}

async fn close_stdio_connection(
    transport_event_tx: &mpsc::Sender<TransportEvent>,
    connection_id: crate::outgoing_message::ConnectionId,
    cancellation: &CancellationToken,
    connection_closed: &AtomicBool,
) {
    cancellation.cancel();
    if !connection_closed.swap(true, Ordering::AcqRel) {
        let _ = transport_event_tx
            .send(TransportEvent::ConnectionClosed { connection_id })
            .await;
    }
}

fn stdio_initialize_client_name(line: &str) -> Option<String> {
    let message = serde_json::from_str::<JSONRPCMessage>(line).ok()?;
    let JSONRPCMessage::Request(JSONRPCRequest { method, params, .. }) = message else {
        return None;
    };
    if method != "initialize" {
        return None;
    }
    let params = serde_json::from_value::<InitializeParams>(params?).ok()?;
    Some(params.client_info.name)
}

#[cfg(test)]
mod tests {
    use codex_app_server_protocol::RequestId;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::time::Duration;
    use tokio::time::timeout;

    use super::*;
    use crate::outgoing_message::OutgoingMessage;
    use crate::outgoing_message::OutgoingResponse;

    #[test]
    fn native_stdio_eof_releases_runtime_with_unread_stdout() {
        const CHILD_ENV: &str = "CODEX_STDIO_EOF_RUNTIME_TEST_CHILD";
        const ACK_ENV: &str = "CODEX_STDIO_EOF_ACK_PATH";
        if let Some(mode) = std::env::var_os(CHILD_ENV) {
            let healthy = mode == "healthy";
            let ack_path = std::path::PathBuf::from(std::env::var_os(ACK_ENV).unwrap());
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let (events_tx, mut events_rx) = mpsc::channel(CHANNEL_CAPACITY);
                let (initialize_tx, _initialize_rx) = oneshot::channel();
                let mut handles = Vec::new();
                start_stdio_connection(events_tx, &mut handles, initialize_tx)
                    .await
                    .unwrap();
                let mut output = None;
                let mut admitted = 0;
                let mut abandoned_completion = None;
                while let Some(event) = events_rx.recv().await {
                    match event {
                        TransportEvent::ConnectionOpened { writer, .. } => output = Some(writer),
                        TransportEvent::IncomingMessage {
                            message: JSONRPCMessage::Request(request),
                            ..
                        } => {
                            admitted += 1;
                            assert_eq!(request.method, "native-output-probe");
                            let (write_complete_tx, write_complete_rx) = oneshot::channel();
                            output
                                .as_ref()
                                .unwrap()
                                .send(QueuedOutgoingMessage {
                                    message: OutgoingMessage::Response(OutgoingResponse {
                                        id: request.id,
                                        result: json!({"payload": "x".repeat(2 * 1024 * 1024)}),
                                    }),
                                    write_complete_tx: Some(write_complete_tx),
                                })
                                .await
                                .unwrap();
                            if healthy {
                                write_complete_rx
                                    .await
                                    .expect("physical native write and flush must be acknowledged");
                                std::fs::write(&ack_path, b"physical write and flush completed")
                                    .unwrap();
                            } else {
                                abandoned_completion = Some(write_complete_rx);
                            }
                        }
                        TransportEvent::ConnectionClosed { .. } => break,
                        event => panic!("unexpected native stdio event: {event:?}"),
                    }
                }
                assert_eq!(admitted, 1);
                drop(output);
                for handle in handles {
                    timeout(Duration::from_secs(2), handle)
                        .await
                        .unwrap()
                        .unwrap();
                }
                if let Some(completion) = abandoned_completion {
                    assert!(
                        completion.await.is_err(),
                        "canceled native write must never report write-complete success"
                    );
                }
            });
            drop(runtime);
            // Avoid writing libtest's success banner to the intentionally blocked
            // stdout after the behavior under test has completed.
            std::process::exit(0);
        }
        use std::io::Read as _;
        use std::io::Write as _;
        use std::process::Command;
        use std::process::Stdio;
        use std::time::Instant;

        for healthy in [false, true] {
            let witness_dir = tempfile::tempdir().unwrap();
            let ack_path = witness_dir.path().join("native-write-complete");
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "transport::stdio::tests::native_stdio_eof_releases_runtime_with_unread_stdout",
                    "--nocapture",
                ])
                .env(CHILD_ENV, if healthy { "healthy" } else { "blocked" })
                .env(ACK_ENV, &ack_path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000);
            }
            let mut child = command.spawn().unwrap();
            let mut input = child.stdin.take().unwrap();
            input
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"native-output-probe\"}\n")
                .unwrap();
            input.flush().unwrap();
            let output = child.stdout.take().unwrap();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                let mut output = output;
                let mut byte = [0];
                let mut started = false;
                for _ in 0..4096 {
                    if output.read_exact(&mut byte).is_err() {
                        break;
                    }
                    if byte[0] == b'{' {
                        started = true;
                        break;
                    }
                }
                let _ = started_tx.send(started);
                if healthy && started {
                    use std::io::BufRead as _;
                    let mut response = vec![b'{'];
                    let mut buffered = std::io::BufReader::new(&mut output);
                    buffered.read_until(b'\n', &mut response).unwrap();
                    let response: serde_json::Value = serde_json::from_slice(&response).unwrap();
                    assert_eq!(response["id"], 1);
                    assert_eq!(response["result"]["payload"], "x".repeat(2 * 1024 * 1024));
                }
                let _ = release_rx.recv();
                drop(output);
            });
            let started = started_rx.recv_timeout(std::time::Duration::from_secs(5));
            if !matches!(started, Ok(true)) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = release_tx.send(());
                reader.join().unwrap();
                panic!("native response never entered stdout: {started:?}");
            }
            // The response is much larger than the OS pipe. Withhold the remaining
            // bytes while EOF is delivered independently through stdin.
            let mut acknowledged = false;
            if healthy {
                let ack_deadline = Instant::now() + std::time::Duration::from_secs(5);
                while Instant::now() < ack_deadline {
                    if ack_path.exists() {
                        acknowledged = true;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            } else {
                assert!(
                    !ack_path.exists(),
                    "blocked native write cannot be acknowledged"
                );
            }
            drop(input);
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break Some(status);
                }
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            if status.is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = release_tx.send(());
            reader.join().unwrap();
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            assert!(
                status.is_some_and(|status| status.success()),
                "observed native EOF must release the runtime with stdout unread; status={status:?}, stderr={stderr}"
            );
            assert_eq!(
                acknowledged, healthy,
                "healthy physical write must be acknowledged before EOF"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdout_failure_releases_full_writer_before_close_can_enter_ingress() {
        use std::pin::Pin;
        use std::task::Context;
        use std::task::Poll;
        use tokio::sync::Notify;

        struct GatedFailure {
            entered: Arc<Notify>,
            fail: oneshot::Receiver<()>,
        }
        impl AsyncWrite for GatedFailure {
            fn poll_write(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                _buf: &[u8],
            ) -> Poll<IoResult<usize>> {
                self.entered.notify_one();
                match Pin::new(&mut self.fail).poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(_) => Poll::Ready(Err(std::io::Error::new(
                        ErrorKind::BrokenPipe,
                        "external stdout closed",
                    ))),
                }
            }
            fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<IoResult<()>> {
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<IoResult<()>> {
                Poll::Ready(Ok(()))
            }
        }

        for writer_fails in [true, false] {
            let (transport_tx, mut transport_rx) = mpsc::channel(1);
            let (stdin_tx, stdin_rx) = mpsc::channel(1);
            let (initialize_tx, _initialize_rx) = oneshot::channel();
            let entered = Arc::new(Notify::new());
            let (fail_tx, fail_rx) = oneshot::channel();
            let mut handles = Vec::new();
            start_stdio_connection_with_io(
                transport_tx.clone(),
                &mut handles,
                initialize_tx,
                stdin_rx,
                GatedFailure {
                    entered: entered.clone(),
                    fail: fail_rx,
                },
            )
            .await
            .unwrap();
            let (connection_id, writer) = match transport_rx.recv().await.unwrap() {
                TransportEvent::ConnectionOpened {
                    connection_id,
                    writer,
                    ..
                } => (connection_id, writer),
                event => panic!("unexpected open event: {event:?}"),
            };
            let response = || {
                QueuedOutgoingMessage::new(OutgoingMessage::Response(OutgoingResponse {
                    id: RequestId::Integer(1),
                    result: json!({"ok": true}),
                }))
            };
            writer.send(response()).await.unwrap();
            timeout(Duration::from_secs(1), entered.notified())
                .await
                .unwrap();
            for _ in 0..CHANNEL_CAPACITY {
                writer.try_send(response()).unwrap();
            }
            let waiting_send = writer.send(response());
            tokio::pin!(waiting_send);
            assert!(futures::poll!(&mut waiting_send).is_pending());

            // Both messages use the real parser/ingress path. The first is admitted;
            // the second blocks behind it and remains cancellable on writer failure.
            stdin_tx
                .send(Ok(r#"{"jsonrpc":"2.0","method":"admitted"}"#.into()))
                .await
                .unwrap();
            timeout(Duration::from_secs(1), async {
                while transport_tx.capacity() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let mut stdin_sender = Some(stdin_tx);
            if writer_fails {
                stdin_sender
                    .as_ref()
                    .unwrap()
                    .send(Ok(r#"{"jsonrpc":"2.0","method":"pending"}"#.into()))
                    .await
                    .unwrap();
                let permit = timeout(
                    Duration::from_secs(1),
                    stdin_sender.as_ref().unwrap().reserve(),
                )
                .await
                .unwrap()
                .unwrap();
                drop(permit);
                fail_tx.send(()).unwrap();
            } else {
                drop(stdin_sender.take());
            }
            assert!(
                timeout(Duration::from_secs(1), &mut waiting_send)
                    .await
                    .expect("failed writer must release router before ingress is drained")
                    .is_err()
            );
            if let Some(stdin_sender) = stdin_sender {
                timeout(Duration::from_secs(1), stdin_sender.closed())
                    .await
                    .expect("writer failure must cancel the blocked input owner");
            }
            match transport_rx.recv().await.unwrap() {
                TransportEvent::IncomingMessage {
                    connection_id: id,
                    message: JSONRPCMessage::Notification(message),
                } => {
                    assert_eq!(id, connection_id);
                    assert_eq!(message.method, "admitted");
                }
                event => panic!("admitted input must be retained: {event:?}"),
            }
            match timeout(Duration::from_secs(1), transport_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                TransportEvent::ConnectionClosed { connection_id: id } => {
                    assert_eq!(id, connection_id)
                }
                event => panic!("unexpected close event: {event:?}"),
            }
            for handle in handles {
                timeout(Duration::from_secs(1), handle)
                    .await
                    .unwrap()
                    .unwrap();
            }
            drop(transport_tx);
            assert!(
                transport_rx.recv().await.is_none(),
                "one close, no unadmitted notification"
            );
        }
    }

    #[tokio::test]
    async fn stdout_failure_closes_connection_while_stdin_remains_open() {
        let (transport_event_tx, mut transport_event_rx) = mpsc::channel(8);
        let mut stdio_handles = Vec::new();
        let (initialize_client_name_tx, _initialize_client_name_rx) = oneshot::channel();
        let (_stdin_line_tx, stdin_lines) = mpsc::channel::<IoResult<String>>(1);
        let (stdout_reader, stdout_writer) = tokio::io::duplex(64);
        drop(stdout_reader);

        start_stdio_connection_with_io(
            transport_event_tx,
            &mut stdio_handles,
            initialize_client_name_tx,
            stdin_lines,
            stdout_writer,
        )
        .await
        .expect("stdio connection should start");

        let (connection_id, writer) = match transport_event_rx
            .recv()
            .await
            .expect("connection should open")
        {
            TransportEvent::ConnectionOpened {
                connection_id,
                writer,
                ..
            } => (connection_id, writer),
            event => panic!("expected connection-opened event, got {event:?}"),
        };
        writer
            .send(QueuedOutgoingMessage::new(OutgoingMessage::Response(
                OutgoingResponse {
                    id: RequestId::Integer(1),
                    result: json!({"ok": true}),
                },
            )))
            .await
            .expect("writer queue should be open");

        let closed_connection_id = match timeout(Duration::from_secs(1), transport_event_rx.recv())
            .await
            .expect("stdout failure should close the connection")
            .expect("transport event channel should remain open")
        {
            TransportEvent::ConnectionClosed { connection_id } => connection_id,
            event => panic!("expected connection-closed event, got {event:?}"),
        };
        assert_eq!(closed_connection_id, connection_id);

        for handle in stdio_handles {
            timeout(Duration::from_secs(1), handle)
                .await
                .expect("both stdio halves should terminate")
                .expect("stdio task should not panic");
        }
        assert!(
            timeout(Duration::from_secs(1), transport_event_rx.recv())
                .await
                .expect("transport should close after both stdio tasks finish")
                .is_none(),
            "shutdown must emit exactly one connection-closed event"
        );
    }
}
