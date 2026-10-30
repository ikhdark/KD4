use codex_protocol::tool_failure::ToolFailure;
use codex_protocol::tool_failure::ToolFailureCode;
use thiserror::Error;

/// Error returned while executing a model-visible tool invocation.
#[derive(Debug, Error, PartialEq)]
pub enum FunctionCallError {
    #[error("{0}")]
    Structured(Box<ToolFailure>),
    #[error("{0}")]
    RespondToModel(String),
    /// A model-visible refusal of this operation. Refusal does not establish
    /// that the operation is required to finish the entire turn. Keep the
    /// distinction from execution failure structured through relay.
    #[error("{0}")]
    DeniedToModel(String),
    /// The runtime has established that the turn cannot proceed, for example
    /// because its assignment was revoked. Unlike call-local denial, this is terminal.
    #[error("{0}")]
    RequiredOperationBlocked(String),
    #[error("Fatal error: {0}")]
    Fatal(String),
}

impl FunctionCallError {
    pub fn structured(failure: ToolFailure) -> Self {
        Self::Structured(Box::new(failure))
    }

    /// Preserve legacy diagnostics without guessing effect certainty from text.
    pub fn failure(&self) -> ToolFailure {
        match self {
            Self::Structured(failure) => (**failure).clone(),
            Self::RespondToModel(message) => ToolFailure::new(ToolFailureCode::Failed, message),
            Self::DeniedToModel(message) => {
                ToolFailure::new(ToolFailureCode::PermissionDenied, message)
            }
            Self::RequiredOperationBlocked(message) => {
                ToolFailure::new(ToolFailureCode::RequiredOperationBlocked, message)
            }
            Self::Fatal(message) => ToolFailure::new(ToolFailureCode::UncertainEffects, message),
        }
    }

    pub fn model_message(&self) -> String {
        self.failure().model_message()
    }
}
