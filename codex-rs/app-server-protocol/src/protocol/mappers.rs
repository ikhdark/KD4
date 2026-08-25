use crate::protocol::v1;
use crate::protocol::v2;
impl From<v1::ExecOneOffCommandParams> for v2::CommandExecParams {
    fn from(value: v1::ExecOneOffCommandParams) -> Self {
        Self {
            command: value.command,
            process_id: None,
            tty: false,
            stream_stdin: false,
            stream_stdout_stderr: false,
            output_bytes_cap: None,
            disable_output_cap: false,
            disable_timeout: false,
            timeout_ms: value
                .timeout_ms
                .map(|timeout| i64::try_from(timeout).unwrap_or(i64::MAX)),
            cwd: value.cwd,
            env: None,
            size: None,
            sandbox_policy: value.sandbox_policy.map(std::convert::Into::into),
            permission_profile: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_timeout_saturates_when_v2_cannot_represent_it() {
        let params = v1::ExecOneOffCommandParams {
            command: vec!["echo".to_string()],
            timeout_ms: Some(u64::MAX),
            cwd: None,
            sandbox_policy: None,
        };

        let mapped: v2::CommandExecParams = params.into();

        assert_eq!(mapped.timeout_ms, Some(i64::MAX));
    }
}
