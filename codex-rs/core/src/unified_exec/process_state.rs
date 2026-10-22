#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProcessState {
    pub(crate) has_exited: bool,
    pub(crate) exit_code: Option<i32>,
    pub(crate) exit_observed_at: Option<std::time::Instant>,
    pub(crate) failure_message: Option<String>,
    pub(crate) sandbox_denied: bool,
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
        }
    }

    pub(crate) fn failed(&self, message: String) -> Self {
        Self {
            has_exited: self.has_exited,
            exit_code: self.exit_code,
            exit_observed_at: self.exit_observed_at,
            failure_message: Some(message),
            sandbox_denied: self.sandbox_denied,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProcessState;
    #[test]
    fn audit_observation_failure_does_not_establish_exit() {
        for sandbox_denied in [false, true] {
            let initial = ProcessState {
                sandbox_denied,
                ..ProcessState::default()
            };
            let failed = initial.failed("transport lost".into());
            assert_eq!(
                failed,
                ProcessState {
                    failure_message: Some("transport lost".into()),
                    ..initial.clone()
                }
            );
            for exit_code in [None, Some(0), Some(17)] {
                let exited = failed.exited(exit_code);
                assert_eq!(
                    exited,
                    ProcessState {
                        has_exited: true,
                        exit_code,
                        exit_observed_at: exited.exit_observed_at,
                        ..failed.clone()
                    }
                );
                assert!(exited.exit_observed_at.is_some());
                assert_eq!(
                    exited.failed("later error".into()),
                    ProcessState {
                        failure_message: Some("later error".into()),
                        ..exited.clone()
                    }
                );
            }
        }
    }
}
