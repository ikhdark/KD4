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
use tokio::io;
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
        io::stdout(),
    )
    .await
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
        assert!(matches!(
            transport_event_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}
