use crate::connection_rpc_gate::ConnectionRpcGate;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::fs_watch::FsWatchManager;
use crate::outgoing_message::ConnectionId;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_app_server_protocol_types::FsCopyParams;
use codex_app_server_protocol_types::FsCopyResponse;
use codex_app_server_protocol_types::FsCreateDirectoryParams;
use codex_app_server_protocol_types::FsCreateDirectoryResponse;
use codex_app_server_protocol_types::FsGetMetadataParams;
use codex_app_server_protocol_types::FsGetMetadataResponse;
use codex_app_server_protocol_types::FsReadDirectoryEntry;
use codex_app_server_protocol_types::FsReadDirectoryParams;
use codex_app_server_protocol_types::FsReadDirectoryResponse;
use codex_app_server_protocol_types::FsReadFileParams;
use codex_app_server_protocol_types::FsReadFileResponse;
use codex_app_server_protocol_types::FsRemoveParams;
use codex_app_server_protocol_types::FsRemoveResponse;
use codex_app_server_protocol_types::FsUnwatchParams;
use codex_app_server_protocol_types::FsUnwatchResponse;
use codex_app_server_protocol_types::FsWatchParams;
use codex_app_server_protocol_types::FsWatchResponse;
use codex_app_server_protocol_types::FsWriteFileParams;
use codex_app_server_protocol_types::FsWriteFileResponse;
use codex_app_server_protocol_types::JSONRPCErrorError;
use codex_exec_server::CopyOptions;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::RemoveOptions;
use codex_utils_path_uri::PathUri;
use std::io;
use std::sync::Arc;

// Base64 expands responses by roughly one third, so keep the raw file cap low
// enough to bound both the read buffer and the encoded JSON-RPC response while
// still accommodating ordinary source and document reads.
const MAX_READ_FILE_BYTES: usize = 10 * 1024 * 1024;
const MAX_WRITE_FILE_BYTES: usize = 10 * 1024 * 1024;
const MAX_WRITE_FILE_BASE64_BYTES: usize = MAX_WRITE_FILE_BYTES.div_ceil(3) * 4;
const MAX_READ_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_READ_DIRECTORY_NAME_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct FsRequestProcessor {
    environment_manager: Arc<EnvironmentManager>,
    fs_watch_manager: FsWatchManager,
}

impl FsRequestProcessor {
    pub(crate) fn new(
        environment_manager: Arc<EnvironmentManager>,
        fs_watch_manager: FsWatchManager,
    ) -> Self {
        Self {
            environment_manager,
            fs_watch_manager,
        }
    }

