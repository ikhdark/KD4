use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use codex_exec_server_protocol::JSONRPCErrorError;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;
use tokio::io;

use crate::CopyOptions;
use crate::CreateDirectoryOptions;
use crate::ExecutorFileSystem;
use crate::RemoveOptions;
use crate::local_file_system::DirectFileSystem;
use crate::protocol::FS_CANONICALIZE_METHOD;
use crate::protocol::FS_COPY_METHOD;
use crate::protocol::FS_CREATE_DIRECTORY_METHOD;
use crate::protocol::FS_GET_METADATA_METHOD;
use crate::protocol::FS_MUTATE_FILE_IF_MATCHES_METHOD;
use crate::protocol::FS_READ_DIRECTORY_METHOD;
use crate::protocol::FS_READ_FILE_BOUNDED_METHOD;
use crate::protocol::FS_READ_FILE_METHOD;
use crate::protocol::FS_REMOVE_METHOD;
use crate::protocol::FS_WALK_METHOD;
use crate::protocol::FS_WRITE_FILE_METHOD;
use crate::protocol::FsCanonicalizeParams;
use crate::protocol::FsCanonicalizeResponse;
use crate::protocol::FsCopyParams;
use crate::protocol::FsCopyResponse;
use crate::protocol::FsCreateDirectoryParams;
use crate::protocol::FsCreateDirectoryResponse;
use crate::protocol::FsGetMetadataParams;
use crate::protocol::FsGetMetadataResponse;
use crate::protocol::FsMutateFileIfMatchesParams;
use crate::protocol::FsMutateFileIfMatchesResponse;
use crate::protocol::FsReadDirectoryEntry;
use crate::protocol::FsReadDirectoryParams;
use crate::protocol::FsReadDirectoryResponse;
use crate::protocol::FsReadFileBoundedParams;
use crate::protocol::FsReadFileBoundedResponse;
use crate::protocol::FsReadFileParams;
use crate::protocol::FsReadFileResponse;
use crate::protocol::FsRemoveParams;
use crate::protocol::FsRemoveResponse;
use crate::protocol::FsWalkParams;
use crate::protocol::FsWalkResponse;
use crate::protocol::FsWriteFileParams;
use crate::protocol::FsWriteFileResponse;
use crate::rpc::internal_error;
use crate::rpc::invalid_request;
use crate::rpc::not_found;

pub const CODEX_FS_HELPER_ARG1: &str = "--codex-run-as-fs-helper";
const FS_READ_DIRECTORY_BOUNDED_OPERATION: &str = "fs/readDirectoryBounded";
pub(crate) const FS_PERMISSION_DENIED_ERROR_CODE: i64 = -32003;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FsHelperReadDirectoryBoundedParams {
    pub(crate) path: PathUri,
    pub(crate) max_entries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FsHelperReadDirectoryBoundedResponse {
    pub(crate) entries: Vec<FsReadDirectoryEntry>,
    pub(crate) entries_examined: usize,
    pub(crate) limit_reached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "params")]
pub(crate) enum FsHelperRequest {
    #[serde(rename = "fs/readFile")]
    ReadFile(FsReadFileParams),
    // A distinct operation makes older helpers refuse rather than ignore the cap.
    #[serde(rename = "fs/readFileCapped")]
    ReadFileCapped { path: PathUri, max_bytes: usize },
    #[serde(rename = "fs/readFileBounded")]
    ReadFileBounded(FsReadFileBoundedParams),
    #[serde(rename = "fs/readFileBoundedWithSource")]
    ReadFileBoundedWithSource(FsReadFileBoundedParams),
    #[serde(rename = "fs/writeFile")]
    WriteFile(FsWriteFileParams),
    #[serde(rename = "fs/createDirectory")]
    CreateDirectory(FsCreateDirectoryParams),
    #[serde(rename = "fs/getMetadata")]
    GetMetadata(FsGetMetadataParams),
    #[serde(rename = "fs/canonicalize")]
    Canonicalize(FsCanonicalizeParams),
    #[serde(rename = "fs/readDirectory")]
    ReadDirectory(FsReadDirectoryParams),
    #[serde(rename = "fs/readDirectoryBounded")]
    ReadDirectoryBounded(FsHelperReadDirectoryBoundedParams),
    #[serde(rename = "fs/walk")]
    Walk(FsWalkParams),
    #[serde(rename = "fs/remove")]
    Remove(FsRemoveParams),
    #[serde(rename = "fs/copy")]
    Copy(FsCopyParams),
    #[serde(rename = "fs/mutateFileIfMatches")]
    MutateFileIfMatches(FsMutateFileIfMatchesParams),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "camelCase")]
