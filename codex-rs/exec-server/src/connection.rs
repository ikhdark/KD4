use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::ws::Message as AxumWebSocketMessage;
use axum::extract::ws::WebSocket as AxumWebSocket;
use codex_exec_server_protocol::JSONRPCMessage;
use codex_utils_pty::ManagedRootProcess;
use codex_utils_pty::WINDOWS_PROCESS_OPERATION_TIMEOUT;
use codex_utils_pty::run_windows_process_operation;
use futures::Sink;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::debug;
use tracing::warn;

use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::BufWriter;

pub(crate) const CHANNEL_CAPACITY: usize = 128;
// Small, finite control-read headroom while application delivery is blocked.
// A receiver that stalls beyond this backlog fails via the same liveness bound.
pub(crate) const WEBSOCKET_CONTROL_BACKLOG: usize = 8;
/// Largest serialized JSON-RPC message any exec-server transport accepts.
///
/// Stdio and WebSocket enforce it per message and Noise enforces it before
/// splitting a message into records, so a payload limit does not depend on
/// which transport carries the connection.
pub(crate) const MAX_JSONRPC_MESSAGE_LEN: usize = 64 * 1024 * 1024;
/// Room kept for the envelope and non-payload fields such as paths and sandbox policy.
const JSONRPC_PAYLOAD_RESERVE_LEN: usize = 1024 * 1024;
/// Largest raw file payload whose base64 encoding fits in one JSON-RPC message.
///
/// An oversized message is rejected by the receiving transport, which closes the
/// whole connection; file operations check this bound so they fail on their own.
pub(crate) const MAX_FILE_PAYLOAD_BYTES: usize =
    (MAX_JSONRPC_MESSAGE_LEN - JSONRPC_PAYLOAD_RESERVE_LEN) / 4 * 3;
const STDIO_TERMINATION_GRACE_PERIOD: Duration = Duration::from_secs(2);
const STDIO_FORCE_REAP_TIMEOUT: Duration = Duration::from_secs(5);
const STDIO_SUPERVISOR_TIMEOUT: Duration = Duration::from_secs(70);
pub(crate) const WEBSOCKET_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

// Whole-message policy, not a process/RPC timeout. Unlike 60 KiB Noise
// records, legacy peers may place a Pong behind one 64 MiB frame. Allow
// that frame 64 KiB/s plus 60 seconds of slack in either direction. A
// continuously slower link must use a smaller message/streamed file read.
pub(crate) const WEBSOCKET_MESSAGE_TIMEOUT: Duration =
    Duration::from_secs(60 + (MAX_JSONRPC_MESSAGE_LEN / (64 * 1024)) as u64);

/// Reader limits for WebSockets that carry one JSON-RPC message per frame.
///
/// Tungstenite writes each message as a single frame, so its default 16 MiB
/// frame limit would reject messages that every other transport accepts.
pub(crate) fn jsonrpc_websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_frame_size(Some(MAX_JSONRPC_MESSAGE_LEN))
        .max_message_size(Some(MAX_JSONRPC_MESSAGE_LEN))
}

#[derive(Debug)]
pub(crate) enum JsonRpcConnectionEvent {
    Message(JSONRPCMessage),
    MalformedMessage { reason: String },
    Disconnected { reason: Option<String> },
}

#[derive(Clone)]
pub(crate) enum JsonRpcTransport {
    // Plain means no child process; transport bytes may still be encrypted.
    Plain,
    Stdio { transport: StdioTransport },
}

impl JsonRpcTransport {

    fn from_managed_child_process(
        child_process: Child,
        managed_root: Arc<ManagedRootProcess>,
    ) -> Self {
        Self::Stdio {
            transport: StdioTransport::spawn(child_process, Some(managed_root)),
        }
    }

    pub(crate) fn terminate(&self) {
        match self {
            Self::Plain => {}
            Self::Stdio { transport } => transport.terminate(),
        }
    }

    pub(crate) async fn terminate_and_wait(&self) -> bool {
        match self {
            Self::Plain => true,
            Self::Stdio { transport } => transport.terminate_and_wait().await,
        }
    }


}

#[derive(Clone)]
pub(crate) struct StdioTransport {
    handle: Arc<StdioTransportHandle>,
}

