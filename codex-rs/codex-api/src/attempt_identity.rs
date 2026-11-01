#![deny(clippy::print_stdout, clippy::print_stderr)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseAttemptIdentity {
    pub sampling_request_id: String,
    pub physical_attempt_id: String,
}
