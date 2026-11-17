//! JSON-RPC conversions shared by the exec-server filesystem handlers.
//!
//! The former out-of-process filesystem helper existed only to run operations
//! inside the removed sandbox; KD4 performs filesystem operations directly.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use codex_exec_server_protocol::JSONRPCErrorError;
use tokio::io;

use crate::protocol::FS_MUTATE_FILE_IF_MATCHES_METHOD;
use crate::rpc::internal_error;
use crate::rpc::invalid_request;
use crate::rpc::not_found;

pub(crate) const FS_PERMISSION_DENIED_ERROR_CODE: i64 = -32003;

/// Decoded expected preimage and replacement bytes of a conditional mutation.
type MutationPayloads = (Option<Vec<u8>>, Option<Vec<u8>>);

/// Decodes the optional preimage and replacement of a conditional mutation.
pub(crate) fn decode_mutation_payloads(
    expected_base64: Option<String>,
    replacement_base64: Option<String>,
) -> Result<MutationPayloads, JSONRPCErrorError> {
    let decode = |field: &str, data: Option<String>| {
        data.map(|data| STANDARD.decode(data))
            .transpose()
            .map_err(|err| {
                invalid_request(format!(
                    "{FS_MUTATE_FILE_IF_MATCHES_METHOD} requires valid base64 {field}: {err}"
                ))
            })
    };
    Ok((
        decode("expectedBase64", expected_base64)?,
        decode("replacementBase64", replacement_base64)?,
    ))
}

pub(crate) fn map_fs_error(err: io::Error) -> JSONRPCErrorError {
    let mut error = match err.kind() {
        io::ErrorKind::NotFound => not_found(err.to_string()),
        io::ErrorKind::InvalidInput => invalid_request(err.to_string()),
        io::ErrorKind::PermissionDenied => JSONRPCErrorError {
            code: FS_PERMISSION_DENIED_ERROR_CODE,
            data: None,
            message: err.to_string(),
        },
        _ => internal_error(err.to_string()),
    };
    let mut data = codex_exec_server_protocol::file_system_error_data(err.kind());
    // `CopyFailure` holds only integers, booleans and strings, so it serializes;
    // the error message already carries the same recovery text regardless.
    if let Some(copy) = codex_file_system::CopyFailure::from_io_error(&err)
        && let Ok(copy) = serde_json::to_value(copy)
    {
        data["copy"] = copy;
    }
    error.data = Some(data);
    error
}
