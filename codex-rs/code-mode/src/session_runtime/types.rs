use std::fmt;
use std::future::Future;
use std::time::Duration;

use codex_code_mode_protocol::NestedCancellation;
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

/// Identifies one execution cell within a session runtime.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CellId(String);

impl CellId {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CellId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Selects the next observable frontier for a running cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObserveMode {
    YieldAfter(Duration),
    /// Wake on model-visible output or terminal completion without periodic
    /// empty observations.
    StateChange,
    /// Buffer output while the script owns its continuation. Explicit yields
    /// and terminal outcomes still release the observer, as does ten minutes
    /// without output or a nested-call completion. The cell remains resumable.
    Decision,
}

/// An observable cell lifecycle event.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) enum CellEvent {
    Yielded {
        content_items: Vec<OutputItem>,
    },
    /// The cell explicitly requested a new model decision with
    /// `yield_control()`. Unlike a timer-driven yield, owners must not drain
    /// this event internally.
    ExplicitYield {
        content_items: Vec<OutputItem>,
    },
    Completed {
        content_items: Vec<OutputItem>,
        error_text: Option<String>,
        output_loss: Option<codex_code_mode_protocol::OutputLoss>,
    },
    Terminated {
        content_items: Vec<OutputItem>,
    },
}

/// Output emitted by a cell since its preceding observation.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) enum OutputItem {
    Text {
        text: String,
    },
    Image {
        image_url: String,
        detail: Option<ImageDetail>,
    },
}

/// Requested image fidelity for an output image.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) enum ImageDetail {
    Auto,
    Low,
    High,
    Original,
}

/// Transport-neutral input for creating a cell.
///
/// The owning session assigns the cell ID when it admits the request.
pub(crate) struct CreateCellRequest {
    pub(crate) state_path: Option<std::path::PathBuf>,
    pub(crate) tool_call_id: String,
    pub(crate) enabled_tools: std::sync::Arc<[codex_code_mode_protocol::ToolDefinition]>,
    pub(crate) source: String,
    pub(crate) default_tool_timeout_ms: u64,
}

/// A tool name with an optional namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolName {
    pub(crate) name: String,
    pub(crate) namespace: Option<String>,
}

/// The JavaScript calling convention for a tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolKind {
    Function,
    Freeform,
}

/// A nested tool request emitted by a running cell.
pub(crate) struct NestedToolCall {
    pub(crate) cell_id: CellId,
    pub(crate) parent_tool_call_id: String,
    pub(crate) runtime_tool_call_id: String,
    pub(crate) buffered_output_bytes: usize,
    pub(crate) tool_name: ToolName,
    pub(crate) tool_kind: ToolKind,
    pub(crate) input: Option<JsonValue>,
    /// Instant the cell's wrapper timeout fires for this call.
    pub(crate) nested_deadline: Option<std::time::Instant>,
}

/// Host callbacks used by cells owned by a [`super::SessionRuntime`].
///
/// Implementations must honor cancellation tokens. `cell_closed` is called
/// after the runtime has stopped routing requests to the cell.
pub(crate) trait SessionRuntimeDelegate: Send + Sync + 'static {
    fn invoke_tool(
        &self,
        invocation: NestedToolCall,
        cancellation: NestedCancellation,
    ) -> impl Future<Output = Result<JsonValue, String>> + Send;

    fn notify(
        &self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> impl Future<Output = Result<(), String>> + Send;

    fn cell_closed(&self, cell_id: &CellId);
}

/// A failure reported by a session runtime operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    ShuttingDown,
    /// Carries the registered cells that hold permits, so callers can wait on one.
    ActiveCellLimit(Vec<CellId>),
    CellIdSpaceExhausted,
    DuplicateCell(CellId),
    MissingCell(CellId),
    ExpiredResult { cell_id: CellId, completed: bool },
    BusyObserver(CellId),
    AlreadyTerminating(CellId),
    ClosedCell(CellId),
    Runtime(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShuttingDown => formatter.write_str("code mode session is shutting down"),
            Self::ActiveCellLimit(active) => {
                let active = active.iter().map(|cell_id| serde_json::json!({
                    "cell_id": cell_id.as_str(),
                    "state": "registered",
                    "readiness": "unknown",
                    "permitted_actions": ["wait", "terminate"],
                })).collect::<Vec<_>>();
                write!(formatter, "{}", serde_json::json!({
                    "kind": "code_mode_admission_failure",
                    "version": 1,
                    "reason": "active_cell_limit",
                    "message": "code mode has reached its active cell limit; wait for a known dependency or explicitly terminate an unwanted cell before starting another exec",
                    "started": false,
                    "active_cells": active,
                    "preparing_cells_may_be_unlisted": true,
                    "retry_after": "a cell releases its permit",
                }))
            }
            Self::CellIdSpaceExhausted => {
                formatter.write_str("code mode session exhausted its cell ID space")
            }
            Self::DuplicateCell(cell_id) => write!(formatter, "exec cell {cell_id} already exists"),
            Self::MissingCell(cell_id) => write!(formatter, "exec cell {cell_id} not found"),
            Self::ExpiredResult { cell_id, completed } => write!(formatter, "{}", serde_json::json!({
                "kind": "exec_result_unavailable",
                "cell_id": cell_id.as_str(),
                "status": "expired_result",
                "terminal_state": if *completed { "completed" } else { "interrupted" },
                "recovery": null,
                "automatic_replay_allowed": false,
                "message": "The terminal result was evicted. No retained result locator is known; inspect previously retained tool receipts and do not replay effects blindly.",
            })),
            Self::BusyObserver(cell_id) => {
                write!(
                    formatter,
                    "exec cell {cell_id} already has an active observer"
                )
            }
            Self::AlreadyTerminating(cell_id) => {
                write!(formatter, "exec cell {cell_id} is already terminating")
            }
            Self::ClosedCell(cell_id) => {
                write!(formatter, "exec cell {cell_id} is closed")
            }
            Self::Runtime(error_text) => formatter.write_str(error_text),
        }
    }
}

impl std::error::Error for Error {}
