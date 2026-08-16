use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

/// Stable category used to route a tool failure without parsing its prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureClass {
    InvalidPayload,
    UnsupportedTool,
    MissingOutput,
    ArtifactRetention,
    ToolExecution,
    Compiler,
    Test,
    Runtime,
    Completion,
}

/// Model-visible diagnostic that retains a safe failure identity and recovery route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFailureDiagnostic {
    pub class: ToolFailureClass,
    pub fingerprint: String,
    pub retryable: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    #[serde(skip)]
    fatal: bool,
}

impl ToolFailureDiagnostic {
    pub fn fatal(
        class: ToolFailureClass,
        fingerprint: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            class,
            fingerprint: fingerprint.into(),
            retryable: false,
            message: message.into(),
            owner_hint: None,
            next_action: None,
            fatal: true,
        }
    }

    pub fn model_visible(
        class: ToolFailureClass,
        fingerprint: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            fatal: false,
            ..Self::fatal(class, fingerprint, message)
        }
    }

    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_owner_hint(mut self, owner_hint: impl Into<String>) -> Self {
        self.owner_hint = Some(owner_hint.into());
        self
    }

    pub fn with_next_action(mut self, next_action: impl Into<String>) -> Self {
        self.next_action = Some(next_action.into());
        self
    }

    pub fn is_fatal(&self) -> bool {
        self.fatal
    }
}

impl std::fmt::Display for ToolFailureDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let json = serde_json::to_string(self).map_err(|_| std::fmt::Error)?;
        formatter.write_str(&json)
    }
}

/// Error returned while executing a model-visible tool invocation.
#[derive(Debug, Error, PartialEq)]
pub enum FunctionCallError {
    #[error("{0}")]
    RespondToModel(String),
    #[error("Fatal error: {0}")]
    Fatal(String),
    #[error("{0}")]
    Diagnostic(ToolFailureDiagnostic),
}

impl FunctionCallError {
    pub fn diagnostic(&self) -> Option<&ToolFailureDiagnostic> {
        match self {
            Self::Diagnostic(diagnostic) => Some(diagnostic),
            Self::RespondToModel(_) | Self::Fatal(_) => None,
        }
    }

    pub fn fingerprint(&self) -> Option<&str> {
        self.diagnostic()
            .map(|diagnostic| diagnostic.fingerprint.as_str())
    }

    pub fn is_fatal(&self) -> bool {
        match self {
            Self::Fatal(_) => true,
            Self::Diagnostic(diagnostic) => diagnostic.is_fatal(),
            Self::RespondToModel(_) => false,
        }
    }

    pub fn into_fatal_message(self) -> String {
        match self {
            Self::Fatal(message) => message,
            Self::Diagnostic(diagnostic) => diagnostic.to_string(),
            Self::RespondToModel(message) => message,
        }
    }
}

#[cfg(test)]
#[path = "function_call_error_tests.rs"]
mod tests;
