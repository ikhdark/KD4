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
#[cfg(test)]
pub(crate) const WEBSOCKET_KEEPALIVE_INTERVAL: Duration = Duration::from_millis(25);
#[cfg(not(test))]
pub(crate) const WEBSOCKET_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

// Whole-message policy, not a process/RPC timeout. Unlike 60 KiB Noise
// records, legacy peers may place a Pong behind one 64 MiB frame. Allow
// that frame 64 KiB/s plus 60 seconds of slack in either direction. A
// continuously slower link must use a smaller message/streamed file read.
#[cfg(not(test))]
pub(crate) const WEBSOCKET_MESSAGE_TIMEOUT: Duration =
    Duration::from_secs(60 + (MAX_JSONRPC_MESSAGE_LEN / (64 * 1024)) as u64);
#[cfg(test)]
pub(crate) const WEBSOCKET_MESSAGE_TIMEOUT: Duration = Duration::from_millis(200);

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
    #[cfg(test)]
    fn from_child_process(child_process: Child) -> Self {
        Self::Stdio {
            transport: StdioTransport::spawn(child_process, None),
        }
    }

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

    #[cfg(test)]
    pub(crate) fn pending_supervisor_for_test() -> (Self, watch::Receiver<bool>, watch::Sender<bool>)
    {
        let (terminate_tx, terminate_rx) = watch::channel(false);
        let (terminated_tx, terminated_rx) = watch::channel(false);
        let transport = StdioTransport {
            handle: Arc::new(StdioTransportHandle {
                terminate_tx,
                terminated_rx,
                terminate_requested: AtomicBool::new(false),
            }),
        };
        (Self::Stdio { transport }, terminate_rx, terminated_tx)
    }

    #[cfg(test)]
    pub(crate) fn termination_completed(&self) -> bool {
        match self {
            Self::Plain => true,
            Self::Stdio { transport } => transport.termination_completed(),
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
                    break;
                }
                if terminated_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .is_ok();
        if !completed {
            warn!("exec-server stdio child supervisor exceeded its termination deadline");
        }
        completed
    }

    #[cfg(test)]
    fn termination_completed(&self) -> bool {
        *self.handle.terminated_rx.borrow()
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
        let _completion = StdioChildSupervisorCompletion(terminated_tx);
        tokio::select! {
            result = child_process.wait() => {
                log_stdio_child_wait_result(result);
                kill_process_tree(&mut child_process, process_group_id).await;
            }
            () = wait_for_stdio_termination(&mut terminate_rx) => {
                if let Some(managed_root) = managed_root.as_ref()
                    && let Err(err) = managed_root.terminate()
                {
                    debug!("failed to terminate managed exec-server stdio process tree: {err}");
                }
                terminate_stdio_child(&mut child_process, process_group_id).await;
            }
        }
    });
}

struct StdioChildSupervisorCompletion(watch::Sender<bool>);

