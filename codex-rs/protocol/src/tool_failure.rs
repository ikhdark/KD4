use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::fmt;

/// Stable failure categories, independent of human-readable diagnostics.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureCode {
    InvalidInput,
    InvalidSyntax,
    StaleInput,
    MissingAnchor,
    AmbiguousAnchor,
    PermissionDenied,
    RetryableFailure,
    TimedOut,
    Cancelled,
    UncertainEffects,
    Failed,
    RequiredOperationBlocked,
}

/// Settlement of the tool invocation, independent of external effects or child
/// process lifetime. A missing legacy state or lost response remains unknown.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionState {
    NotStarted,
    Completed,
    #[default]
    Unknown,
}

/// Certainty about this invocation, not a claim that cancellation rolled it back.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffectsState {
    None,
    Reported,
    Unknown,
}

/// Owner-certified dispatch settlement, separate from housekeeping effects.
/// Only the admission owner may prove that no requested action was admitted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDispatchState {
    NotAdmitted,
}

/// An existing owner-issued continuation, never a replacement launch request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolOperationHandle {
    pub tool: String,
    pub arguments: Value,
}

/// A rejected argument location, using an RFC 6901 pointer (empty for the root).
/// Expected values come from the public contract, never from rejected input.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolFieldError {
    pub path: String,
    pub expected: String,
    pub correction: String,
}

/// Shared direct/Code Mode failure payload. Codes do not authorize retries;
/// callers must also inspect effect certainty, actions and live operations.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolFailure {
    pub code: ToolFailureCode,
    pub message: String,
    #[serde(default)]
    pub execution_state: ToolExecutionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_state: Option<ToolDispatchState>,
    pub effects: ToolEffectsState,
    pub retry_safe: bool,
    pub safe_next_actions: Vec<String>,
    pub live_operations: Vec<ToolOperationHandle>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_errors: Vec<ToolFieldError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ToolFailure {
    pub fn new(code: ToolFailureCode, message: impl Into<String>) -> Self {
        let action = match code {
            ToolFailureCode::InvalidInput => "Correct the arguments using the advertised contract; inspect any earlier effects before submitting again.",
            ToolFailureCode::InvalidSyntax => "Correct the syntax at the reported location using the advertised grammar; do not replay earlier operations.",
            ToolFailureCode::StaleInput => "Recover the current source/revision or the originating operation receipt, then reconcile the request; do not replay stale input.",
            ToolFailureCode::MissingAnchor => "Inspect the reported source region and reconcile the anchor and contiguous old lines; submit only the remaining intended changes.",
            ToolFailureCode::AmbiguousAnchor => "Select the intended location with unique contiguous context or a current revision-bound range; do not guess among matches.",
            ToolFailureCode::PermissionDenied => "Respect the denial; continue independent permitted work. Do not retry through another route or escalate without authorization.",
            ToolFailureCode::RetryableFailure => "Inspect effect certainty and live operations before retrying; this code alone does not establish that another execution is safe or authorized.",
            ToolFailureCode::TimedOut => "The observation deadline expired; this does not establish process exit or termination. Preserve captured output and unknown exit status, and resume any retained live operation before considering a retry.",
            ToolFailureCode::Cancelled => "Cancellation is not rollback. Inspect retained results and live operations before retrying any unfinished work.",
            ToolFailureCode::RequiredOperationBlocked => "The required operation is blocked; preserve completed work and resolve the reported blocker before continuing.",
            ToolFailureCode::UncertainEffects | ToolFailureCode::Failed => "Inspect retained results and external state; do not automatically replay the request.",
        };
        Self {
            code,
            message: message.into(),
            execution_state: ToolExecutionState::Unknown,
            dispatch_state: None,
            effects: ToolEffectsState::Unknown,
            retry_safe: false,
            safe_next_actions: vec![action.to_string()],
            live_operations: Vec::new(),
            field_errors: Vec::new(),
            details: None,
        }
    }

    /// Only the effect owner may establish that execution never started.
    pub fn no_effects(code: ToolFailureCode, message: impl Into<String>) -> Self {
        Self {
            execution_state: ToolExecutionState::NotStarted,
            effects: ToolEffectsState::None,
            retry_safe: code == ToolFailureCode::RetryableFailure,
            ..Self::new(code, message)
        }
    }

    /// A handler's own refusal of arguments its decoder accepted, decided
    /// before it started anything. `kind` names the refusing check. The marker
    /// tells a direct-call dispatcher that this reason is not a schema
    /// rejection to re-explain.
    pub fn handler_refusal(code: ToolFailureCode, message: impl Into<String>, kind: &str) -> Self {
        let mut failure = Self::no_effects(code, message);
        failure.details = Some(serde_json::json!({"handler_refusal": kind}));
        failure
    }

    pub fn is_handler_refusal(&self) -> bool {
        self.details
            .as_ref()
            .is_some_and(|details| details.get("handler_refusal").is_some())
    }

    /// Only an actual owner result establishes invocation completion. This is
    /// neither process-exit evidence nor proof that external effects are known.
    pub fn with_completed_execution(mut self) -> Self {
        self.execution_state = ToolExecutionState::Completed;
        self.retry_safe = false;
        self
    }

    pub fn invalid_json(line: usize, column: usize) -> Self {
        let mut failure = Self::no_effects(ToolFailureCode::InvalidSyntax,
            format!("Arguments rejected before execution: invalid JSON at line {line}, column {column}."));
        failure.field_errors.push(ToolFieldError {
            path: String::new(), expected: "a valid JSON object".into(),
            correction: "Correct JSON syntax at the reported line and column.".into(),
        });
        failure.details = Some(serde_json::json!({"line": line, "column": column}));
        failure
    }

    pub fn model_message(&self) -> String {
        serde_json::json!({"tool_failure": self}).to_string()
    }
}

impl fmt::Display for ToolFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}
