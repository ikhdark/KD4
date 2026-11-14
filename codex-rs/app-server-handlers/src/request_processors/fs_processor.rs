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
use codex_app_server_protocol_types::FsReadFileRangeParams;
use codex_app_server_protocol_types::FsReadFileRangeResponse;
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
        diagnostic_operation: Option<&str>,
    ) -> Result<FsReadFileResponse, JSONRPCErrorError> {
        let mut observation = fs_observation(diagnostic_operation, "fs/readFile");
        let path = PathUri::from_abs_path(&params.path);
        let bytes = self
            .file_system()
            .inspect_err(|_| finish_fs_observation(&mut observation, "filesystem_unavailable", || serde_json::Value::Null))?
            .read_file_bounded(&path, MAX_READ_FILE_BYTES, /*sandbox*/ None)
            .await
            .inspect_err(|error| observe_fs_error(&mut observation, error))
            .map_err(map_fs_error)?
            .ok_or_else(|| {
                finish_fs_observation(&mut observation, "size_limit_or_changed_during_read", || {
                    serde_json::json!({"max_bytes":MAX_READ_FILE_BYTES})
                });
                invalid_request(format!(
                    "fs/readFile file exceeds maximum size of {MAX_READ_FILE_BYTES} bytes or changed while reading"
                ))
            })?;
        finish_fs_observation(&mut observation, "read", || serde_json::json!({"bytes":bytes.len()}));
        Ok(FsReadFileResponse {
            data_base64: STANDARD.encode(bytes),
        })
    }

    pub(crate) async fn read_file_range(
        &self,
        params: FsReadFileRangeParams,
        diagnostic_operation: Option<&str>,
    ) -> Result<FsReadFileRangeResponse, JSONRPCErrorError> {
        let mut observation = fs_observation(diagnostic_operation, "fs/readFileRange");
        let max_bytes = codex_file_system::FILE_RANGE_MAX_BYTES;
        if params.len > max_bytes {
            finish_fs_observation(&mut observation, "range_limit", || serde_json::json!({"max_bytes":max_bytes}));
            return Err(invalid_request(format!(
                "fs/readFileRange len exceeds maximum of {max_bytes} bytes"
            )));
        }
        let path = PathUri::from_abs_path(&params.path);
        let read = self
            .file_system()
            .inspect_err(|_| finish_fs_observation(&mut observation, "filesystem_unavailable", || serde_json::Value::Null))?
            .read_file_range(
                &path,
                params.start,
                params.len,
                params.expected_revision.as_deref(),
                /*sandbox*/ None,
            )
            .await
            .inspect_err(|error| observe_fs_error(&mut observation, error))
            .map_err(map_fs_error)?;
        finish_fs_observation(&mut observation, "read", || serde_json::json!({
            "bytes":read.bytes.len(), "file_bytes":read.file_bytes,
            "start":params.start, "requested_bytes":params.len,
            "expected_revision_supplied":params.expected_revision.is_some()}));
        Ok(FsReadFileRangeResponse {
            data_base64: STANDARD.encode(read.bytes),
            file_bytes: read.file_bytes,
            revision: read.revision,
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
        diagnostic_operation: Option<&str>,
    ) -> Result<FsGetMetadataResponse, JSONRPCErrorError> {
        let mut observation = fs_observation(diagnostic_operation, "fs/getMetadata");
        let path = PathUri::from_abs_path(&params.path);
        let metadata = self
            .file_system()
            .inspect_err(|_| finish_fs_observation(&mut observation, "filesystem_unavailable", || serde_json::Value::Null))
            .map_err(|error| with_metadata_error_data(error, "filesystem_unavailable", None))?
            .get_metadata(&path, /*sandbox*/ None)
            .await
            .inspect_err(|error| observe_fs_error(&mut observation, error))
            .map_err(|error| {
                let kind = metadata_error_kind(error.kind());
                let os_error_code = error.raw_os_error();
                with_metadata_error_data(map_fs_error(error), kind, os_error_code)
            })?;
        finish_fs_observation(&mut observation, "metadata_read", || serde_json::json!({
            "file_bytes":metadata.size, "is_file":metadata.is_file,
            "is_directory":metadata.is_directory, "is_symlink":metadata.is_symlink}));
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
        diagnostic_operation: Option<&str>,
    ) -> Result<FsReadDirectoryResponse, JSONRPCErrorError> {
        let mut observation = fs_observation(diagnostic_operation, "fs/readDirectory");
        if let Some(operation) = diagnostic_operation {
            let _ = codex_protocol::timing::observations::record(
                "mobile_directory_path_decoded", "", "", operation,
                Self::directory_path_facts(Some(&params.path.as_path().to_string_lossy())),
            );
        }
        let path = PathUri::from_abs_path(&params.path);
        let batch = self
            .file_system()
            .inspect_err(|_| finish_fs_observation(&mut observation, "filesystem_unavailable", || serde_json::Value::Null))?
            .read_directory_bounded(&path, MAX_READ_DIRECTORY_ENTRIES + 1, /*sandbox*/ None)
            .await
            .inspect_err(|error| observe_fs_error(&mut observation, error))
            .map_err(map_fs_error)?;
        let entries = batch.entries;
        if batch.limit_reached || entries.len() > MAX_READ_DIRECTORY_ENTRIES
            || entries.iter().try_fold(0usize, |bytes, entry| bytes.checked_add(entry.file_name.len()))
                .is_none_or(|bytes| bytes > MAX_READ_DIRECTORY_NAME_BYTES)
        {
            finish_fs_observation(&mut observation, "response_budget_exceeded", || serde_json::json!({
                "entry_count":entries.len(), "entry_limit_reached":batch.limit_reached}));
            return Err(invalid_request("fs/readDirectory exceeds the directory response budget"));
        }
        finish_fs_observation(&mut observation, "directory_read", || {
            let git_entry = entries.iter().find(|entry| entry.file_name == ".git");
            serde_json::json!({"entry_count":entries.len(),
                "git_entry_present":git_entry.is_some(),
                "git_entry_is_directory":git_entry.map(|entry| entry.is_directory),
                "git_entry_is_file":git_entry.map(|entry| entry.is_file)})
        });
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

    // Fingerprint the exact input without recording private paths or other names.
    // Comparing ingress and decoded facts also exposes Windows path rewriting.
    pub(crate) fn directory_path_facts(path: Option<&str>) -> serde_json::Value {
        use sha2::Digest;

        let syntax = path.map(|path| {
            let bytes = path.as_bytes();
            if path.starts_with("\\\\?\\") || path.starts_with("//?/") {
                "windows_verbatim"
            } else if path.starts_with("file:") {
                "file_uri"
            } else if bytes.first().is_some_and(u8::is_ascii_alphabetic)
                && bytes.get(1) == Some(&b':')
            {
                "windows_drive"
            } else if path.starts_with("\\\\") || path.starts_with("//") {
                "unc"
            } else if path.starts_with('/') {
                "unix_absolute"
            } else {
                "relative_or_other"
            }
        });
        serde_json::json!({"is_string":path.is_some(), "syntax":syntax,
            "bytes":path.map(str::len),
            "sha256":path.map(|path| hex::encode(sha2::Sha256::digest(path.as_bytes())))})
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

// Correlate only opaque request occurrences; never journal paths, revisions,
// file contents or io::Error messages (which may contain all of those).
fn fs_observation(operation: Option<&str>, method: &str) -> Option<codex_protocol::timing::observations::Span> {
    operation.map(|operation| codex_protocol::timing::observations::Span::start(
        if operation.starts_with("local:") { "local_file_read" } else { "mobile_file_read" },
        "", "", operation, serde_json::json!({"method":method})))
}

fn finish_fs_observation(
    observation: &mut Option<codex_protocol::timing::observations::Span>,
    outcome: &str,
    facts: impl FnOnce() -> serde_json::Value,
) {
    if let Some(observation) = observation.take() {
        observation.finish(outcome, facts());
    }
}

fn observe_fs_error(observation: &mut Option<codex_protocol::timing::observations::Span>, error: &io::Error) {
    finish_fs_observation(observation, "io_error", || serde_json::json!({
        "io_error_kind":format!("{:?}", error.kind()), "os_error_code":error.raw_os_error()}));
}

fn metadata_error_kind(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::PermissionDenied => "permission_denied",
        io::ErrorKind::InvalidInput => "invalid_input",
        io::ErrorKind::AlreadyExists => "already_exists",
        io::ErrorKind::NotADirectory => "not_directory",
        io::ErrorKind::IsADirectory => "is_directory",
        io::ErrorKind::Unsupported => "unsupported",
        io::ErrorKind::Interrupted => "interrupted",
        io::ErrorKind::TimedOut => "timed_out",
        _ => "other",
    }
}

fn with_metadata_error_data(
    mut error: JSONRPCErrorError,
    kind: &'static str,
    os_error_code: Option<i32>,
) -> JSONRPCErrorError {
    let data = error.data.get_or_insert_with(|| serde_json::json!({}));
    if let Some(data) = data.as_object_mut() {
        data.insert("kdrFs".to_owned(), serde_json::json!({
            "version": 1, "errorKind": kind, "osErrorCode": os_error_code
        }));
    }
    error
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
