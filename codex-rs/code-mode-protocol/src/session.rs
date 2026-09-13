use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::CodeModeNestedToolCall;
use crate::ExecuteRequest;
use crate::RuntimeResponse;
use crate::WaitOutcome;
use crate::WaitRequest;

pub type CodeModeSessionResultFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;
pub type CodeModeSessionProviderFuture<'a> =
    CodeModeSessionResultFuture<'a, Arc<dyn CodeModeSession>>;
pub type ToolInvocationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<JsonValue, String>> + Send + 'a>>;
pub type NotificationFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct CellId(String);

impl CellId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for CellId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for CellId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A session-owned cell and its one-shot initial response receiver.
///
/// Dropping this handle or its response future only drops the receiver; it does
/// not request termination. Retain `cell_id` before consuming the handle when
/// recovery may require `wait` or `terminate`. The session remains responsible
/// for the cell until completion, termination, or session shutdown.
pub struct StartedCell {
    pub cell_id: CellId,
    initial_response: CodeModeSessionResultFuture<'static, RuntimeResponse>,
}

impl StartedCell {
    pub fn new(cell_id: CellId, initial_response_rx: oneshot::Receiver<RuntimeResponse>) -> Self {
        Self {
            cell_id,
            initial_response: Box::pin(async move {
                initial_response_rx
                    .await
                    .map_err(|_| "exec runtime ended unexpectedly".to_string())
            }),
        }
    }

    pub fn from_result_receiver(
        cell_id: CellId,
        initial_response_rx: oneshot::Receiver<Result<RuntimeResponse, String>>,
    ) -> Self {
        Self {
            cell_id,
            initial_response: Box::pin(async move {
                initial_response_rx
                    .await
                    .map_err(|_| "exec runtime ended unexpectedly".to_string())?
            }),
        }
    }

    /// An error reports response-delivery or runtime failure, not proof that the
    /// cell never ran. Do not replay side-effecting work based on this error alone.
    pub async fn initial_response(self) -> Result<RuntimeResponse, String> {
        self.initial_response.await
    }
}

/// Host callbacks used by a code-mode session while cells are executing.
pub trait CodeModeSessionDelegate: Send + Sync {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a>;

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a>;

    /// Releases delegate state associated with a cell after it reaches a terminal state.
    ///
    /// This is not a join of all delegate futures: remote callbacks may still be
    /// unwinding after their cancellation tokens and response delivery are revoked.
    /// Cleanup must tolerate those late callbacks without recreating live cell state.
    fn cell_closed(&self, cell_id: &CellId);
}

/// A durable code-mode session owned by one Codex thread.
///
/// Cells executed in the same session share stored values. Separate sessions
/// must keep those values isolated. Implementations may execute cells
/// in-process or remotely.
///
/// The session owns admitted work independently of the futures observing it.
/// Dropping an operation future or receiving an error does not establish that
/// execution had no effects. Callers retain returned cell IDs for recovery and
/// use `shutdown` when abandoning the session, including after a start whose
/// outcome is unknown.
pub trait CodeModeSession: Send + Sync {
    /// Starts a cell and returns its identity before the initial response.
    /// Cancellation before this returns can race with admission; it is not a
    /// portable guarantee that the cell never started.
    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
    ) -> CodeModeSessionResultFuture<'a, StartedCell>;

    /// Observes an existing cell. Dropping the observation does not terminate it
    /// or guarantee that output already consumed by that observation is replayed.
    fn wait<'a>(&'a self, request: WaitRequest) -> CodeModeSessionResultFuture<'a, WaitOutcome>;

    /// Requests termination; dropping this future is not confirmation of cleanup.
    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome>;

    /// Shuts down session-owned work. Await this when relinquishing ownership;
    /// dropping the future alone does not establish that cleanup has finished.
    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()>;
}

/// Creates code-mode sessions for Codex threads.
///
/// Implementations may share a remote host process across all sessions created
/// by one provider.
pub trait CodeModeSessionProvider: Send + Sync {
    fn create_session<'a>(
        &'a self,
        delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> CodeModeSessionProviderFuture<'a>;
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