    fn file_system(&self) -> Result<Arc<dyn ExecutorFileSystem>, JSONRPCErrorError> {
        self.environment_manager
            .try_local_environment()
            .map(|environment| environment.get_filesystem())
            .ok_or_else(|| internal_error("local filesystem is not configured"))
    }

    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        self.fs_watch_manager.connection_closed(connection_id).await;
    }

    pub(crate) async fn read_file(
        &self,
        params: FsReadFileParams,
    ) -> Result<FsReadFileResponse, JSONRPCErrorError> {
        let path = PathUri::from_abs_path(&params.path);
        let bytes = self
            .file_system()?
            .read_file_bounded(&path, MAX_READ_FILE_BYTES, /*sandbox*/ None)
            .await
            .map_err(map_fs_error)?
            .ok_or_else(|| {
                invalid_request(format!(
                    "fs/readFile file exceeds maximum size of {MAX_READ_FILE_BYTES} bytes or changed while reading"
                ))
            })?;
        Ok(FsReadFileResponse {
            data_base64: STANDARD.encode(bytes),
        })
    }

    pub(crate) async fn write_file(
        &self,
        params: FsWriteFileParams,
    ) -> Result<FsWriteFileResponse, JSONRPCErrorError> {
        if params.data_base64.len() > MAX_WRITE_FILE_BASE64_BYTES {
            return Err(write_file_size_error());
        }
        let bytes = STANDARD.decode(params.data_base64).map_err(|err| {
            invalid_request(format!(
                "fs/writeFile requires valid base64 dataBase64: {err}"
            ))
        })?;
        if bytes.len() > MAX_WRITE_FILE_BYTES {
            return Err(write_file_size_error());
        }
        let path = PathUri::from_abs_path(&params.path);
        self.file_system()?
            .write_file(&path, bytes, /*sandbox*/ None)
            .await
            .map_err(map_fs_error)?;
        Ok(FsWriteFileResponse {})
    }

    pub(crate) async fn create_directory(
        &self,
        params: FsCreateDirectoryParams,
    ) -> Result<FsCreateDirectoryResponse, JSONRPCErrorError> {
        let path = PathUri::from_abs_path(&params.path);
        self.file_system()?
            .create_directory(
                &path,
                CreateDirectoryOptions {
                    recursive: params.recursive.unwrap_or(true),
                },
                /*sandbox*/ None,
            )
            .await
            .map_err(map_fs_error)?;
        Ok(FsCreateDirectoryResponse {})
    }

    pub(crate) async fn get_metadata(
        &self,
        params: FsGetMetadataParams,
    ) -> Result<FsGetMetadataResponse, JSONRPCErrorError> {
        let path = PathUri::from_abs_path(&params.path);
        let metadata = self
            .file_system()?
            .get_metadata(&path, /*sandbox*/ None)
            .await
            .map_err(map_fs_error)?;
        Ok(FsGetMetadataResponse {
            is_directory: metadata.is_directory,
            is_file: metadata.is_file,
            is_symlink: metadata.is_symlink,
            size: metadata.size,
            created_at_ms: metadata.created_at_ms,
            modified_at_ms: metadata.modified_at_ms,
        })
    }

    pub(crate) async fn read_directory(
        &self,
        params: FsReadDirectoryParams,
    ) -> Result<FsReadDirectoryResponse, JSONRPCErrorError> {
        let path = PathUri::from_abs_path(&params.path);
        let entries = self
            .file_system()?
            .read_directory(&path, /*sandbox*/ None)
            .await
            .map_err(map_fs_error)?;
        if entries.len() > MAX_READ_DIRECTORY_ENTRIES
            || entries.iter().try_fold(0usize, |bytes, entry| bytes.checked_add(entry.file_name.len()))
                .is_none_or(|bytes| bytes > MAX_READ_DIRECTORY_NAME_BYTES)
        {
            return Err(invalid_request("fs/readDirectory exceeds the directory response budget"));
        }
        Ok(FsReadDirectoryResponse {
            entries: entries
                .into_iter()
                .map(|entry| FsReadDirectoryEntry {
                    file_name: entry.file_name,
                    is_directory: entry.is_directory,
                    is_file: entry.is_file,
                })
                .collect(),
        })
    }

    pub(crate) async fn remove(
        &self,
        params: FsRemoveParams,
    ) -> Result<FsRemoveResponse, JSONRPCErrorError> {
        let path = PathUri::from_abs_path(&params.path);
        self.file_system()?
            .remove(
                &path,
                RemoveOptions {
                    recursive: params.recursive.unwrap_or(true),
                    force: params.force.unwrap_or(true),
                },
                /*sandbox*/ None,
            )
            .await
            .map_err(map_fs_error)?;
        Ok(FsRemoveResponse {})
    }

    pub(crate) async fn copy(
        &self,
        params: FsCopyParams,
    ) -> Result<FsCopyResponse, JSONRPCErrorError> {
        let source_path = PathUri::from_abs_path(&params.source_path);
        let destination_path = PathUri::from_abs_path(&params.destination_path);
        self.file_system()?
            .copy(
                &source_path,
                &destination_path,
                CopyOptions {
                    recursive: params.recursive,
                },
                /*sandbox*/ None,
            )
            .await
            .map_err(map_fs_error)?;
        Ok(FsCopyResponse {})
    }

    pub(crate) async fn watch(
        &self,
        connection_id: ConnectionId,
        params: FsWatchParams,
        rpc_gate: &ConnectionRpcGate,
    ) -> Result<FsWatchResponse, JSONRPCErrorError> {
        self.file_system()?;
        self.fs_watch_manager
            .watch_with_gate(connection_id, params, rpc_gate)
            .await
    }

    pub(crate) async fn unwatch(
        &self,
        connection_id: ConnectionId,
        params: FsUnwatchParams,
    ) -> Result<FsUnwatchResponse, JSONRPCErrorError> {
        self.file_system()?;
        self.fs_watch_manager.unwatch(connection_id, params).await
    }
}

fn write_file_size_error() -> JSONRPCErrorError {
    invalid_request(format!(
        "fs/writeFile data exceeds maximum size of {MAX_WRITE_FILE_BYTES} bytes"
    ))
}

fn map_fs_error(err: io::Error) -> JSONRPCErrorError {
    let mut error = if err.kind() == io::ErrorKind::InvalidInput {
        invalid_request(err.to_string())
    } else {
        internal_error(err.to_string())
    };
    if let Some(copy) = codex_file_system::CopyFailure::from_io_error(&err) {
        error.data = Some(serde_json::json!({"copy": copy}));
    }
    error
}
