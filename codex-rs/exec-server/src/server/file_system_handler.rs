use std::io;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use codex_exec_server_protocol::JSONRPCErrorError;

use crate::CopyOptions;
use crate::CreateDirectoryOptions;
use crate::ExecServerRuntimePaths;
use crate::ExecutorFileSystem;
use crate::RemoveOptions;
use crate::connection::MAX_FILE_PAYLOAD_BYTES;
use crate::file_read::FileReadHandleManager;
use crate::local_file_system::LocalFileSystem;
use crate::protocol::FS_WRITE_FILE_METHOD;
use crate::protocol::FsCanonicalizeParams;
use crate::protocol::FsCanonicalizeResponse;
use crate::protocol::FsCloseParams;
use crate::protocol::FsCloseResponse;
use crate::protocol::FsCopyParams;
use crate::protocol::FsCopyResponse;
use crate::protocol::FsCreateDirectoryParams;
use crate::protocol::FsCreateDirectoryResponse;
use crate::protocol::FsGetMetadataParams;
use crate::protocol::FsGetMetadataResponse;
use crate::protocol::FsOpenParams;
use crate::protocol::FsOpenResponse;
use crate::protocol::FsReadBlockParams;
use crate::protocol::FsReadBlockResponse;
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

const MAX_FILE_READ_HANDLE_ID_BYTES: usize = 32;
const MAX_DIRECTORY_ENTRIES: usize = 16_384;

#[derive(Clone)]
pub(crate) struct FileSystemHandler {
    file_system: LocalFileSystem,
    file_reads: FileReadHandleManager,
}

