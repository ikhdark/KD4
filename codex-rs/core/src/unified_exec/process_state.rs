#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProcessState {
    pub(crate) has_exited: bool,
    pub(crate) exit_code: Option<i32>,
    pub(crate) failure_message: Option<String>,
    pub(crate) sandbox_denied: bool,
}

impl ProcessState {
    pub(crate) fn exited(&self, exit_code: Option<i32>) -> Self {
        Self {
            has_exited: true,
            exit_code,
            failure_message: self.failure_message.clone(),
            sandbox_denied: self.sandbox_denied,
        }
    }

    pub(crate) fn failed(&self, message: String) -> Self {
        Self {
            has_exited: self.has_exited,
            exit_code: self.exit_code,
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
        let failed = ProcessState::default().failed("transport lost".into());
        assert!(!failed.has_exited);
        assert_eq!(failed.failure_message.as_deref(), Some("transport lost"));
        assert!(failed.exited(Some(0)).has_exited);
        assert!(
            failed
                .exited(Some(0))
                .failed("later error".into())
                .has_exited
        );
    }
}