struct StdioTransportHandle {
    terminate_tx: watch::Sender<bool>,
    terminated_rx: watch::Receiver<bool>,
    terminate_requested: AtomicBool,
}

impl StdioTransport {
    fn spawn(child_process: Child, managed_root: Option<Arc<ManagedRootProcess>>) -> Self {
        let (terminate_tx, terminate_rx) = watch::channel(false);
        let (terminated_tx, terminated_rx) = watch::channel(false);
        let handle = Arc::new(StdioTransportHandle {
            terminate_tx,
            terminated_rx,
            terminate_requested: AtomicBool::new(false),
        });
        spawn_stdio_child_supervisor(child_process, managed_root, terminate_rx, terminated_tx);
        Self { handle }
    }

    fn terminate(&self) {
        self.handle.terminate();
    }

    async fn terminate_and_wait(&self) -> bool {
        self.terminate_and_wait_with_timeout(STDIO_SUPERVISOR_TIMEOUT)
            .await
    }

    async fn terminate_and_wait_with_timeout(&self, wait_timeout: Duration) -> bool {
        self.terminate();
        let mut terminated_rx = self.handle.terminated_rx.clone();
        let completed = timeout(wait_timeout, async {
            loop {
                if *terminated_rx.borrow() {
                    return true;
                }
                if terminated_rx.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false);
        if !completed {
            warn!("exec-server stdio child supervisor exceeded its termination deadline");
        }
        completed
    }

}

impl StdioTransportHandle {
    fn terminate(&self) {
        if !self.terminate_requested.swap(true, Ordering::AcqRel) {
            let _ = self.terminate_tx.send(true);
        }
    }
}

impl Drop for StdioTransportHandle {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn spawn_stdio_child_supervisor(
    mut child_process: Child,
    managed_root: Option<Arc<ManagedRootProcess>>,
    mut terminate_rx: watch::Receiver<bool>,
    terminated_tx: watch::Sender<bool>,
) {
    let process_group_id = child_process.id();
    tokio::spawn(async move {
        tokio::select! {
            result = child_process.wait() => {
                match result {
                    Ok(_) => {
                        // wait() released the root handle: its numeric PID is no
                        // longer a safe cleanup identity. The Job still owns descendants.
                        if let Some(root) = managed_root.as_ref()
                            && let Err(err) = root.terminate()
                        {
                            debug!("failed to terminate exec-server stdio descendants: {err}");
                        }
                    }
                    Err(err) => {
                        debug!("failed to wait for exec-server stdio child: {err}");
                        terminate_stdio_child(&mut child_process, managed_root.as_deref(), process_group_id).await;
                    }
                }
            }
            () = wait_for_stdio_termination(&mut terminate_rx) => {
                terminate_stdio_child(&mut child_process, managed_root.as_deref(), process_group_id).await;
            }
        }
        drop(managed_root);
        terminated_tx.send_replace(true);
    });
}

async fn wait_for_stdio_termination(terminate_rx: &mut watch::Receiver<bool>) {
    loop {
        if *terminate_rx.borrow() {
            return;
        }
        if terminate_rx.changed().await.is_err() {
            return;
        }
    }
}

async fn terminate_stdio_child(
    child_process: &mut Child,
    managed_root: Option<&ManagedRootProcess>,
    process_group_id: Option<u32>,
) {
    let mut wait_timeout = STDIO_TERMINATION_GRACE_PERIOD;
    loop {
        if let Some(root) = managed_root {
            if let Err(err) = root.terminate() {
                debug!("failed to terminate managed exec-server stdio process tree: {err}");
                kill_direct_child(child_process, "kill");
            }
        } else {
            kill_process_tree(child_process, process_group_id).await;
        }
        match timeout(wait_timeout, child_process.wait()).await {
            Ok(Ok(_)) => return,
            Ok(Err(err)) => {
                debug!("failed to wait for exec-server stdio child; retaining admission: {err}");
                if matches!(child_process.try_wait(), Ok(Some(_))) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(_) => warn!("exec-server stdio child exit remains unconfirmed; retaining admission"),
        }
        wait_timeout = STDIO_FORCE_REAP_TIMEOUT;
    }
}

async fn kill_process_tree(child_process: &mut Child, process_group_id: Option<u32>) {
    let Some(process_group_id) = process_group_id else {
        kill_direct_child(child_process, "terminate");
        return;
    };

    if !kill_windows_process_tree(process_group_id).await {
        kill_direct_child(child_process, "terminate");
    }
}

fn kill_direct_child(child_process: &mut Child, action: &str) {
    if let Err(err) = child_process.start_kill() {
        debug!("failed to {action} exec-server stdio child: {err}");
    }
}

async fn kill_windows_process_tree(pid: u32) -> bool {
    let pid = pid.to_string();
    let pid_for_task = pid.clone();
    match run_windows_process_operation(WINDOWS_PROCESS_OPERATION_TIMEOUT, move || {
        std::process::Command::new("taskkill")
            .args(["/PID", pid_for_task.as_str(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    })
    .await
    {
        Ok(status) => status.success(),
        Err(err) => {
            warn!("failed to run taskkill for exec-server stdio process tree {pid}: {err}");
            false
        }
    }
}

pub(crate) struct JsonRpcConnection {
    pub(crate) outgoing_tx: mpsc::Sender<JSONRPCMessage>,
    pub(crate) incoming_rx: mpsc::Receiver<JsonRpcConnectionEvent>,
    pub(crate) disconnected_rx: watch::Receiver<bool>,
    pub(crate) task_handles: Vec<tokio::task::JoinHandle<()>>,
    pub(crate) transport: JsonRpcTransport,
}

impl JsonRpcConnection {
    pub(crate) fn from_stdio<R, W>(reader: R, writer: W, connection_label: String) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::from_stdio_with_max_message_len(
            reader,
            writer,
            connection_label,
            MAX_JSONRPC_MESSAGE_LEN,
        )
    }

    fn from_stdio_with_max_message_len<R, W>(
        reader: R,
        writer: W,
        connection_label: String,
        max_message_len: usize,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (incoming_tx, incoming_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (disconnected_tx, disconnected_rx) = watch::channel(false);

        let reader_label = connection_label.clone();
        let incoming_tx_for_reader = incoming_tx.clone();
        let disconnected_tx_for_reader = disconnected_tx.clone();
        // Read one byte past the payload limit so an unterminated oversized
        // message fails promptly. A trailing CR gets one more byte of lookahead
        // because it may be the first half of a valid CRLF terminator.
        let read_limit = u64::try_from(max_message_len.saturating_add(1)).unwrap_or(u64::MAX);
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                if line.capacity() > 1024 * 1024 {
                    line = String::new();
                } else {
                    line.clear();
                }
                let read_result = (&mut reader).take(read_limit).read_line(&mut line).await;
                match read_result {
                    Ok(0) => {
                        send_disconnected(
                            &incoming_tx_for_reader,
                            &disconnected_tx_for_reader,
                            /*reason*/ None,
                        )
                        .await;
                        break;
                    }
                    Ok(_) => {
                        if line.ends_with('\n') {
                            line.pop();
                            if line.ends_with('\r') {
                                line.pop();
                            }
                        } else if line.len() > max_message_len && line.ends_with('\r') {
                            match reader.read_u8().await {
                                Ok(b'\n') => {
                                    line.pop();
                                }
                                Ok(_) => {}
                                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {}
                                Err(err) => {
                                    send_disconnected(
                                        &incoming_tx_for_reader,
                                        &disconnected_tx_for_reader,
                                        Some(format!(
                                            "failed to read JSON-RPC message from {reader_label}: {err}"
                                        )),
                                    )
                                    .await;
                                    break;
                                }
                            }
                        }
                        if line.len() > max_message_len {
                            send_disconnected(
                                &incoming_tx_for_reader,
                                &disconnected_tx_for_reader,
                                Some(format!(
                                    "JSON-RPC message from {reader_label} exceeds maximum length of {max_message_len} bytes"
                                )),
                            )
                            .await;
                            break;
                        }
                        if line.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<JSONRPCMessage>(&line) {
                            Ok(message) => {
                                if incoming_tx_for_reader
                                    .send(JsonRpcConnectionEvent::Message(message))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(err) => {
                                send_malformed_message(
                                    &incoming_tx_for_reader,
                                    Some(format!(
                                        "failed to parse JSON-RPC message from {reader_label}: {err}"
                                    )),
                                )
                                .await;
                            }
                        }
                    }
                    Err(err) => {
                        send_disconnected(
                            &incoming_tx_for_reader,
                            &disconnected_tx_for_reader,
                            Some(format!(
                                "failed to read JSON-RPC message from {reader_label}: {err}"
                            )),
                        )
                        .await;
                        break;
                    }
                }
            }
        });

        let writer_task = tokio::spawn(async move {
            let mut writer = BufWriter::new(writer);
            while let Some(message) = outgoing_rx.recv().await {
                if let Err(err) = write_jsonrpc_line_message(&mut writer, &message).await {
                    send_disconnected(
                        &incoming_tx,
                        &disconnected_tx,
                        Some(format!(
                            "failed to write JSON-RPC message to {connection_label}: {err}"
                        )),
                    )
                    .await;
                    break;
                }
            }
        });

        Self {
            outgoing_tx,
            incoming_rx,
            disconnected_rx,
            task_handles: vec![reader_task, writer_task],
            transport: JsonRpcTransport::Plain,
        }
    }

    pub(crate) fn from_websocket<S>(stream: WebSocketStream<S>, connection_label: String) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self::from_websocket_stream(stream, connection_label, Some(WEBSOCKET_KEEPALIVE_INTERVAL))
    }

    pub(crate) fn from_axum_websocket(stream: AxumWebSocket, connection_label: String) -> Self {
        Self::from_websocket_stream(stream, connection_label, Some(WEBSOCKET_KEEPALIVE_INTERVAL))
    }

    fn from_websocket_stream<T, M, E>(
        websocket: T,
        connection_label: String,
        ping_interval: Option<Duration>,
    ) -> Self
    where
        T: Sink<M, Error = E> + Stream<Item = Result<M, E>> + Unpin + Send + 'static,
        M: JsonRpcWebSocketMessage,
        E: std::fmt::Display + Send + 'static,
    {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (incoming_tx, incoming_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (disconnected_tx, disconnected_rx) = watch::channel(false);

        let websocket_task = tokio::spawn(async move {
            let (mut writer, mut reader) = websocket.split();
            let liveness =
                crate::websocket_pong_watchdog::WebSocketLiveness::new(WEBSOCKET_MESSAGE_TIMEOUT);
            // Frames read ahead of a busy consumer live outside `read`: a close,
            // read error, or liveness verdict must not drop already received messages.
            let mut pending: std::collections::VecDeque<JsonRpcConnectionEvent> =
                std::collections::VecDeque::new();
            let read = async {
                loop {
                    let message = tokio::select! {
                        permit = incoming_tx.reserve(), if !pending.is_empty() => {
                            let Ok(permit) = permit else { return None; };
                            if let Some(event) = pending.pop_front() { permit.send(event); }
                            continue;
                        }
                        message = reader.next(), if pending.len() < WEBSOCKET_CONTROL_BACKLOG => message,
                    };
                    let message = message?;
                    match message {
                        Ok(message) => match message.parse_jsonrpc_frame() {
                            Ok(JsonRpcWebSocketFrame::Message(message)) => {
                                pending.push_back(JsonRpcConnectionEvent::Message(message));
                            }
                            Ok(JsonRpcWebSocketFrame::Pong) => liveness.received_pong(),
                            Ok(JsonRpcWebSocketFrame::Close) => return None,
                            Ok(JsonRpcWebSocketFrame::Ignore) => {}
                            Err(err) => {
                                pending.push_back(JsonRpcConnectionEvent::MalformedMessage {
                                    reason: format!("failed to parse websocket JSON-RPC message from {connection_label}: {err}"),
                                });
                            }
                        },
                        Err(err) => {
                            return Some(format!(
                                "failed to read websocket JSON-RPC message from {connection_label}: {err}"
                            ));
                        }
                    }
                }
            };
            let write = async {
                let mut ping_interval = ping_interval.map(|ping_interval| {
                    let mut interval = tokio::time::interval_at(
                        tokio::time::Instant::now() + ping_interval,
                        ping_interval,
                    );
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    interval
                });
                loop {
                    tokio::select! {
                        message = outgoing_rx.recv() => {
                            let message = message?;
                            if let Err(reason) = timeout(WEBSOCKET_MESSAGE_TIMEOUT, send_websocket_jsonrpc_message(
                                &mut writer, &connection_label, &message,
                            )).await.unwrap_or_else(|_| Err("websocket message write deadline expired".into())) {
                                return Some(reason);
                            }
                        }
                        _ = async {
                            match ping_interval.as_mut() {
                                Some(interval) => interval.tick().await,
                                None => std::future::pending().await,
                            }
                        } => {
                            let generation = liveness.pong_generation();
                            match timeout(WEBSOCKET_MESSAGE_TIMEOUT, writer.send(M::ping())).await {
                                Ok(Ok(())) => {},
                                Ok(Err(error)) => return Some(format!("failed to write websocket ping: {error}")),
                                Err(_) => return Some("websocket ping write deadline expired".into()),
                            }
                            // Flush errors also fail closed; never restart a partial send.
                            // A Pong can have arrived while the flush was pending.
                            liveness.ping_flushed(generation);
                        }
                    }
                }
            };
            // Keep both futures alive until disconnect: inbound progress must not
            // cancel a pending send, and a full inbound queue must not stop writes.
            let reason = tokio::select! {
                biased;
                reason = read => reason,
                reason = write => reason,
                _ = liveness.expired() => Some("websocket Pong deadline expired".into()),
            };
            // Publish the disconnect before waiting on a possibly stalled consumer,
            // then deliver received frames in order ahead of the terminal event.
            let _ = disconnected_tx.send(true);
            for event in pending.drain(..) {
                if incoming_tx.send(event).await.is_err() {
                    break;
                }
            }
            send_disconnected(&incoming_tx, &disconnected_tx, reason).await;
        });

        Self {
            outgoing_tx,
            incoming_rx,
            disconnected_rx,
            task_handles: vec![websocket_task],
            transport: JsonRpcTransport::Plain,
        }
    }


    pub(crate) fn with_managed_child_process(
        mut self,
        child_process: Child,
        managed_root: Arc<ManagedRootProcess>,
    ) -> Self {
        self.transport = JsonRpcTransport::from_managed_child_process(child_process, managed_root);
        self
    }
}

enum JsonRpcWebSocketFrame {
    Message(JSONRPCMessage),
    Pong,
    Close,
    Ignore,
}

trait JsonRpcWebSocketMessage: Send + 'static {
    fn parse_jsonrpc_frame(self) -> Result<JsonRpcWebSocketFrame, serde_json::Error>;
    fn from_text(text: String) -> Self;
    fn ping() -> Self;
}

impl JsonRpcWebSocketMessage for Message {
    fn parse_jsonrpc_frame(self) -> Result<JsonRpcWebSocketFrame, serde_json::Error> {
        match self {
            Message::Text(text) => {
                serde_json::from_str(text.as_ref()).map(JsonRpcWebSocketFrame::Message)
            }
            Message::Binary(bytes) => {
                serde_json::from_slice(bytes.as_ref()).map(JsonRpcWebSocketFrame::Message)
            }
            Message::Close(_) => Ok(JsonRpcWebSocketFrame::Close),
            Message::Pong(_) => Ok(JsonRpcWebSocketFrame::Pong),
            Message::Ping(_) | Message::Frame(_) => Ok(JsonRpcWebSocketFrame::Ignore),
        }
    }

    fn from_text(text: String) -> Self {
        Self::Text(text.into())
    }

    fn ping() -> Self {
        Self::Ping(Vec::new().into())
    }
}

impl JsonRpcWebSocketMessage for AxumWebSocketMessage {
    fn parse_jsonrpc_frame(self) -> Result<JsonRpcWebSocketFrame, serde_json::Error> {
        match self {
            AxumWebSocketMessage::Text(text) => {
                serde_json::from_str(text.as_ref()).map(JsonRpcWebSocketFrame::Message)
            }
            AxumWebSocketMessage::Binary(bytes) => {
                serde_json::from_slice(bytes.as_ref()).map(JsonRpcWebSocketFrame::Message)
            }
            AxumWebSocketMessage::Close(_) => Ok(JsonRpcWebSocketFrame::Close),
            AxumWebSocketMessage::Pong(_) => Ok(JsonRpcWebSocketFrame::Pong),
            AxumWebSocketMessage::Ping(_) => Ok(JsonRpcWebSocketFrame::Ignore),
        }
    }

    fn from_text(text: String) -> Self {
        Self::Text(text.into())
    }

    fn ping() -> Self {
        Self::Ping(Vec::new().into())
    }
}

async fn send_disconnected(
    incoming_tx: &mpsc::Sender<JsonRpcConnectionEvent>,
    disconnected_tx: &watch::Sender<bool>,
    reason: Option<String>,
) {
    let _ = disconnected_tx.send(true);
    let _ = incoming_tx
        .send(JsonRpcConnectionEvent::Disconnected { reason })
        .await;
}

async fn send_malformed_message(
    incoming_tx: &mpsc::Sender<JsonRpcConnectionEvent>,
    reason: Option<String>,
) {
    let _ = incoming_tx
        .send(JsonRpcConnectionEvent::MalformedMessage {
            reason: reason.unwrap_or_else(|| "malformed JSON-RPC message".to_string()),
        })
        .await;
}

async fn write_jsonrpc_line_message<W>(
    writer: &mut BufWriter<W>,
    message: &JSONRPCMessage,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let encoded =
        serialize_jsonrpc_message(message).map_err(|err| std::io::Error::other(err.to_string()))?;
    writer.write_all(encoded.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

async fn send_websocket_jsonrpc_message<W, M, E>(
    websocket_writer: &mut W,
    connection_label: &str,
    message: &JSONRPCMessage,
) -> Result<(), String>
where
    W: Sink<M, Error = E> + Unpin,
    M: JsonRpcWebSocketMessage,
    E: std::fmt::Display,
{
    match serialize_jsonrpc_message(message) {
        Ok(encoded) => websocket_writer
            .send(M::from_text(encoded))
            .await
            .map_err(|err| {
                format!("failed to write websocket JSON-RPC message to {connection_label}: {err}")
            }),
        Err(err) => Err(format!(
            "failed to serialize JSON-RPC message for {connection_label}: {err}"
        )),
    }
}

fn serialize_jsonrpc_message(message: &JSONRPCMessage) -> Result<String, serde_json::Error> {
    serialize_jsonrpc_message_with_limit(message, MAX_JSONRPC_MESSAGE_LEN)
}

fn serialize_jsonrpc_message_with_limit(message: &JSONRPCMessage, limit: usize) -> Result<String, serde_json::Error> {
    let encode = |message: &JSONRPCMessage| {
        let mut buffer = BoundedJsonBuffer { bytes: Vec::new(), limit };
        serde_json::to_writer(&mut buffer, message)?;
        String::from_utf8(buffer.bytes).map_err(|error| serde_json::Error::io(std::io::Error::other(error)))
    };
    match encode(message) {
        Ok(wire) => Ok(wire),
        Err(error) => {
            let id = match message {
                JSONRPCMessage::Response(response) => &response.id,
                JSONRPCMessage::Error(response) => &response.id,
                _ => return Err(error),
            };
            // An operation's oversized response must not tear down unrelated
            // sessions. Echo the admitted identity in a bounded error instead.
            encode(&JSONRPCMessage::Error(codex_exec_server_protocol::JSONRPCError {
                id: id.clone(),
                error: crate::rpc::internal_error("exec-server response exceeds the JSON-RPC byte limit".into()),
            }))
        }
    }
}

struct BoundedJsonBuffer { bytes: Vec<u8>, limit: usize }

impl std::io::Write for BoundedJsonBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("JSON-RPC byte limit exceeded"));
        }
        let required = self.bytes.len() + bytes.len();
        if required > self.bytes.capacity() {
            let capacity = required.max(self.bytes.capacity().saturating_mul(2)).min(self.limit);
            self.bytes.try_reserve_exact(capacity - self.bytes.len()).map_err(std::io::Error::other)?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

