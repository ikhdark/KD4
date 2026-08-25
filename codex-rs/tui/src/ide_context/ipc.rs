//! Private transport for fetching IDE context for TUI `/ide` support.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;

use serde_json::json;
use thiserror::Error;

use super::IdeContext;

// The desktop IPC client gives requests 5 seconds to complete. Match that prompt-time budget here:
// fetching IDE context includes router discovery and extension event-loop work, so a shorter TUI
// deadline can incorrectly skip context even though the IDE answers normally.
const IDE_CONTEXT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

const MAX_IPC_FRAME_BYTES: usize = 256 * 1024 * 1024;

const TUI_SOURCE_CLIENT_ID: &str = "codex-tui";

const OPEN_IDE_HINT: &str =
    "Open this project in VS Code or Cursor with the Codex extension active.";

const IDE_DID_NOT_PROVIDE_CONTEXT_HINT: &str = "The IDE extension did not provide context.";

const KEEP_TRYING_HINT: &str = "Codex will keep trying on future messages.";

#[derive(Debug, Error)]
pub(crate) enum IdeContextError {
    #[error("failed to connect to IDE context provider: {0}")]
    Connect(std::io::Error),

    #[error("failed to request IDE context: {0}")]
    Send(std::io::Error),

    #[error("failed to read IDE context: {0}")]
    Read(std::io::Error),

    #[error("invalid IDE context response: {0}")]
    InvalidResponse(String),

    #[error("IDE context response exceeded maximum size")]
    ResponseTooLarge,

    #[error("IDE context request failed")]
    RequestFailed(String),
}

impl IdeContextError {
    pub(crate) fn user_facing_hint(&self) -> String {
        match self {
            IdeContextError::Connect(_) => OPEN_IDE_HINT.to_string(),
            IdeContextError::RequestFailed(error) if error == "no-client-found" => {
                OPEN_IDE_HINT.to_string()
            }
            IdeContextError::RequestFailed(_) => {
                format!("{IDE_DID_NOT_PROVIDE_CONTEXT_HINT} Try /ide again.")
            }
            IdeContextError::ResponseTooLarge => {
                "The selected IDE context is too large. Clear any large selection in your IDE and try /ide again.".to_string()
            }
            IdeContextError::Send(_) => {
                "Codex could not request IDE context. Try /ide again.".to_string()
            }
            IdeContextError::Read(_) | IdeContextError::InvalidResponse(_) => {
                "Codex could not read IDE context. Try /ide again.".to_string()
            }
        }
    }

    pub(crate) fn prompt_skip_hint(&self) -> String {
        match self {
            IdeContextError::ResponseTooLarge => {
                "The selected IDE context is too large. Clear any large selection in your IDE."
                    .to_string()
            }
            IdeContextError::Connect(_) => OPEN_IDE_HINT.to_string(),
            IdeContextError::RequestFailed(error) if error == "no-client-found" => {
                OPEN_IDE_HINT.to_string()
            }
            IdeContextError::Read(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                "Codex timed out waiting for IDE context. It will keep trying on future messages."
                    .to_string()
            }
            IdeContextError::RequestFailed(error) if error == "client-disconnected" => {
                hint_with_retry("The IDE connection changed while Codex was requesting context.")
            }
            IdeContextError::RequestFailed(error) if error == "request-timeout" => {
                hint_with_retry("The IDE extension did not answer in time.")
            }
            IdeContextError::RequestFailed(error) if error == "request-version-mismatch" => {
                "The connected IDE extension is not compatible with this IDE context request."
                    .to_string()
            }
            IdeContextError::RequestFailed(error) if error == "no-handler-for-request" => {
                "The connected IDE client does not support IDE context requests.".to_string()
            }
            IdeContextError::Send(_) => {
                hint_with_retry("Codex lost the IDE connection while requesting context.")
            }
            IdeContextError::InvalidResponse(_) => {
                hint_with_retry("Codex received an unexpected IDE context response.")
            }
            IdeContextError::RequestFailed(_) => hint_with_retry(IDE_DID_NOT_PROVIDE_CONTEXT_HINT),
            IdeContextError::Read(_) => hint_with_retry("Codex could not read IDE context."),
        }
    }
}

fn hint_with_retry(message: &str) -> String {
    format!("{message} {KEEP_TRYING_HINT}")
}

type IdeContextStream = super::windows_pipe::WindowsPipeStream;

pub(crate) fn fetch_ide_context(workspace_root: &Path) -> Result<IdeContext, IdeContextError> {
    fetch_ide_context_from_socket(
        default_ipc_socket_path(),
        workspace_root,
        IDE_CONTEXT_REQUEST_TIMEOUT,
    )
}

fn default_ipc_socket_path() -> PathBuf {
    PathBuf::from(r"\\.\pipe\codex-ipc")
}

fn fetch_ide_context_from_socket(
    socket_path: PathBuf,
    workspace_root: &Path,
    timeout: Duration,
) -> Result<IdeContext, IdeContextError> {
    let deadline = Instant::now() + timeout;
    let mut stream = connect_stream(socket_path, deadline)?;
    fetch_ide_context_from_stream(&mut stream, workspace_root, deadline)
}

fn connect_stream(
    socket_path: PathBuf,
    deadline: Instant,
) -> Result<IdeContextStream, IdeContextError> {
    super::windows_pipe::WindowsPipeStream::connect(socket_path, deadline)
        .map_err(IdeContextError::Connect)
}

fn answer_unsupported_request<T: std::io::Write + ?Sized>(
    stream: &mut T,
    message: &Value,
) -> Result<(), IdeContextError> {
    if let Some(inbound_request_id) = message.get("requestId").and_then(Value::as_str) {
        let response = json!({
            "type": "response",
            "requestId": inbound_request_id,
            "resultType": "error",
            "error": "no-handler-for-request",
        });
        write_frame(stream, &response).map_err(IdeContextError::Send)?;
    }
    Ok(())
}

