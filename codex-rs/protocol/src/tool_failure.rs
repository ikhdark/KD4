use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::fmt;

/// Stable failure categories, independent of human-readable diagnostics.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureCode {
    InvalidInput,
    StaleInput,
    PermissionDenied,
    RetryableFailure,
    Cancelled,
    UncertainEffects,
    Failed,
    RequiredOperationBlocked,
}

/// Certainty about this invocation, not a claim that cancellation rolled it back.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffectsState {
    None,
    Reported,
    Unknown,
}

/// An existing owner-issued continuation, never a replacement launch request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolOperationHandle {
    pub tool: String,
    pub arguments: Value,
}

/// Shared direct/Code Mode failure payload. Codes do not authorize retries;
/// callers must also inspect effect certainty, actions and live operations.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolFailure {
    pub code: ToolFailureCode,
    pub message: String,
    pub effects: ToolEffectsState,
    pub retry_safe: bool,
    pub safe_next_actions: Vec<String>,
    pub live_operations: Vec<ToolOperationHandle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ToolFailure {
    pub fn new(code: ToolFailureCode, message: impl Into<String>) -> Self {
        let action = match code {
            ToolFailureCode::InvalidInput => "Correct the arguments using the advertised contract; inspect any earlier effects before submitting again.",
            ToolFailureCode::StaleInput => "Recover the current source/revision or the originating operation receipt, then reconcile the request; do not replay stale input.",
            ToolFailureCode::PermissionDenied => "Respect the denial; continue independent permitted work. Do not retry through another route or escalate without authorization.",
            ToolFailureCode::RetryableFailure => "Inspect effect certainty and live operations before retrying; this code alone does not establish that another execution is safe or authorized.",
            ToolFailureCode::Cancelled => "Cancellation is not rollback. Inspect retained results and live operations before retrying any unfinished work.",
            ToolFailureCode::RequiredOperationBlocked => "The required operation is blocked; preserve completed work and resolve the reported blocker before continuing.",
            ToolFailureCode::UncertainEffects | ToolFailureCode::Failed => "Inspect retained results and external state; do not automatically replay the request.",
        };
        Self {
            code,
            message: message.into(),
            effects: ToolEffectsState::Unknown,
            retry_safe: false,
            safe_next_actions: vec![action.to_string()],
            live_operations: Vec::new(),
            details: None,
        }
    }

    /// Only the effect owner may establish that execution never started.
    pub fn no_effects(code: ToolFailureCode, message: impl Into<String>) -> Self {
        Self {
            effects: ToolEffectsState::None,
            retry_safe: code == ToolFailureCode::RetryableFailure,
            ..Self::new(code, message)
        }
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
