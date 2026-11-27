//! rmcp transport adapter for an executor-managed MCP stdio process.
//!
//! This module owns the lower-level byte translation after
//! `stdio_server_launcher` has already started a process through
//! `ExecBackend::start`. It does not choose where the MCP server runs and it
//! does not implement MCP lifecycle behavior. MCP protocol ownership stays in
//! `RmcpClient` and rmcp:
//!
//! 1. rmcp serializes a JSON-RPC message and calls [`Transport::send`].
//! 2. This transport appends the stdio newline delimiter and writes those bytes
//!    to executor `process/write`.
//! 3. The executor writes the bytes to the child process stdin.
//! 4. The child writes newline-delimited JSON-RPC messages to stdout.
//! 5. The executor reports stdout bytes through pushed process events.
//! 6. This transport buffers stdout until it has one full line, deserializes
//!    that line, and returns the rmcp message from [`Transport::receive`].
//!
//! Stderr is deliberately not part of the MCP byte stream. It is logged for
//! diagnostics only, matching the local stdio implementation.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::BytesMut;
use codex_exec_server::ExecOutputStream;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEvent;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessOutputChunk;
use codex_exec_server::WriteStatus;
use memchr::memchr;
use rmcp::service::RoleClient;
use rmcp::service::RxJsonRpcMessage;
use rmcp::service::TxJsonRpcMessage;
use rmcp::transport::Transport;
use serde_json::from_slice;
use serde_json::to_vec;
use tokio::sync::broadcast;
use tokio::time::Instant;
use tokio::time::timeout_at;
use tracing::debug;
use tracing::info;
use tracing::warn;

static PROCESS_COUNTER: AtomicUsize = AtomicUsize::new(1);
/// Longest stderr diagnostic line retained for logging; longer lines are dropped.
pub(super) const MCP_STDERR_MAX_LINE_BYTES: usize = 1024 * 1024;
/// Longest stdout JSON-RPC message accepted before the transport closes.
///
/// Tool results routinely carry multi-megabyte base64 payloads such as
/// screenshots, and local stdio accepts them without a limit. This bound only
/// stops an unterminated stream from growing forever, so it matches the
/// exec-server's own 64 MiB JSON-RPC message limit rather than the diagnostic
/// line limit.
const MCP_STDOUT_MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// Observation budget for one gap repair, not for ordinary pushed output.
const OUTPUT_RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct LineBuffer {
    bytes: BytesMut,
    /// Prefix already scanned and known not to contain a newline.
    scanned_len: usize,
    /// Length of the unterminated line at the end of `bytes`.
    trailing_line_len: usize,
}

impl LineBuffer {
    fn extend_from_slice(
        &mut self,
        bytes: &[u8],
        max_line_bytes: usize,
    ) -> Result<(), LineLimitExceeded> {
        let mut trailing_line_len = self.trailing_line_len;
        let mut remaining = bytes;
        while let Some(newline_index) = memchr(b'\n', remaining) {
            if trailing_line_len.saturating_add(newline_index) > max_line_bytes {
                return Err(LineLimitExceeded);
            }
            trailing_line_len = 0;
            remaining = &remaining[newline_index + 1..];
        }
        trailing_line_len = trailing_line_len.saturating_add(remaining.len());
        if trailing_line_len > max_line_bytes {
            return Err(LineLimitExceeded);
        }

        self.bytes.extend_from_slice(bytes);
        self.trailing_line_len = trailing_line_len;
        Ok(())
    }

    fn take_line(&mut self) -> Option<BytesMut> {
        let Some(relative_index) = memchr(b'\n', &self.bytes[self.scanned_len..]) else {
            self.scanned_len = self.bytes.len();
            return None;
        };

        let newline_index = self.scanned_len + relative_index;
        let mut line = self.bytes.split_to(newline_index + 1);
        line.truncate(newline_index);
        self.scanned_len = 0;
        Some(line)
    }

    fn take_remaining(&mut self) -> Option<BytesMut> {
        if self.bytes.is_empty() {
            return None;
        }

        self.scanned_len = 0;
        self.trailing_line_len = 0;
        Some(self.bytes.split())
    }

    fn clear(&mut self) {
        self.bytes.clear();
        self.scanned_len = 0;
        self.trailing_line_len = 0;
    }
}

