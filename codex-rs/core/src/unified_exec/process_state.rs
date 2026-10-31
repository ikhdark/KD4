#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProcessState {
    pub(crate) has_exited: bool,
    pub(crate) exit_code: Option<i32>,
    pub(crate) exit_observed_at: Option<std::time::Instant>,
    pub(crate) failure_message: Option<String>,
    pub(crate) sandbox_denied: bool,
    pub(crate) shutdown: Option<crate::tools::context::ExecShutdownStatus>,
    pub(crate) shutdown_error: Option<String>,
}

impl ProcessState {
    pub(crate) fn exited(&self, exit_code: Option<i32>) -> Self {
        Self {
            has_exited: true,
            exit_code,
            exit_observed_at: Some(
                self.exit_observed_at
                    .unwrap_or_else(std::time::Instant::now),
            ),
            failure_message: self.failure_message.clone(),
            sandbox_denied: self.sandbox_denied,
            shutdown: self.shutdown,
            shutdown_error: self.shutdown_error.clone(),
        }
    }

    pub(crate) fn failed(&self, message: String) -> Self {
        Self {
            has_exited: self.has_exited,
            exit_code: self.exit_code,
            exit_observed_at: self.exit_observed_at,
            failure_message: Some(message),
            sandbox_denied: self.sandbox_denied,
            shutdown: self.shutdown,
            shutdown_error: self.shutdown_error.clone(),
        }
    }
}

