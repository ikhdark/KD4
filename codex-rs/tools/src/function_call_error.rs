use thiserror::Error;

/// Error returned while executing a model-visible tool invocation.
#[derive(Debug, Error, PartialEq)]
pub enum FunctionCallError {
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
