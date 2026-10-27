use super::CHANNEL_CAPACITY;
use super::ConnectionOrigin;
use super::TransportEvent;
use super::enqueue_incoming_message;
use super::next_connection_id;
use crate::outgoing_message::OutgoingMessage;
use crate::outgoing_message::QueuedOutgoingMessage;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCRequest;
use serde::Deserialize;
use std::io::BufRead;
use std::io::ErrorKind;
use std::io::Read;
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

// Bound allocation before JSON parsing and request-queue admission can run.
const MAX_STDIN_LINE_BYTES: usize = 32 * 1024 * 1024;
const STDIN_READ_AHEAD: usize = 1;
const MAX_STDOUT_BATCH_MESSAGES: usize = 64;
const STDOUT_BATCH_TARGET_BYTES: usize = 64 * 1024;
const STDOUT_STREAM_CHUNK_BYTES: usize = 64 * 1024;

pub async fn start_stdio_connection(
    transport_event_tx: mpsc::Sender<TransportEvent>,
    stdio_handles: &mut Vec<JoinHandle<()>>,
    initialize_client_name_tx: oneshot::Sender<String>,
) -> IoResult<()> {
    start_stdio_connection_with_io(
        transport_event_tx,
        stdio_handles,
        initialize_client_name_tx,
        spawn_stdin_line_reader()?,
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

fn spawn_stdin_line_reader() -> IoResult<mpsc::Receiver<IoResult<String>>> {
    // Tokio's stdin reader uses an uncancellable blocking read that runtime shutdown waits for.
    // Keep that read on a detached OS thread so closing the async receiver lets this transport and
    // its runtime finish even when the client deliberately leaves stdin open.
    // One queued line plus the native reader's current line bounds staging to
    // two frames, rather than CHANNEL_CAPACITY potentially 32-MiB messages.
    let (line_tx, line_rx) = mpsc::channel(STDIN_READ_AHEAD);
    std::thread::Builder::new()
        .name("codex-app-server-stdin".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();
            read_stdin_lines(stdin.lock(), line_tx, MAX_STDIN_LINE_BYTES);
        })?;
    Ok(line_rx)
}

fn read_stdin_lines(
    mut stdin: impl BufRead,
    line_tx: mpsc::Sender<IoResult<String>>,
    max_line_bytes: usize,
) {
    loop {
        let mut bytes = Vec::new();
        // Read at most one byte beyond the cap, even if the peer never sends a newline.
        match stdin
            .by_ref()
            .take(max_line_bytes as u64 + 1)
            .read_until(b'\n', &mut bytes)
        {
            Ok(0) => break,
            Ok(length) if length > max_line_bytes => {
                let _ = line_tx.blocking_send(Err(std::io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("stdin message exceeds the {max_line_bytes}-byte line limit"),
                )));
                break;
            }
            Ok(_) => {
                while matches!(bytes.last(), Some(b'\n' | b'\r')) {
                    bytes.pop();
                }
                let line = String::from_utf8(bytes)
                    .map_err(|error| std::io::Error::new(ErrorKind::InvalidData, error));
                // Invalid UTF-8 is a malformed frame. Its newline was consumed, so the
                // next frame can still be handled without corrupting valid input.
                if line_tx.blocking_send(line).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = line_tx.blocking_send(Err(error));
                break;
            }
        }
    }
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
    let cancellation = CancellationToken::new();
    let (writer_tx, mut writer_rx) = mpsc::channel::<QueuedOutgoingMessage>(CHANNEL_CAPACITY);
    let writer_tx_for_reader = writer_tx.clone();
    transport_event_tx
        .send(TransportEvent::ConnectionOpened {
            connection_id,
            origin: ConnectionOrigin::Stdio,
            writer: writer_tx,
            disconnect_sender: Some(cancellation.clone()),
        })
        .await
        .map_err(|_| std::io::Error::new(ErrorKind::BrokenPipe, "processor unavailable"))?;

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
                    let message = match serde_json::from_str::<JSONRPCMessage>(&line) {
                        Ok(message) => message,
                        Err(err) => {
                            error!("Failed to deserialize JSONRPCMessage: {err}");
                            continue;
                        }
                    };
                    if initialize_client_name_tx.is_some()
                        && let Some(client_name) = stdio_initialize_client_name(&message)
                        && let Some(initialize_client_name_tx) = initialize_client_name_tx.take()
                    {
                        let _ = initialize_client_name_tx.send(client_name);
                    }
                    let forwarded = tokio::select! {
                        _ = cancellation_for_reader.cancelled() => break,
                        forwarded = enqueue_incoming_message(
                            &transport_event_tx_for_reader,
                            &writer_tx_for_reader,
                            connection_id,
                            message,
                        ) => forwarded,
                    };
                    if !forwarded {
                        break;
                    }
                }
                Some(Err(err)) => {
                    error!("Failed reading stdin: {err}");
                    if err.kind() != ErrorKind::InvalidData {
                        break;
                    }
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
        let mut batch = Vec::new();
        let mut pending_message = None;
        'writer: loop {
            let queued_message = if pending_message.is_some() {
                pending_message.take()
            } else {
                tokio::select! {
                    _ = cancellation_for_writer.cancelled() => break,
                    queued_message = writer_rx.recv() => queued_message,
                }
            };
            let Some(mut queued_message) = queued_message else {
                break;
            };
            if let Some(streamed_len) = queued_message
                .encoded_json
                .as_ref()
                .and_then(|encoded| encoded.streamed_len)
            {
                // Hold the exclusive admission credit through the physical
                // write/flush. Never mix a streamed frame into a buffered batch.
                let encoded = queued_message.encoded_json.take();
                batch = Vec::new();
                let write_result = tokio::select! {
                    _ = cancellation_for_writer.cancelled() => break 'writer,
                    result = write_streamed_message(
                        &mut stdout,
                        queued_message.message,
                        streamed_len,
                    ) => result,
                };
                if let Err(err) = write_result {
                    error!("Failed to stream outgoing message to stdout: {err}");
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
                drop(encoded);
                if let Some(write_complete_tx) = queued_message.write_complete_tx {
                    let _ = write_complete_tx.send(std::time::Instant::now());
                }
                continue;
            }
            batch.clear();
            let mut byte_credits = Vec::new();
            let mut write_complete_tx = None;
            for index in 0..MAX_STDOUT_BATCH_MESSAGES {
                let frame_start = batch.len();
                let encoded = if let Some(mut encoded) = queued_message.encoded_json.take() {
                    batch.extend_from_slice(&encoded.bytes);
                    byte_credits.extend(encoded.delivery_credit.take());
                    Ok(())
                } else {
                    serde_json::to_writer(&mut batch, &queued_message.message)
                };
                match encoded {
                    Ok(()) => {
                        batch.push(b'\n');
                        write_complete_tx = queued_message.write_complete_tx;
                    }
                    Err(err) => {
                        batch.truncate(frame_start);
                        error!("Failed to serialize OutgoingMessage: {err}");
                    }
                }
                // Only coalesce messages already admitted. A receipt is a flush
                // boundary: later traffic must not delay its acknowledgement.
                if write_complete_tx.is_some()
                    || batch.len() >= STDOUT_BATCH_TARGET_BYTES
                    || index + 1 == MAX_STDOUT_BATCH_MESSAGES
                {
                    break;
                }
                match writer_rx.try_recv() {
                    Ok(next)
                        if next
                            .encoded_json
                            .as_ref()
                            .is_some_and(|encoded| encoded.streamed_len.is_some()) =>
                    {
                        pending_message = Some(next);
                        break;
                    }
                    Ok(next) => queued_message = next,
                    Err(_) => break,
                }
            }
            if batch.is_empty() {
                continue;
            }
            let write_result = tokio::select! {
                _ = cancellation_for_writer.cancelled() => break 'writer,
                result = stdout.write_all(&batch) => result,
            };
            if let Err(err) = write_result {
                error!("Failed to write to stdout: {err}");
                break;
            }
            if write_complete_tx.is_some() {
                let flush_result = tokio::select! {
                    _ = cancellation_for_writer.cancelled() => break 'writer,
                    result = stdout.flush() => result,
                };
                if let Err(err) = flush_result {
                    error!("Failed to flush stdout: {err}");
                    break;
                }
            }
            drop(byte_credits);
            if let Some(write_complete_tx) = write_complete_tx {
                let _ = write_complete_tx.send(std::time::Instant::now());
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

async fn write_streamed_message<W: AsyncWrite + Unpin>(
    stdout: &mut W,
    message: OutgoingMessage,
    expected_len: usize,
) -> IoResult<()> {
    struct ChunkWriter {
        sender: mpsc::Sender<IoResult<Vec<u8>>>,
        buffer: Vec<u8>,
    }

    impl std::io::Write for ChunkWriter {
        fn write(&mut self, mut bytes: &[u8]) -> IoResult<usize> {
            let len = bytes.len();
            while !bytes.is_empty() {
                let count = bytes.len().min(STDOUT_STREAM_CHUNK_BYTES - self.buffer.len());
                self.buffer.extend_from_slice(&bytes[..count]);
                bytes = &bytes[count..];
                if self.buffer.len() == STDOUT_STREAM_CHUNK_BYTES {
                    self.flush()?;
                }
            }
            Ok(len)
        }

        fn flush(&mut self) -> IoResult<()> {
            if !self.buffer.is_empty() {
                let chunk = std::mem::replace(
                    &mut self.buffer,
                    Vec::with_capacity(STDOUT_STREAM_CHUNK_BYTES),
                );
                self.sender.blocking_send(Ok(chunk)).map_err(|_| {
                    std::io::Error::new(ErrorKind::BrokenPipe, "stdio streaming cancelled")
                })?;
            }
            Ok(())
        }
    }

    // serde's writer is synchronous. A single frame-owned native worker and
    // one queued chunk keep blocking serialization off the async runtime. When
    // cancellation drops the receiver, even a backpressured worker can exit;
    // it is not a Tokio blocking task that runtime shutdown must wait for.
    let (sender, mut chunks) = mpsc::channel(1);
    std::thread::Builder::new()
        .name("codex-app-server-json".to_string())
        .spawn(move || {
            let mut writer = ChunkWriter {
                sender,
                buffer: Vec::with_capacity(STDOUT_STREAM_CHUNK_BYTES),
            };
            let result = serde_json::to_writer(&mut writer, &message)
                .map_err(std::io::Error::other)
                .and_then(|()| std::io::Write::flush(&mut writer));
            if let Err(error) = result {
                let _ = writer.sender.blocking_send(Err(error));
            }
        })?;

    let mut remaining = expected_len;
    while let Some(chunk) = chunks.recv().await {
        let chunk = chunk?;
        if chunk.len() > remaining {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "streamed stdio message exceeded its validated length",
            ));
        }
        stdout.write_all(&chunk).await?;
        remaining -= chunk.len();
    }
    if remaining != 0 {
        return Err(std::io::Error::new(
            ErrorKind::UnexpectedEof,
            "streamed stdio message ended before its validated length",
        ));
    }
    stdout.write_all(b"\n").await
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

fn stdio_initialize_client_name(message: &JSONRPCMessage) -> Option<String> {
    let JSONRPCMessage::Request(JSONRPCRequest { method, params, .. }) = message else {
        return None;
    };
    if method != "initialize" {
        return None;
    }
    let params = InitializeParams::deserialize(params.as_ref()?).ok()?;
    Some(params.client_info.name)
}