pub(crate) enum FsHelperResponse {
    Ok(FsHelperPayload),
    Error(JSONRPCErrorError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "response")]
pub(crate) enum FsHelperPayload {
    #[serde(rename = "fs/readFile")]
    ReadFile(FsReadFileResponse),
    #[serde(rename = "fs/readFileBounded")]
    ReadFileBounded(FsReadFileBoundedResponse),
    #[serde(rename = "fs/readFileBoundedWithSource")]
    ReadFileBoundedWithSource {
        data_base64: Option<String>,
        source: Option<std::path::PathBuf>,
    },
    #[serde(rename = "fs/writeFile")]
    WriteFile(FsWriteFileResponse),
    #[serde(rename = "fs/createDirectory")]
    CreateDirectory(FsCreateDirectoryResponse),
    #[serde(rename = "fs/getMetadata")]
    GetMetadata(FsGetMetadataResponse),
    #[serde(rename = "fs/canonicalize")]
    Canonicalize(FsCanonicalizeResponse),
    #[serde(rename = "fs/readDirectory")]
    ReadDirectory(FsReadDirectoryResponse),
    #[serde(rename = "fs/readDirectoryBounded")]
    ReadDirectoryBounded(FsHelperReadDirectoryBoundedResponse),
    #[serde(rename = "fs/walk")]
    Walk(FsWalkResponse),
    #[serde(rename = "fs/remove")]
    Remove(FsRemoveResponse),
    #[serde(rename = "fs/copy")]
    Copy(FsCopyResponse),
    #[serde(rename = "fs/mutateFileIfMatches")]
    MutateFileIfMatches(FsMutateFileIfMatchesResponse),
}

impl FsHelperPayload {
    fn operation(&self) -> &'static str {
        match self {
            Self::ReadFile(_) => FS_READ_FILE_METHOD,
            Self::ReadFileBounded(_) => FS_READ_FILE_BOUNDED_METHOD,
            Self::ReadFileBoundedWithSource { .. } => "fs/readFileBoundedWithSource",
            Self::WriteFile(_) => FS_WRITE_FILE_METHOD,
            Self::CreateDirectory(_) => FS_CREATE_DIRECTORY_METHOD,
            Self::GetMetadata(_) => FS_GET_METADATA_METHOD,
            Self::Canonicalize(_) => FS_CANONICALIZE_METHOD,
            Self::ReadDirectory(_) => FS_READ_DIRECTORY_METHOD,
            Self::ReadDirectoryBounded(_) => FS_READ_DIRECTORY_BOUNDED_OPERATION,
            Self::Walk(_) => FS_WALK_METHOD,
            Self::Remove(_) => FS_REMOVE_METHOD,
            Self::Copy(_) => FS_COPY_METHOD,
            Self::MutateFileIfMatches(_) => FS_MUTATE_FILE_IF_MATCHES_METHOD,
        }
    }

    pub(crate) fn expect_read_file(self) -> Result<FsReadFileResponse, JSONRPCErrorError> {
        match self {
            Self::ReadFile(response) => Ok(response),
            other => Err(unexpected_response(FS_READ_FILE_METHOD, other.operation())),
        }
    }

    pub(crate) fn expect_read_file_bounded(
        self,
    ) -> Result<FsReadFileBoundedResponse, JSONRPCErrorError> {
        match self {
            Self::ReadFileBounded(response) => Ok(response),
            other => Err(unexpected_response(
                FS_READ_FILE_BOUNDED_METHOD,
                other.operation(),
            )),
        }
    }

    pub(crate) fn expect_read_file_bounded_with_source(
        self,
    ) -> Result<(Option<String>, Option<std::path::PathBuf>), JSONRPCErrorError> {
        match self {
            Self::ReadFileBoundedWithSource {
                data_base64,
                source,
            } => Ok((data_base64, source)),
            other => Err(unexpected_response(
                "fs/readFileBoundedWithSource",
                other.operation(),
            )),
        }
    }

    pub(crate) fn expect_write_file(self) -> Result<FsWriteFileResponse, JSONRPCErrorError> {
        match self {
            Self::WriteFile(response) => Ok(response),
            other => Err(unexpected_response(FS_WRITE_FILE_METHOD, other.operation())),
        }
    }

    pub(crate) fn expect_create_directory(
        self,
    ) -> Result<FsCreateDirectoryResponse, JSONRPCErrorError> {
        match self {
            Self::CreateDirectory(response) => Ok(response),
            other => Err(unexpected_response(
                FS_CREATE_DIRECTORY_METHOD,
                other.operation(),
            )),
        }
    }

    pub(crate) fn expect_get_metadata(self) -> Result<FsGetMetadataResponse, JSONRPCErrorError> {
        match self {
            Self::GetMetadata(response) => Ok(response),
            other => Err(unexpected_response(
                FS_GET_METADATA_METHOD,
                other.operation(),
            )),
        }
    }

    pub(crate) fn expect_canonicalize(self) -> Result<FsCanonicalizeResponse, JSONRPCErrorError> {
        match self {
            Self::Canonicalize(response) => Ok(response),
            other => Err(unexpected_response(
                FS_CANONICALIZE_METHOD,
                other.operation(),
            )),
        }
    }

    pub(crate) fn expect_read_directory(
        self,
    ) -> Result<FsReadDirectoryResponse, JSONRPCErrorError> {
        match self {
            Self::ReadDirectory(response) => Ok(response),
            other => Err(unexpected_response(
                FS_READ_DIRECTORY_METHOD,
                other.operation(),
            )),
        }
    }

    pub(crate) fn expect_read_directory_bounded(
        self,
    ) -> Result<FsHelperReadDirectoryBoundedResponse, JSONRPCErrorError> {
        match self {
            Self::ReadDirectoryBounded(response) => Ok(response),
            other => Err(unexpected_response(
                FS_READ_DIRECTORY_BOUNDED_OPERATION,
                other.operation(),
            )),
        }
    }

    pub(crate) fn expect_walk(self) -> Result<FsWalkResponse, JSONRPCErrorError> {
        match self {
            Self::Walk(response) => Ok(response),
            other => Err(unexpected_response(FS_WALK_METHOD, other.operation())),
        }
    }

    pub(crate) fn expect_remove(self) -> Result<FsRemoveResponse, JSONRPCErrorError> {
        match self {
            Self::Remove(response) => Ok(response),
            other => Err(unexpected_response(FS_REMOVE_METHOD, other.operation())),
        }
    }

    pub(crate) fn expect_copy(self) -> Result<FsCopyResponse, JSONRPCErrorError> {
        match self {
            Self::Copy(response) => Ok(response),
            other => Err(unexpected_response(FS_COPY_METHOD, other.operation())),
        }
    }

    pub(crate) fn expect_mutate_file_if_matches(
        self,
    ) -> Result<FsMutateFileIfMatchesResponse, JSONRPCErrorError> {
        match self {
            Self::MutateFileIfMatches(response) => Ok(response),
            other => Err(unexpected_response(
                FS_MUTATE_FILE_IF_MATCHES_METHOD,
                other.operation(),
            )),
        }
    }
}

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