#[derive(Debug)]
struct LineLimitExceeded;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReceiveState {
    Open,
    CleanEof,
    Interrupted,
}

struct PendingOutputRecovery {
    deadline: Instant,
    trigger: Option<ExecProcessEvent>,
}

// Remote public implementation.

/// A client-side rmcp transport backed by an executor-managed process.
///
/// The orchestrator owns this value and calls rmcp on it. The process it wraps
/// may be local or remote depending on the `ExecBackend` used to create it, but
/// for remote MCP stdio the process lives on the executor and all interaction
/// crosses the executor process RPC boundary.
pub(super) struct ExecutorProcessTransport {
    /// Logical process handle returned by the executor process API.
    ///
    /// `write` forwards stdin bytes. The wrapping `StdioServerProcessHandle`
    /// owns process termination.
    process: Arc<dyn ExecProcess>,

    /// Pushed output/lifecycle stream for the process.
    ///
    /// The executor process API still supports retained-output reads, but MCP
    /// stdio is naturally streaming. This receiver lets rmcp wait for stdout
    /// chunks without issuing `process/read` after each output notification.
    events: ExecProcessEventReceiver,

    /// Human-readable program name used only in diagnostics.
    program_name: String,

    /// Buffered child stdout bytes that have not yet formed a complete
    /// newline-delimited JSON-RPC message.
    stdout: LineBuffer,

    /// Buffered stderr bytes for diagnostic logging.
    stderr: LineBuffer,

    /// Only an ordered process close proves that stdout ended without loss.
    receive_state: ReceiveState,

    /// Highest executor process event sequence observed by this transport.
    ///
    /// When the pushed event stream lags, use this as the retained-output read
    /// cursor to recover missed stdout/stderr chunks from the executor.
    last_seq: u64,
    exit_known: bool,

    // Retain the consumed trigger and its original deadline if rmcp drops a
    // receive future. The receive future still owns the read itself.
    pending_output_recovery: Option<PendingOutputRecovery>,
}

impl ExecutorProcessTransport {
    pub(super) fn new(process: Arc<dyn ExecProcess>, program_name: String) -> Self {
        // Subscribe before returning the transport to rmcp. Some test servers
        // can emit output or exit quickly after `process/start`, and the
        // process event log will replay anything that landed before this
        // subscriber was attached.
        let events = process.subscribe_events();
        Self {
            process,
            events,
            program_name,
            stdout: LineBuffer::default(),
            stderr: LineBuffer::default(),
            receive_state: ReceiveState::Open,
            last_seq: 0,
            exit_known: false,
            pending_output_recovery: None,
        }
    }

    pub(super) fn next_process_id() -> ProcessId {
        // Process IDs are logical handles scoped to the executor connection,
        // not OS pids. A monotonic client-side id is enough to avoid
        // collisions between MCP servers started in the same session.
        let index = PROCESS_COUNTER.fetch_add(1, Ordering::Relaxed);
        ProcessId::from(format!("mcp-stdio-{index}"))
    }
}

impl Transport<RoleClient> for ExecutorProcessTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send + 'static {
        let process = Arc::clone(&self.process);
        async move {
            // rmcp hands us a structured JSON-RPC message. Stdio transport on
            // the wire is JSON plus one newline delimiter.
            let mut bytes = to_vec(&item).map_err(io::Error::other)?;
            bytes.push(b'\n');
            let response = process.write(bytes).await.map_err(io::Error::other)?;
            match response.status {
                WriteStatus::Accepted => Ok(()),
                WriteStatus::UnknownProcess => {
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "unknown process"))
                }
                WriteStatus::StdinClosed => {
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdin closed"))
                }
                WriteStatus::Starting => Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "process is starting",
                )),
            }
        }
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.receive_message()
    }

    async fn close(&mut self) -> std::result::Result<(), Self::Error> {
        // `StdioServerTransport` terminates through its shared process handle
        // before closing this byte adapter.
        Ok(())
    }
}

