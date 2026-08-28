use thiserror::Error;

/// Error returned while executing a model-visible tool invocation.
#[derive(Debug, Error, PartialEq)]
pub enum FunctionCallError {
    #[error("{0}")]
    RespondToModel(String),
    /// A model-visible refusal of a required operation. Keep this structured
    /// through relay so terminal classification can distinguish Blocked from
    /// an execution failure without parsing human-readable text.
    #[error("{0}")]
    DeniedToModel(String),
    #[error("Fatal error: {0}")]
    Fatal(String),
}