fn unexpected_response(expected: &str, actual: &str) -> JSONRPCErrorError {
    internal_error(format!(
        "unexpected fs sandbox helper response: expected {expected}, got {actual}"
    ))
}

pub(crate) async fn run_direct_request(
    request: FsHelperRequest,
) -> Result<FsHelperPayload, JSONRPCErrorError> {
    let file_system = DirectFileSystem;
    match request {
        FsHelperRequest::ReadFileCapped { path, max_bytes } => {
            let data = file_system
                .read_file_capped(&path, max_bytes, None)
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::ReadFile(FsReadFileResponse {
                data_base64: STANDARD.encode(data),
            }))
        }
        FsHelperRequest::ReadFile(params) => {
            let data = file_system
                .read_file(&params.path, /*sandbox*/ None)
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::ReadFile(FsReadFileResponse {
                data_base64: STANDARD.encode(data),
            }))
        }
        FsHelperRequest::ReadFileBounded(params) => {
            let data = if let Some(root) = params.confined_root.as_ref() {
                file_system
                    .read_file_bounded_confined(
                        &params.path,
                        root,
                        params.max_bytes,
                        /*sandbox*/ None,
                    )
                    .await
            } else {
                file_system
                    .read_file_bounded(&params.path, params.max_bytes, /*sandbox*/ None)
                    .await
            }
            .map_err(map_fs_error)?;
            Ok(FsHelperPayload::ReadFileBounded(
                FsReadFileBoundedResponse {
                    data_base64: data.map(|data| STANDARD.encode(data)),
                },
            ))
        }
        FsHelperRequest::ReadFileBoundedWithSource(params) => {
            if params.confined_root.is_some() {
                return Err(invalid_request(
                    "source acquisition does not accept confinedRoot".to_string(),
                ));
            }
            let read = file_system
                .read_file_bounded_with_source(&params.path, params.max_bytes, None)
                .await
                .map_err(map_fs_error)?;
            let (data_base64, source) = read.map_or((None, None), |(bytes, source)| {
                (Some(STANDARD.encode(bytes)), source)
            });
            Ok(FsHelperPayload::ReadFileBoundedWithSource {
                data_base64,
                source,
            })
        }
        FsHelperRequest::WriteFile(params) => {
            let bytes = STANDARD.decode(params.data_base64).map_err(|err| {
                invalid_request(format!(
                    "{FS_WRITE_FILE_METHOD} requires valid base64 dataBase64: {err}"
                ))
            })?;
            file_system
                .write_file(&params.path, bytes, /*sandbox*/ None)
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::WriteFile(FsWriteFileResponse {}))
        }
        FsHelperRequest::CreateDirectory(params) => {
            file_system
                .create_directory(
                    &params.path,
                    CreateDirectoryOptions {
                        recursive: params.recursive.unwrap_or(true),
                    },
                    /*sandbox*/ None,
                )
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::CreateDirectory(
                FsCreateDirectoryResponse {},
            ))
        }
        FsHelperRequest::GetMetadata(params) => {
            let metadata = file_system
                .get_metadata(&params.path, /*sandbox*/ None)
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::GetMetadata(FsGetMetadataResponse {
                directory_case_sensitive: if params.include_directory_case_sensitivity == Some(true)
                {
                    file_system
                        .directory_case_sensitive(&params.path, None)
                        .await
                        .map_err(map_fs_error)?
                } else {
                    None
                },
                is_directory: metadata.is_directory,
                is_file: metadata.is_file,
                is_symlink: metadata.is_symlink,
                size: metadata.size,
                created_at_ms: metadata.created_at_ms,
                modified_at_ms: metadata.modified_at_ms,
            }))
        }
        FsHelperRequest::Canonicalize(params) => {
            let path = file_system
                .canonicalize(&params.path, /*sandbox*/ None)
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::Canonicalize(FsCanonicalizeResponse {
                path,
            }))
        }
        FsHelperRequest::ReadDirectory(params) => {
            let entries = file_system
                .read_directory(&params.path, /*sandbox*/ None)
                .await
                .map_err(map_fs_error)?
                .into_iter()
                .map(|entry| FsReadDirectoryEntry {
                    file_name: entry.file_name,
                    is_directory: entry.is_directory,
                    is_file: entry.is_file,
                })
                .collect();
            Ok(FsHelperPayload::ReadDirectory(FsReadDirectoryResponse {
                entries,
            }))
        }
        FsHelperRequest::ReadDirectoryBounded(params) => {
            let outcome = file_system
                .read_directory_bounded(&params.path, params.max_entries, /*sandbox*/ None)
                .await
                .map_err(map_fs_error)?;
            let entries = outcome
                .entries
                .into_iter()
                .map(|entry| FsReadDirectoryEntry {
                    file_name: entry.file_name,
                    is_directory: entry.is_directory,
                    is_file: entry.is_file,
                })
                .collect();
            Ok(FsHelperPayload::ReadDirectoryBounded(
                FsHelperReadDirectoryBoundedResponse {
                    entries,
                    entries_examined: outcome.entries_examined,
                    limit_reached: outcome.limit_reached,
                },
            ))
        }
        FsHelperRequest::Walk(params) => {
            let outcome = file_system
                .walk(&params.path, params.options, /*sandbox*/ None)
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::Walk(outcome))
        }
        FsHelperRequest::Remove(params) => {
            file_system
                .remove(
                    &params.path,
                    RemoveOptions {
                        recursive: params.recursive.unwrap_or(true),
                        force: params.force.unwrap_or(true),
                    },
                    /*sandbox*/ None,
                )
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::Remove(FsRemoveResponse {}))
        }
        FsHelperRequest::Copy(params) => {
            file_system
                .copy(
                    &params.source_path,
                    &params.destination_path,
                    CopyOptions {
                        recursive: params.recursive,
                    },
                    /*sandbox*/ None,
                )
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::Copy(FsCopyResponse {}))
        }
        FsHelperRequest::MutateFileIfMatches(params) => {
            let (expected, replacement) =
                decode_mutation_payloads(params.expected_base64, params.replacement_base64)?;
            let outcome = file_system
                .mutate_file_if_matches(&params.path, expected, replacement, /*sandbox*/ None)
                .await
                .map_err(map_fs_error)?;
            Ok(FsHelperPayload::MutateFileIfMatches(
                FsMutateFileIfMatchesResponse {
                    outcome: outcome.into(),
                },
            ))
        }
    }
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