impl ExecutorProcessTransport {
    async fn receive_message(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        loop {
            // rmcp stdio framing is line-oriented JSON. We first drain any
            // complete line already buffered from an earlier process event.
            if let Some(message) = self.take_stdout_message() {
                return Some(message);
            }
            if self.receive_state != ReceiveState::Open {
                // Complete lines have been drained. Never decode an
                // unterminated suffix after an interrupted stream.
                self.stdout.clear();
                self.flush_stderr();
                return None;
            }

            if let Some(recovery) = &self.pending_output_recovery {
                let deadline = recovery.deadline;
                let failure_context = if recovery.trigger.is_some() {
                    "Failed to recover remote MCP output sequence"
                } else {
                    "Failed to recover remote MCP server output stream"
                };
                if let Err(error) = self.recover_events(deadline).await {
                    warn!("{failure_context} ({}): {error}", self.program_name);
                    self.pending_output_recovery = None;
                    self.receive_state = ReceiveState::Interrupted;
                }
                continue;
            }

            match self.events.recv().await {
                Ok(event) if self.has_forward_gap(&event) => {
                    self.begin_output_recovery(Some(event));
                }
                Ok(event) => self.handle_process_event(event),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(
                        "Remote MCP server output stream lagged ({}): skipped {skipped} events",
                        self.program_name
                    );
                    self.begin_output_recovery(None);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    warn!(
                        "Remote MCP server output receiver closed before process closure ({})",
                        self.program_name
                    );
                    self.receive_state = ReceiveState::Interrupted;
                }
            }
        }
    }

    fn handle_process_event(&mut self, event: ExecProcessEvent) {
        match event {
            ExecProcessEvent::OutputGap { through_seq, .. } => {
                if through_seq <= self.last_seq {
                    return;
                }
                // A JSON-RPC byte stream cannot safely resume through a
                // missing frame. Report transport loss, not process death.
                warn!(
                    "Remote MCP output lost through sequence {through_seq} ({})",
                    self.program_name
                );
                self.receive_state = ReceiveState::Interrupted;
            }
            ExecProcessEvent::Output(chunk) => self.push_process_output_if_new(chunk),
            ExecProcessEvent::Exited { seq, .. } => {
                self.note_seq(seq);
                self.exit_known = true;
                // Wait for `Closed` so shutdown output can still be decoded.
            }
            ExecProcessEvent::Closed { seq, .. } => {
                self.note_seq(seq);
                self.receive_state = ReceiveState::CleanEof;
            }
            ExecProcessEvent::Failed(message) => {
                warn!(
                    "Remote MCP server process failed ({}): {message}",
                    self.program_name
                );
                self.receive_state = ReceiveState::Interrupted;
            }
        }
    }

    fn note_seq(&mut self, seq: u64) {
        self.last_seq = self.last_seq.max(seq);
    }

    fn has_forward_gap(&self, event: &ExecProcessEvent) -> bool {
        let seq = match event {
            ExecProcessEvent::Output(chunk) => Some(chunk.seq),
            ExecProcessEvent::Exited { seq, .. } | ExecProcessEvent::Closed { seq, .. } => {
                Some(*seq)
            }
            // A producer capture loss occupies one sequence. Initial
            // subscriber history can be truncated before it, so recover any
            // retained prefix just as for a sequenced output event.
            ExecProcessEvent::OutputGap {
                through_seq,
                capture_incomplete: Some(_),
            } => Some(*through_seq),
            _ => None,
        };
        seq.is_some_and(|seq| seq > self.last_seq.saturating_add(1))
    }

    fn should_accept_seq(&mut self, seq: u64) -> bool {
        if seq <= self.last_seq {
            return false;
        }
        self.last_seq = seq;
        true
    }

    fn begin_output_recovery(&mut self, trigger: Option<ExecProcessEvent>) {
        self.pending_output_recovery = Some(PendingOutputRecovery {
            deadline: Instant::now() + OUTPUT_RECOVERY_TIMEOUT,
            trigger,
        });
    }

    async fn recover_events(&mut self, deadline: Instant) -> io::Result<()> {
        Self::check_output_recovery_deadline(deadline)?;
        // wait_ms=0 only skips the server's long poll; queueing and reply
        // delivery can still stall. With no byte cap this read is a single
        // snapshot through its reported sequence, including terminal events.
        let response = timeout_at(
            deadline,
            self.process.read(
                Some(self.last_seq),
                /*max_bytes*/ None,
                /*wait_ms*/ Some(0),
            ),
        )
        .await
        .map_err(|_| Self::output_recovery_timeout())?
        .map_err(io::Error::other)?;
        Self::check_output_recovery_deadline(deadline)?;
        let trigger = self
            .pending_output_recovery
            .take()
            .and_then(|recovery| recovery.trigger);
        // A retained read can include valid output before its first gap.
        // Replay in sequence order and stop at that gap rather than dropping
        // the entire validated prefix or interpreting a later close as EOF.
        let recovered = codex_exec_server::replay_process_events(
            self.last_seq,
            self.exit_known,
            response,
            trigger,
        )
        .map_err(io::Error::other)?;
        for event in recovered {
            // Do not promote a late terminal event to clean EOF. Any complete
            // lines already buffered still drain on the interrupted path.
            Self::check_output_recovery_deadline(deadline)?;
            self.handle_process_event(event);
            if self.receive_state != ReceiveState::Open {
                return Ok(());
            }
        }
        Self::check_output_recovery_deadline(deadline)
    }

    fn check_output_recovery_deadline(deadline: Instant) -> io::Result<()> {
        if Instant::now() >= deadline {
            return Err(Self::output_recovery_timeout());
        }
        Ok(())
    }

    fn output_recovery_timeout() -> io::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("remote MCP output recovery timed out after {OUTPUT_RECOVERY_TIMEOUT:?}"),
        )
    }

    fn push_process_output_if_new(&mut self, chunk: ProcessOutputChunk) {
        if !self.should_accept_seq(chunk.seq) {
            return;
        }
        self.push_process_output(chunk);
    }

    fn push_process_output(&mut self, chunk: ProcessOutputChunk) {
        let bytes = chunk.chunk.into_inner();
        match chunk.stream {
            // MCP stdio uses stdout as the protocol stream. PTY output is
            // accepted defensively because the executor process API has a
            // unified stream enum, but remote MCP starts with `tty=false`.
            ExecOutputStream::Stdout | ExecOutputStream::Pty => {
                if self
                    .stdout
                    .extend_from_slice(&bytes, MCP_STDOUT_MAX_MESSAGE_BYTES)
                    .is_err()
                {
                    warn!(
                        "Remote MCP server stdout message exceeded the {MCP_STDOUT_MAX_MESSAGE_BYTES}-byte limit ({}); closing transport",
                        self.program_name
                    );
                    self.receive_state = ReceiveState::Interrupted;
                }
            }
            // Stderr is intentionally out-of-band. It should help debug server
            // startup failures without entering rmcp framing.
            ExecOutputStream::Stderr => {
                self.push_stderr(&bytes);
            }
        }
    }

    fn take_stdout_message(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        // A normal MCP stdio server emits one JSON-RPC message per newline.
        // At clean EOF, accept a final unterminated line like local rmcp's
        // `decode_eof` handling.
        loop {
            let line = match self.stdout.take_line() {
                Some(line) => line,
                None if self.receive_state == ReceiveState::CleanEof => {
                    self.stdout.take_remaining()?
                }
                None => return None,
            };
            let line = Self::trim_trailing_carriage_return(line);
            match from_slice::<RxJsonRpcMessage<RoleClient>>(&line) {
                Ok(message) => return Some(message),
                Err(error) => {
                    debug!(
                        "Failed to parse remote MCP server message ({}): {error}",
                        self.program_name
                    );
                }
            }
        }
    }

    fn push_stderr(&mut self, bytes: &[u8]) {
        // Keep stderr line-oriented in logs so a chatty MCP server does not
        // produce one log record per byte chunk.
        if self
            .stderr
            .extend_from_slice(bytes, MCP_STDERR_MAX_LINE_BYTES)
            .is_err()
        {
            warn!(
                "MCP server stderr line exceeded the {MCP_STDERR_MAX_LINE_BYTES}-byte limit ({}); discarding buffered diagnostics",
                self.program_name
            );
            self.stderr.clear();
            return;
        }
        while let Some(line) = self.stderr.take_line() {
            let line = Self::trim_trailing_carriage_return(line);
            info!(
                "MCP server stderr ({}): {}",
                self.program_name,
                String::from_utf8_lossy(&line)
            );
        }
    }

    fn flush_stderr(&mut self) {
        let Some(line) = self.stderr.take_remaining() else {
            return;
        };
        info!(
            "MCP server stderr ({}): {}",
            self.program_name,
            String::from_utf8_lossy(&line)
        );
    }

    fn trim_trailing_carriage_return(mut line: BytesMut) -> BytesMut {
        if line.last() == Some(&b'\r') {
            line.truncate(line.len() - 1);
        }
        line
    }
}

