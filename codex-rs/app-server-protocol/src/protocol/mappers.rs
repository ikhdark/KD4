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
        for (timeout_ms, expected) in [
            (None, None),
            (Some(0), Some(0)),
            (Some(123), Some(123)),
            (Some(i64::MAX as u64), Some(i64::MAX)),
            (Some(i64::MAX as u64 + 1), Some(i64::MAX)),
            (Some(u64::MAX), Some(i64::MAX)),
        ] {
            let params = v1::ExecOneOffCommandParams {
                command: vec!["echo".to_string(), "hello".to_string()],
                timeout_ms,
                cwd: Some(std::path::PathBuf::from("workdir")),
                sandbox_policy: None,
            };
            let mapped: v2::CommandExecParams = params.into();
            assert_eq!(mapped.timeout_ms, expected, "{timeout_ms:?}");
            assert_eq!(mapped.command, ["echo", "hello"]);
            assert_eq!(mapped.cwd.as_deref(), Some(std::path::Path::new("workdir")));
            assert!(!mapped.disable_timeout);
        }
    }
}