fn fetch_ide_context_from_stream(
    stream: &mut IdeContextStream,
    workspace_root: &Path,
    deadline: Instant,
) -> Result<IdeContext, IdeContextError> {
    let request_id = uuid::Uuid::new_v4().to_string();
    write_ide_context_request(stream, &request_id, workspace_root)
        .map_err(IdeContextError::Send)?;
    let response = read_response_frame(stream, &request_id, deadline)?;
    extract_ide_context(response)
}

fn write_ide_context_request<T: std::io::Write + ?Sized>(
    stream: &mut T,
    request_id: &str,
    workspace_root: &Path,
) -> std::io::Result<()> {
    let ide_context_request = json!({
        "type": "request",
        "requestId": request_id,
        "sourceClientId": TUI_SOURCE_CLIENT_ID,
        "version": 0,
        "method": "ide-context",
        "params": {
            "workspaceRoot": workspace_root.to_string_lossy(),
        },
    });
    write_frame(stream, &ide_context_request)
}

fn write_frame<T: std::io::Write + ?Sized>(stream: &mut T, message: &Value) -> std::io::Result<()> {
    let payload = serde_json::to_vec(message).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid IDE context JSON message: {err}"),
        )
    })?;
    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "IDE context payload exceeds u32 length",
        )
    })?;
    stream.write_all(&payload_len.to_le_bytes())?;
    stream.write_all(&payload)?;
    stream.flush()
}

fn read_frame<T: std::io::Read + ?Sized>(
    stream: &mut T,
    deadline: Instant,
) -> Result<Value, IdeContextError> {
    let mut len_bytes = [0_u8; 4];
    read_exact_before_deadline(stream, &mut len_bytes, deadline)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_IPC_FRAME_BYTES {
        return Err(IdeContextError::ResponseTooLarge);
    }

    let mut payload = vec![0_u8; len];
    read_exact_before_deadline(stream, &mut payload, deadline)?;
    serde_json::from_slice(&payload)
        .map_err(|err| IdeContextError::InvalidResponse(format!("invalid JSON payload: {err}")))
}

fn read_exact_before_deadline<T: std::io::Read + ?Sized>(
    stream: &mut T,
    buf: &mut [u8],
    deadline: Instant,
) -> Result<(), IdeContextError> {
    // std::io::Read::read_exact has no way to observe our request deadline between partial reads.
    // Keep the frame header and payload under the same budget as the surrounding response wait.
    let mut read_so_far = 0;
    while read_so_far < buf.len() {
        ensure_deadline_not_expired(deadline)?;
        match stream.read(&mut buf[read_so_far..]) {
            Ok(0) => {
                return Err(IdeContextError::Read(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "failed to fill whole IDE context frame",
                )));
            }
            Ok(bytes_read) => {
                read_so_far += bytes_read;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(IdeContextError::Read(error)),
        }
    }

    ensure_deadline_not_expired(deadline)
}

fn read_response_frame(
    stream: &mut IdeContextStream,
    request_id: &str,
    deadline: Instant,
) -> Result<Value, IdeContextError> {
    loop {
        ensure_deadline_not_expired(deadline)?;
        stream.set_deadline(deadline);
        let message = read_frame(stream, deadline)?;
        match message.get("type").and_then(Value::as_str) {
            Some("response") => {
                if message.get("requestId").and_then(Value::as_str) == Some(request_id) {
                    return Ok(message);
                }
            }
            Some("broadcast") => {}
            Some("client-discovery-request") => {
                if let Some(discovery_request_id) = message.get("requestId").and_then(Value::as_str)
                {
                    let response = json!({
                        "type": "client-discovery-response",
                        "requestId": discovery_request_id,
                        "response": {
                            "canHandle": false,
                        },
                    });
                    write_frame(stream, &response).map_err(IdeContextError::Send)?;
                }
            }
            Some("client-discovery-response") => {}
            Some("request") => {
                answer_unsupported_request(stream, &message)?;
            }
            Some(other) => {
                return Err(IdeContextError::InvalidResponse(format!(
                    "unexpected IDE context message type: {other}"
                )));
            }
            None => {
                return Err(IdeContextError::InvalidResponse(
                    "IDE context message did not include a type".to_string(),
                ));
            }
        }
    }
}

fn ensure_deadline_not_expired(deadline: Instant) -> Result<(), IdeContextError> {
    if Instant::now() >= deadline {
        return Err(timeout_error());
    }

    Ok(())
}

fn timeout_error() -> IdeContextError {
    IdeContextError::Read(deadline_timeout_io_error())
}

fn deadline_timeout_io_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "timed out waiting for IDE context",
    )
}

fn extract_ide_context(response: Value) -> Result<IdeContext, IdeContextError> {
    ensure_success_response(&response)?;
    let ide_context = response
        .get("result")
        .and_then(|result| result.get("ideContext"))
        .cloned()
        .ok_or_else(|| {
            IdeContextError::InvalidResponse(
                "ide-context response did not include result.ideContext".to_string(),
            )
        })?;
    serde_json::from_value(ide_context)
        .map_err(|err| IdeContextError::InvalidResponse(err.to_string()))
}

fn ensure_success_response(response: &Value) -> Result<(), IdeContextError> {
    match response.get("resultType").and_then(Value::as_str) {
        Some("success") => Ok(()),
        Some("error") => Err(IdeContextError::RequestFailed(
            response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string(),
        )),
        _ => Err(IdeContextError::InvalidResponse(
            "response did not include a success or error resultType".to_string(),
        )),
    }
}
