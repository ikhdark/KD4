use codex_app_server_protocol_types::JSONRPCErrorError;

pub(crate) const INVALID_REQUEST_ERROR_CODE: i64 = -32600;
pub(crate) const METHOD_NOT_FOUND_ERROR_CODE: i64 = -32601;
pub const INVALID_PARAMS_ERROR_CODE: i64 = -32602;
pub const REMOTE_CONTROL_TEMPORARY_ERROR_CODE: i64 = -32002;
pub const FS_WATCH_CAPACITY_ERROR_CODE: i64 = -32003;
pub(crate) const INTERNAL_ERROR_CODE: i64 = -32603;
pub const INPUT_TOO_LARGE_ERROR_CODE: &str = "input_too_large";

pub fn invalid_request(message: impl Into<String>) -> JSONRPCErrorError {
    error(INVALID_REQUEST_ERROR_CODE, message)
}

pub fn method_not_found(message: impl Into<String>) -> JSONRPCErrorError {
    error(METHOD_NOT_FOUND_ERROR_CODE, message)
}

pub fn invalid_params(message: impl Into<String>) -> JSONRPCErrorError {
    error(INVALID_PARAMS_ERROR_CODE, message)
}

pub fn internal_error(message: impl Into<String>) -> JSONRPCErrorError {
    error(INTERNAL_ERROR_CODE, message)
}

/// Admission refusal for a bounded live registry (fuzzy search sessions,
/// `process/spawn`): the `fs/watch` capacity code with the registry `reason`
/// and the limiting `scope` (`"server"` or `"connection"`). Releasing an
/// existing entry restores capacity, so the refusal is not blindly retryable.
pub fn capacity_error(
    reason: &'static str,
    scope: &'static str,
    message: impl Into<String>,
) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: FS_WATCH_CAPACITY_ERROR_CODE,
        message: message.into(),
        data: Some(serde_json::json!({
            "reason": reason,
            "scope": scope,
            "retryable": false,
        })),
    }
}

fn error(code: i64, message: impl Into<String>) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code,
        message: message.into(),
        data: None,
    }
}