impl FileSystemHandler {
    pub(crate) fn new(runtime_paths: ExecServerRuntimePaths) -> Self {
        Self {
            file_system: LocalFileSystem::with_runtime_paths(runtime_paths),
            file_reads: FileReadHandleManager::default(),
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.file_reads.close_all().await;
    }

    pub(crate) async fn open(
        &self,
        params: FsOpenParams,
    ) -> Result<FsOpenResponse, JSONRPCErrorError> {
        validate_file_read_handle_id(&params.handle_id)?;
        let file = self
            .file_system
            .open_file_for_read(&params.path, params.sandbox.as_ref())
            .await
            .map_err(map_fs_error)?;
        let handle_id = self
            .file_reads
            .open(params.handle_id, file)
            .await
            .map_err(map_fs_error)?;
        Ok(FsOpenResponse { handle_id })
    }

    pub(crate) async fn read_block(
        &self,
        params: FsReadBlockParams,
    ) -> Result<FsReadBlockResponse, JSONRPCErrorError> {
        validate_file_read_handle_id(&params.handle_id)?;
        let block = self
            .file_reads
            .read_block(&params.handle_id, params.offset, params.len)
            .await
            .map_err(map_fs_error)?;
        Ok(FsReadBlockResponse {
            chunk: block.bytes.into(),
            eof: block.eof,
        })
    }

    pub(crate) async fn close(
        &self,
        params: FsCloseParams,
    ) -> Result<FsCloseResponse, JSONRPCErrorError> {
        validate_file_read_handle_id(&params.handle_id)?;
        self.file_reads.close(&params.handle_id).await;
        Ok(FsCloseResponse {})
    }

    pub(crate) async fn read_file(
        &self,
        params: FsReadFileParams,
    ) -> Result<FsReadFileResponse, JSONRPCErrorError> {
        let bytes = self
            .file_system
            .read_file_capped(
                &params.path,
                MAX_FILE_PAYLOAD_BYTES,
                params.sandbox.as_ref(),
            )
            .await
            .map_err(map_fs_error)?;
        if bytes.len() > MAX_FILE_PAYLOAD_BYTES {
            return Err(invalid_request(format!(
                "file is too large to read through exec-server: {} bytes exceeds the {MAX_FILE_PAYLOAD_BYTES}-byte response limit",
                bytes.len()
            )));
        }
        Ok(FsReadFileResponse {
            data_base64: STANDARD.encode(bytes),
        })
    }

    pub(crate) async fn read_file_bounded(
        &self,
        params: FsReadFileBoundedParams,
    ) -> Result<FsReadFileBoundedResponse, JSONRPCErrorError> {
        // Never read beyond what one response can carry. A larger caller limit
        // cannot be honored remotely, so a miss is reported instead of looking
        // like an ordinary "too large or changed" result for that limit.
        let exceeds_response_limit = params.max_bytes > MAX_FILE_PAYLOAD_BYTES;
        let max_bytes = params.max_bytes.min(MAX_FILE_PAYLOAD_BYTES);
        let sandbox = params.sandbox.as_ref();
        let data = match params.confined_root.as_ref() {
            Some(root) => {
                self.file_system
                    .read_file_bounded_confined(&params.path, root, max_bytes, sandbox)
                    .await
            }
            None => {
                self.file_system
                    .read_file_bounded(&params.path, max_bytes, sandbox)
                    .await
            }
        }
        .map_err(map_fs_error)?;
        if data.is_none() && exceeds_response_limit {
            return Err(invalid_request(format!(
                "file exceeds the {MAX_FILE_PAYLOAD_BYTES}-byte exec-server response limit or changed while being read"
            )));
        }
        Ok(FsReadFileBoundedResponse {
            data_base64: data.map(|data| STANDARD.encode(data)),
        })
    }

    pub(crate) async fn read_file_range(
        &self, params: crate::protocol::FsReadFileRangeParams,
    ) -> Result<crate::protocol::FsReadFileRangeResponse, JSONRPCErrorError> {
        let read = self.file_system.read_file_range(&params.path, params.start, params.len,
            params.expected_revision.as_deref(), params.sandbox.as_ref()).await.map_err(map_fs_error)?;
        Ok(crate::protocol::FsReadFileRangeResponse {
            data_base64: STANDARD.encode(read.bytes), file_bytes: read.file_bytes, revision: read.revision,
        })
    }

    pub(crate) async fn checksum_file(
        &self,
        params: crate::protocol::FsChecksumFileParams,
    ) -> Result<crate::protocol::FsChecksumFileResponse, JSONRPCErrorError> {
        let digest = self
            .file_system
            .checksum_file(&params.path, params.include_text_diagnostics, params.sandbox.as_ref())
            .await
            .map_err(map_fs_error)?;
        Ok(crate::protocol::FsChecksumFileResponse { digest })
    }

    pub(crate) async fn write_file(
        &self,
        params: FsWriteFileParams,
    ) -> Result<FsWriteFileResponse, JSONRPCErrorError> {
        let bytes = STANDARD.decode(params.data_base64).map_err(|err| {
            invalid_request(format!(
                "{FS_WRITE_FILE_METHOD} requires valid base64 dataBase64: {err}"
            ))
        })?;
        self.file_system
            .write_file(&params.path, bytes, params.sandbox.as_ref())
            .await
            .map_err(map_fs_error)?;
        Ok(FsWriteFileResponse {})
    }

    pub(crate) async fn mutate_file_if_matches(
        &self,
        params: crate::protocol::FsMutateFileIfMatchesParams,
    ) -> Result<crate::protocol::FsMutateFileIfMatchesResponse, JSONRPCErrorError> {
        let (expected, replacement) = crate::fs_helper::decode_mutation_payloads(
            params.expected_base64,
            params.replacement_base64,
        )?;
        let outcome = self
            .file_system
            .mutate_file_if_matches(&params.path, expected, replacement, params.sandbox.as_ref())
            .await
            .map_err(map_fs_error)?;
        Ok(crate::protocol::FsMutateFileIfMatchesResponse {
            outcome: outcome.into(),
        })
    }

    pub(crate) async fn create_directory(
        &self,
        params: FsCreateDirectoryParams,
    ) -> Result<FsCreateDirectoryResponse, JSONRPCErrorError> {
        let recursive = params.recursive.unwrap_or(true);
        self.file_system
            .create_directory(
                &params.path,
                CreateDirectoryOptions { recursive },
                params.sandbox.as_ref(),
            )
            .await
            .map_err(map_fs_error)?;
        Ok(FsCreateDirectoryResponse {})
    }

    pub(crate) async fn get_metadata(
        &self,
        params: FsGetMetadataParams,
    ) -> Result<FsGetMetadataResponse, JSONRPCErrorError> {
        let metadata = self
            .file_system
            .get_metadata(&params.path, params.sandbox.as_ref())
            .await
            .map_err(map_fs_error)?;
        Ok(FsGetMetadataResponse {
            directory_case_sensitive: if params.include_directory_case_sensitivity == Some(true) {
                self.file_system
                    .directory_case_sensitive(&params.path, params.sandbox.as_ref())
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
        })
    }

    pub(crate) async fn canonicalize(
        &self,
        params: FsCanonicalizeParams,
    ) -> Result<FsCanonicalizeResponse, JSONRPCErrorError> {
        let path = self
            .file_system
            .canonicalize(&params.path, params.sandbox.as_ref())
            .await
            .map_err(map_fs_error)?;
        Ok(FsCanonicalizeResponse { path })
    }

    pub(crate) async fn read_directory(
        &self,
        params: FsReadDirectoryParams,
    ) -> Result<FsReadDirectoryResponse, JSONRPCErrorError> {
        self.read_directory_with_limit(params, MAX_DIRECTORY_ENTRIES).await
    }

    async fn read_directory_with_limit(
        &self,
        params: FsReadDirectoryParams,
        max_entries: usize,
    ) -> Result<FsReadDirectoryResponse, JSONRPCErrorError> {
        let outcome = self
            .file_system
            .read_directory_bounded(&params.path, max_entries, params.sandbox.as_ref())
            .await
            .map_err(map_fs_error)?;
        if outcome.limit_reached {
            return Err(invalid_request("directory RPC entry limit reached; use fs/walk with explicit bounds".into()));
        }
        let entries = outcome.entries
            .into_iter()
            .map(|entry| FsReadDirectoryEntry {
                file_name: entry.file_name,
                is_directory: entry.is_directory,
                is_file: entry.is_file,
            })
            .collect();
        Ok(FsReadDirectoryResponse { entries })
    }

    pub(crate) async fn walk(
        &self,
        params: FsWalkParams,
    ) -> Result<FsWalkResponse, JSONRPCErrorError> {
        self.file_system
            .walk(&params.path, params.options, params.sandbox.as_ref())
            .await
            .map_err(map_fs_error)
    }

    pub(crate) async fn remove(
        &self,
        params: FsRemoveParams,
    ) -> Result<FsRemoveResponse, JSONRPCErrorError> {
        let recursive = params.recursive.unwrap_or(true);
        let force = params.force.unwrap_or(true);
        self.file_system
            .remove(
                &params.path,
                RemoveOptions { recursive, force },
                params.sandbox.as_ref(),
            )
            .await
            .map_err(map_fs_error)?;
        Ok(FsRemoveResponse {})
    }

    pub(crate) async fn copy(
        &self,
        params: FsCopyParams,
    ) -> Result<FsCopyResponse, JSONRPCErrorError> {
        self.file_system
            .copy(
                &params.source_path,
                &params.destination_path,
                CopyOptions {
                    recursive: params.recursive,
                },
                params.sandbox.as_ref(),
            )
            .await
            .map_err(map_fs_error)?;
        Ok(FsCopyResponse {})
    }
}

fn validate_file_read_handle_id(handle_id: &str) -> Result<(), JSONRPCErrorError> {
    if handle_id.len() > MAX_FILE_READ_HANDLE_ID_BYTES {
        return Err(invalid_request(format!(
            "file read handle ID must not exceed {MAX_FILE_READ_HANDLE_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

fn map_fs_error(err: io::Error) -> JSONRPCErrorError {
    let mut error = match err.kind() {
        io::ErrorKind::NotFound => not_found(err.to_string()),
        io::ErrorKind::InvalidInput | io::ErrorKind::PermissionDenied => {
            invalid_request(err.to_string())
        }
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