impl Drop for StdioChildSupervisorCompletion {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
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

async fn terminate_stdio_child(child_process: &mut Child, process_group_id: Option<u32>) {
    terminate_process_tree(child_process, process_group_id).await;
    match timeout(STDIO_TERMINATION_GRACE_PERIOD, child_process.wait()).await {
        Ok(result) => {
            log_stdio_child_wait_result(result);
        }
        Err(_) => {
            kill_process_tree(child_process, process_group_id).await;
            match timeout(STDIO_FORCE_REAP_TIMEOUT, child_process.wait()).await {
                Ok(result) => log_stdio_child_wait_result(result),
                Err(_) => warn!(
                    "exec-server stdio child did not exit after forced termination within {STDIO_FORCE_REAP_TIMEOUT:?}"
                ),
            }
        }
    }
}

async fn terminate_process_tree(child_process: &mut Child, process_group_id: Option<u32>) {
    let Some(process_group_id) = process_group_id else {
        kill_direct_child(child_process, "terminate");
        return;
    };

    if !kill_windows_process_tree(process_group_id).await {
        kill_direct_child(child_process, "terminate");
    }
}

async fn kill_process_tree(child_process: &mut Child, process_group_id: Option<u32>) {
    let Some(process_group_id) = process_group_id else {
        kill_direct_child(child_process, "kill");
        return;
    };

    if !kill_windows_process_tree(process_group_id).await {
        kill_direct_child(child_process, "kill");
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

fn log_stdio_child_wait_result(result: std::io::Result<std::process::ExitStatus>) {
    if let Err(err) = result {
        debug!("failed to wait for exec-server stdio child: {err}");
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

    #[cfg(test)]
    pub(crate) fn with_child_process(mut self, child_process: Child) -> Self {
        self.transport = JsonRpcTransport::from_child_process(child_process);
        self
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
    serde_json::to_string(message)
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::task::Context;
    use std::task::Poll;

    use codex_exec_server_protocol::JSONRPCRequest;
    use codex_exec_server_protocol::RequestId;
    use futures::channel::mpsc as futures_mpsc;
    use futures::task::AtomicWaker;
    use pretty_assertions::assert_eq;
    use tokio::net::TcpListener;
    use tokio::time::timeout;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::connect_async;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn stdio_transport_close_has_a_deadline_without_discarding_supervisor_state() {
        let (terminate_tx, _terminate_rx) = watch::channel(false);
        let (terminated_tx, terminated_rx) = watch::channel(false);
        let transport = StdioTransport {
            handle: Arc::new(StdioTransportHandle {
                terminate_tx,
                terminated_rx,
                terminate_requested: AtomicBool::new(false),
            }),
        };

        assert!(
            !transport
                .terminate_and_wait_with_timeout(Duration::from_secs(2))
                .await,
            "close should return when the supervisor misses its deadline"
        );
        assert!(!transport.termination_completed());

        terminated_tx.send_replace(true);
        assert!(transport.termination_completed());
    }

    #[tokio::test]
    async fn stdio_connection_rejects_ambiguous_envelopes_and_keeps_reading() -> anyhow::Result<()>
    {
        let (reader, mut peer) = tokio::io::duplex(1024);
        let mut connection = JsonRpcConnection::from_stdio_with_max_message_len(
            reader,
            tokio::io::sink(),
            "test stdio peer".to_string(),
            1024,
        );
        for wire in [
            r#"{"id":null,"method":"process/read"}"#,
            r#"{"id":1,"result":{},"error":{"code":-32000,"message":"failed"}}"#,
        ] {
            peer.write_all(wire.as_bytes()).await?;
            peer.write_all(b"\n").await?;
            let event = timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?;
            assert!(
                matches!(event, Some(JsonRpcConnectionEvent::MalformedMessage { .. })),
                "{event:?}"
            );
        }

        let expected = test_jsonrpc_message();
        peer.write_all(serde_json::to_string(&expected)?.as_bytes())
            .await?;
        peer.write_all(b"\n").await?;
        let event = timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?;
        match event {
            Some(JsonRpcConnectionEvent::Message(actual)) => assert_eq!(actual, expected),
            event => {
                anyhow::bail!("expected valid message after malformed envelopes, got {event:?}")
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn stdio_connection_accepts_message_at_size_limit() -> anyhow::Result<()> {
        let message = test_jsonrpc_message();
        let encoded = serde_json::to_string(&message)?;
        let max_message_len = encoded.len();

        for line_ending in [b"\n".as_slice(), b"\r\n".as_slice()] {
            let (reader, mut peer) =
                tokio::io::duplex(max_message_len.saturating_add(line_ending.len()));
            let mut connection = JsonRpcConnection::from_stdio_with_max_message_len(
                reader,
                tokio::io::sink(),
                "test stdio peer".to_string(),
                max_message_len,
            );

            peer.write_all(encoded.as_bytes()).await?;
            peer.write_all(line_ending).await?;
            let event = timeout(Duration::from_secs(1), connection.incoming_rx.recv())
                .await?
                .expect("stdio connection should report the message");
            match event {
                JsonRpcConnectionEvent::Message(actual) => assert_eq!(actual, message),
                event => anyhow::bail!("expected JSON-RPC message, got {event:?}"),
            }

            drop(peer);
            drop(connection);
        }

        Ok(())
    }

    #[tokio::test]
    async fn stdio_connection_rejects_overlong_unterminated_message() -> anyhow::Result<()> {
        let max_message_len: usize = 32;
        let (reader, mut peer) = tokio::io::duplex(max_message_len.saturating_add(1));
        let mut connection = JsonRpcConnection::from_stdio_with_max_message_len(
            reader,
            tokio::io::sink(),
            "hostile stdio peer".to_string(),
            max_message_len,
        );
        let overlong_message = vec![b'x'; max_message_len + 1];

        peer.write_all(&overlong_message).await?;
        let event = timeout(Duration::from_secs(1), connection.incoming_rx.recv())
            .await?
            .expect("stdio connection should report the framing violation");
        match event {
            JsonRpcConnectionEvent::Disconnected { reason } => assert_eq!(
                reason,
                Some(
                    "JSON-RPC message from hostile stdio peer exceeds maximum length of 32 bytes"
                        .to_string()
                )
            ),
            event => anyhow::bail!("expected stdio disconnect, got {event:?}"),
        }

        drop(peer);
        drop(connection);
        Ok(())
    }

    #[tokio::test]
    async fn websocket_connection_sends_configured_ping() -> anyhow::Result<()> {
        let (client_websocket, mut server_websocket) = websocket_pair().await?;
        let connection = JsonRpcConnection::from_websocket_stream(
            client_websocket,
            "test".into(),
            Some(WEBSOCKET_KEEPALIVE_INTERVAL),
        );

        let message = timeout(Duration::from_secs(1), server_websocket.next())
            .await?
            .expect("websocket should stay open")?;
        assert!(matches!(message, Message::Ping(_)));

        drop(connection);
        Ok(())
    }

    #[tokio::test]
    async fn websocket_connection_ignores_pong_accepts_text_and_binary_then_reports_close()
    -> anyhow::Result<()> {
        let (client_websocket, mut server_websocket) = websocket_pair().await?;
        let mut connection = JsonRpcConnection::from_websocket(client_websocket, "test".into());

        server_websocket
            .send(Message::Pong(b"check".to_vec().into()))
            .await?;
        let message = test_jsonrpc_message();
        server_websocket
            .send(Message::Text(serde_json::to_string(&message)?.into()))
            .await?;
        assert!(matches!(
            timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?,
            Some(JsonRpcConnectionEvent::Message(actual)) if actual == message
        ));

        server_websocket
            .send(Message::Binary(serde_json::to_vec(&message)?.into()))
            .await?;
        assert!(matches!(
            timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?,
            Some(JsonRpcConnectionEvent::Message(actual)) if actual == message
        ));
        server_websocket.close(None).await?;
        assert!(matches!(
            timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?,
            Some(JsonRpcConnectionEvent::Disconnected { reason: None })
        ));
        assert!(*connection.disconnected_rx.borrow());

        drop(connection);
        Ok(())
    }

    #[tokio::test]
    async fn websocket_connection_accepts_messages_above_default_frame_limit() -> anyhow::Result<()>
    {
        // One frame larger than tungstenite's 16 MiB default, still below the
        // shared JSON-RPC ceiling enforced by stdio and Noise.
        let large_text = "x".repeat(17 * 1024 * 1024);
        let message = JSONRPCMessage::Request(JSONRPCRequest {
            id: RequestId::Integer(1),
            method: "large".to_string(),
            params: Some(serde_json::json!({ "data": large_text })),
            trace: None,
        });
        let encoded = serde_json::to_string(&message)?;
        assert!(encoded.len() > 16 * 1024 * 1024 && encoded.len() < MAX_JSONRPC_MESSAGE_LEN);

        for configured in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let websocket_url = format!("ws://{}", listener.local_addr()?);
            let server_task = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                accept_async(stream).await.map_err(anyhow::Error::from)
            });
            let config = configured.then(jsonrpc_websocket_config);
            let (client_websocket, _) = tokio_tungstenite::connect_async_with_config(
                websocket_url,
                config,
                /*disable_nagle*/ false,
            )
            .await?;
            let mut server_websocket = server_task.await??;
            let mut connection = JsonRpcConnection::from_websocket(client_websocket, "test".into());

            let sent = timeout(
                Duration::from_secs(10),
                server_websocket.send(Message::Text(encoded.clone().into())),
            )
            .await?;
            let event = timeout(Duration::from_secs(10), connection.incoming_rx.recv()).await?;
            if configured {
                sent?;
                assert!(
                    matches!(&event, Some(JsonRpcConnectionEvent::Message(actual)) if actual == &message),
                    "configured reader must accept the message"
                );
            } else {
                // Proves the payload exercises the frame limit rather than passing trivially.
                assert!(
                    matches!(
                        event,
                        Some(JsonRpcConnectionEvent::Disconnected { reason: Some(_) })
                    ),
                    "default reader must reject the oversized frame"
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn websocket_connection_keeps_outbound_message_while_send_is_backpressured()
    -> anyhow::Result<()> {
        let (websocket, control, mut outbound_rx) =
            ControlledWebSocket::new(/*write_ready*/ false);
        let mut connection = JsonRpcConnection::from_websocket_stream(
            websocket,
            "test".into(),
            /*ping_interval*/ None,
        );
        let message = test_jsonrpc_message();

        connection.outgoing_tx.send(message.clone()).await?;
        control.wait_for_blocked_write().await?;
        control.send_inbound(Message::Text(serde_json::to_string(&message)?.into()))?;
        assert!(matches!(
            timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?,
            Some(JsonRpcConnectionEvent::Message(inbound)) if inbound == message
        ));

        control.set_write_ready();
        assert!(matches!(
            timeout(Duration::from_secs(1), outbound_rx.next()).await?,
            Some(Message::Text(text)) if serde_json::from_str::<JSONRPCMessage>(&text)? == message
        ));
        drop(connection);
        Ok(())
    }

    #[tokio::test]
    async fn websocket_connection_writes_while_incoming_queue_is_full() -> anyhow::Result<()> {
        let (websocket, control, mut outbound_rx) = ControlledWebSocket::new(true);
        let connection = JsonRpcConnection::from_websocket_stream(websocket, "test".into(), None);
        let message = test_jsonrpc_message();
        for _ in 0..=CHANNEL_CAPACITY {
            control.send_inbound(Message::Text(serde_json::to_string(&message)?.into()))?;
        }
        timeout(Duration::from_secs(1), async {
            while connection.incoming_rx.len() != CHANNEL_CAPACITY {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        connection.outgoing_tx.send(message.clone()).await?;
        assert!(matches!(
            timeout(Duration::from_secs(1), outbound_rx.next()).await?,
            Some(Message::Text(text)) if serde_json::from_str::<JSONRPCMessage>(&text)? == message
        ));
        drop(connection);
        Ok(())
    }

    #[tokio::test]
    async fn websocket_connection_observes_close_while_write_is_blocked() -> anyhow::Result<()> {
        let (websocket, control, _outbound_rx) = ControlledWebSocket::new(false);
        let mut connection =
            JsonRpcConnection::from_websocket_stream(websocket, "test".into(), None);
        connection.outgoing_tx.send(test_jsonrpc_message()).await?;
        control.wait_for_blocked_write().await?;
        control.send_inbound(Message::Close(None))?;
        assert!(matches!(
            timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?,
            Some(JsonRpcConnectionEvent::Disconnected { reason: None })
        ));
        assert!(*connection.disconnected_rx.borrow());
        drop(connection);
        Ok(())
    }

    #[tokio::test]
    async fn websocket_close_after_read_ahead_delivers_every_received_message() -> anyhow::Result<()>
    {
        // The reader races delivery against the next frame. Repeat so a close
        // read while messages are still staged cannot pass by chance.
        let message = test_jsonrpc_message();
        for _ in 0..32 {
            let (websocket, control, _outbound_rx) = ControlledWebSocket::new(true);
            let mut connection =
                JsonRpcConnection::from_websocket_stream(websocket, "test".into(), None);
            for _ in 0..4 {
                control.send_inbound(Message::Text(serde_json::to_string(&message)?.into()))?;
            }
            control.send_inbound(Message::Close(None))?;
            for _ in 0..4 {
                assert!(matches!(
                    timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?,
                    Some(JsonRpcConnectionEvent::Message(actual)) if actual == message
                ));
            }
            assert!(matches!(
                timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?,
                Some(JsonRpcConnectionEvent::Disconnected { reason: None })
            ));
        }
        Ok(())
    }

    async fn websocket_pair() -> anyhow::Result<(
        WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        WebSocketStream<tokio::net::TcpStream>,
    )> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let websocket_url = format!("ws://{}", listener.local_addr()?);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            accept_async(stream).await.map_err(anyhow::Error::from)
        });
        let (client_websocket, _) = connect_async(websocket_url).await?;
        let server_websocket = server_task.await??;
        Ok((client_websocket, server_websocket))
    }

    fn test_jsonrpc_message() -> JSONRPCMessage {
        JSONRPCMessage::Request(JSONRPCRequest {
            id: RequestId::Integer(1),
            method: "test".to_string(),
            params: None,
            trace: None,
        })
    }

    struct ControlledWebSocket {
        inbound_rx: futures_mpsc::UnboundedReceiver<Result<Message, std::convert::Infallible>>,
        outbound_tx: futures_mpsc::UnboundedSender<Message>,
        write_ready: Arc<AtomicBool>,
        write_blocked: Arc<AtomicBool>,
        write_blocked_waker: Arc<AtomicWaker>,
        write_waker: Arc<AtomicWaker>,
    }

    #[tokio::test(start_paused = true)]
    async fn websocket_silent_peer_and_blocked_sink_fail_boundedly() {
        for ready in [true, false] {
            let (socket, _control, _outbound) = ControlledWebSocket::new(ready);
            let mut connection = JsonRpcConnection::from_websocket_stream(
                socket,
                "silent".into(),
                Some(Duration::from_millis(25)),
            );
            connection
                .outgoing_tx
                .send(test_jsonrpc_message())
                .await
                .unwrap();
            timeout(Duration::from_secs(1), async {
                while !*connection.disconnected_rx.borrow_and_update() {
                    connection.disconnected_rx.changed().await.unwrap();
                }
            })
            .await
            .expect("silent peer or blocked flush must release pending owners");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn websocket_healthy_pongs_keep_a_quiet_connection_alive() {
        let (socket, control, mut outbound) = ControlledWebSocket::new(true);
        let connection = JsonRpcConnection::from_websocket_stream(
            socket,
            "healthy".into(),
            Some(Duration::from_millis(25)),
        );
        for _ in 0..12 {
            assert!(matches!(outbound.next().await, Some(Message::Ping(_))));
            control
                .send_inbound(Message::Pong(Vec::new().into()))
                .unwrap();
            tokio::task::yield_now().await;
            assert!(!*connection.disconnected_rx.borrow());
        }
    }

    struct ControlledWebSocketHandle {
        inbound_tx: futures_mpsc::UnboundedSender<Result<Message, std::convert::Infallible>>,
        write_ready: Arc<AtomicBool>,
        write_blocked: Arc<AtomicBool>,
        write_blocked_waker: Arc<AtomicWaker>,
        write_waker: Arc<AtomicWaker>,
    }

    impl ControlledWebSocket {
        fn new(
            write_ready: bool,
        ) -> (
            Self,
            ControlledWebSocketHandle,
            futures_mpsc::UnboundedReceiver<Message>,
        ) {
            let (inbound_tx, inbound_rx) = futures_mpsc::unbounded();
            let (outbound_tx, outbound_rx) = futures_mpsc::unbounded();
            let write_ready = Arc::new(AtomicBool::new(write_ready));
            let write_blocked = Arc::new(AtomicBool::new(false));
            let write_blocked_waker = Arc::new(AtomicWaker::new());
            let write_waker = Arc::new(AtomicWaker::new());
            (
                Self {
                    inbound_rx,
                    outbound_tx,
                    write_ready: Arc::clone(&write_ready),
                    write_blocked: Arc::clone(&write_blocked),
                    write_blocked_waker: Arc::clone(&write_blocked_waker),
                    write_waker: Arc::clone(&write_waker),
                },
                ControlledWebSocketHandle {
                    inbound_tx,
                    write_ready,
                    write_blocked,
                    write_blocked_waker,
                    write_waker,
                },
                outbound_rx,
            )
        }
    }

    impl ControlledWebSocketHandle {
        fn send_inbound(&self, message: Message) -> anyhow::Result<()> {
            self.inbound_tx
                .unbounded_send(Ok(message))
                .map_err(anyhow::Error::from)
        }

        fn set_write_ready(&self) {
            self.write_ready.store(true, Ordering::Release);
            self.write_waker.wake();
        }

        async fn wait_for_blocked_write(&self) -> anyhow::Result<()> {
            timeout(
                Duration::from_secs(1),
                futures::future::poll_fn(|cx| {
                    if self.write_blocked.load(Ordering::Acquire) {
                        Poll::Ready(())
                    } else {
                        self.write_blocked_waker.register(cx.waker());
                        Poll::Pending
                    }
                }),
            )
            .await?;
            Ok(())
        }
    }

    impl Sink<Message> for ControlledWebSocket {
        type Error = std::convert::Infallible;

        fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            if self.write_ready.load(Ordering::Acquire) {
                Poll::Ready(Ok(()))
            } else {
                self.write_blocked.store(true, Ordering::Release);
                self.write_blocked_waker.wake();
                self.write_waker.register(cx.waker());
                Poll::Pending
            }
        }

        fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            self.outbound_tx
                .unbounded_send(item)
                .expect("test outbound receiver should stay open");
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    impl Stream for ControlledWebSocket {
        type Item = Result<Message, std::convert::Infallible>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.inbound_rx).poll_next(cx)
        }
    }
}
