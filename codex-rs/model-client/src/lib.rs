//! Session-scoped model request and transport lifecycle, independent of core orchestration.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![recursion_limit = "256"]
pub mod account_identity;
pub mod attestation;
pub mod client;
pub mod client_common;
pub mod image_preparation;
pub mod responses_metadata;
pub use attestation::{AttestationContext, AttestationProvider, GenerateAttestationFuture};
pub use client::{ModelClient, ModelClientSession};
pub use client_common::{Prompt, ResponseEvent, ResponseStream};
pub use codex_feedback::feedback_tags;
pub use responses_metadata::CodexResponsesMetadata;
