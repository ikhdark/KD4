use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use codex_utils_path_uri::PathUri;
use std::sync::Arc;
use tokio::io;
use tracing::trace;

use crate::CopyOptions;
use crate::CreateDirectoryOptions;
use crate::ExecServerError;
use crate::ExecutorFileSystem;
use crate::ExecutorFileSystemFuture;
use crate::FileMetadata;
use crate::FileSystemReadStream;
use crate::FileSystemResult;
use crate::FileSystemSandboxContext;
use crate::ReadDirectoryEntry;
use crate::RemoveOptions;
use crate::WalkOptions;
use crate::WalkOutcome;
use crate::client::LazyRemoteExecServerClient;
use crate::connection::MAX_FILE_PAYLOAD_BYTES;
use crate::protocol::FsCanonicalizeParams;
use crate::protocol::FsCopyParams;
use crate::protocol::FsCreateDirectoryParams;
use crate::protocol::FsGetMetadataParams;
use crate::protocol::FsReadDirectoryParams;
use crate::protocol::FsReadFileBoundedParams;
use crate::protocol::FsReadFileParams;
use crate::protocol::FsRemoveParams;
use crate::protocol::FsWalkParams;
use crate::protocol::FsWriteFileParams;
use crate::rpc::RpcClient;

const INVALID_REQUEST_ERROR_CODE: i64 = -32600;
const INVALID_PARAMS_ERROR_CODE: i64 = -32602;
const METHOD_NOT_FOUND_ERROR_CODE: i64 = -32601;
const NOT_FOUND_ERROR_CODE: i64 = -32004;

/// Validate the bounded, revision-aware wire result for remote and helper reads.
pub(crate) fn decode_file_range(
    response: crate::protocol::FsReadFileRangeResponse, start: u64, len: usize,
    expected: Option<&str>,
) -> io::Result<codex_file_system::FileRangeRead> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid revision-bound byte-range response");
    if !(1..=codex_file_system::FILE_RANGE_MAX_BYTES).contains(&len)
        || start.checked_add(len as u64).is_none()
        || start > response.file_bytes
        || response.revision.is_empty() || response.revision.len() > 512
        || expected.is_some_and(|expected| expected != response.revision)
        || response.data_base64.len() > len.div_ceil(3) * 4
    {
        return Err(invalid());
    }
    let bytes = STANDARD.decode(response.data_base64).map_err(|_| invalid())?;
    if bytes.len() as u64 != (response.file_bytes - start).min(len as u64) {
        return Err(invalid());
    }
    Ok(codex_file_system::FileRangeRead { bytes, file_bytes: response.file_bytes, revision: response.revision })
}

/// Validate a remote or helper digest before it can certify any bytes.
pub(crate) fn validate_file_digest(
    digest: Option<codex_file_system::FileDigest>,
) -> io::Result<Option<codex_file_system::FileDigest>> {
    if let Some(digest) = &digest
        && (digest.sha256.len() != 64
            || !digest
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || digest.revision.is_empty()
            || digest.revision.len() > 512)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid file checksum response",
        ));
    }
    Ok(digest)
}

#[path = "remote_file_stream.rs"]
mod file_stream;

pub(crate) struct RemoteFileSystem {
    client: LazyRemoteExecServerClient,
}

impl RemoteFileSystem {
    pub(crate) fn new(client: LazyRemoteExecServerClient) -> Self {
        trace!("remote fs new");
        Self { client }
    }

    async fn canonicalize(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<PathUri> {
        trace!("remote fs canonicalize");
        let client = self.client.get().await.map_err(map_remote_error)?;
        let response = client
            .fs_canonicalize(FsCanonicalizeParams {
                path: path.clone(),
                sandbox: remote_sandbox_context(sandbox),
            })
            .await
            .map_err(map_remote_error)?;
        Ok(response.path)
    }

    async fn read_file(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Vec<u8>> {
        trace!("remote fs read_file");
        let client = self.client.get().await.map_err(map_remote_error)?;
        let response = client
            .fs_read_file(FsReadFileParams {
                path: path.clone(),
                sandbox: remote_sandbox_context(sandbox),
            })
            .await
            .map_err(map_remote_error)?;
        STANDARD.decode(response.data_base64).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("remote fs/readFile returned invalid base64 dataBase64: {err}"),
            )
        })
    }

    async fn read_file_bounded_request(
        &self,
        path: &PathUri,
        confined_root: Option<&PathUri>,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Option<Vec<u8>>> {
        trace!("remote fs read_file_bounded");
        let client = self.client.get().await.map_err(map_remote_error)?;
        let response = match client
            .fs_read_file_bounded(FsReadFileBoundedParams {
                path: path.clone(),
                max_bytes,
                confined_root: confined_root.cloned(),
                sandbox: remote_sandbox_context(sandbox),
            })
            .await
        {
            Ok(response) => response,
            // Executors without the method keep their previous behavior.
            Err(ExecServerError::Server {
                code: METHOD_NOT_FOUND_ERROR_CODE,
                ..
            }) => {
                return match confined_root {
                    None => {
                        <Self as ExecutorFileSystem>::read_file_bounded_via_streams(
                            self, path, max_bytes, sandbox,
                        )
                        .await
                    }
                    Some(_) => Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "confined bounded reads are not supported by this exec-server",
                    )),
                };
            }
            Err(error) => return Err(map_remote_error(error)),
        };
        response
            .data_base64
            .map(|data_base64| {
                if data_base64.len() > max_bytes.div_ceil(3).saturating_mul(4) {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "remote fs/readFileBounded exceeded maxBytes"));
                }
                let bytes = STANDARD.decode(data_base64).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "remote fs/readFileBounded returned invalid base64 dataBase64: {err}"
                        ),
                    )
                })?;
                if bytes.len() > max_bytes {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "remote fs/readFileBounded exceeded maxBytes"));
                }
                Ok(bytes)
            })
            .transpose()
    }

    async fn read_file_stream(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<FileSystemReadStream> {
        if sandbox.is_some_and(FileSystemSandboxContext::should_run_in_sandbox) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "streaming file reads do not support platform sandboxing",
            ));
        }
        trace!("remote fs read_file_stream");
        let client = self.client.get().await.map_err(map_remote_error)?;
        file_stream::open(client, path.clone(), remote_sandbox_context(sandbox)).await
    }

    async fn write_file(
        &self,
        path: &PathUri,
        contents: Vec<u8>,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        trace!("remote fs write_file");
        // The receiving transport would close the shared connection instead.
        if contents.len() > MAX_FILE_PAYLOAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "file is too large to write through exec-server: {} bytes exceeds the {MAX_FILE_PAYLOAD_BYTES}-byte request limit",
                    contents.len()
                ),
            ));
        }
        let client = self.client.get().await.map_err(map_remote_error)?;
        client
            .fs_write_file(FsWriteFileParams {
                path: path.clone(),
                data_base64: STANDARD.encode(contents),
                sandbox: remote_sandbox_context(sandbox),
            })
            .await
            .map_err(map_remote_error)?;
        Ok(())
    }

    async fn mutate_file_if_matches(
        &self,
        path: &PathUri,
        expected: Option<Vec<u8>>,
        replacement: Option<Vec<u8>>,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<codex_file_system::FileMutationOutcome> {
        trace!("remote fs mutate_file_if_matches");
        // Both payloads travel in one request; keep it within the write budget.
        let payload_bytes =
            expected.as_ref().map_or(0, Vec::len) + replacement.as_ref().map_or(0, Vec::len);
        if payload_bytes > MAX_FILE_PAYLOAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "file mutation is too large for exec-server: {payload_bytes} bytes exceeds the {MAX_FILE_PAYLOAD_BYTES}-byte request limit"
                ),
            ));
        }
        let client = self.client.get().await.map_err(map_remote_error)?;
        let response = client
            .fs_mutate_file_if_matches(crate::protocol::FsMutateFileIfMatchesParams {
                path: path.clone(),
                expected_base64: expected.map(|bytes| STANDARD.encode(bytes)),
                replacement_base64: replacement.map(|bytes| STANDARD.encode(bytes)),
                sandbox: remote_sandbox_context(sandbox),
            })
            .await
            .map_err(map_remote_error)?;
        Ok(response.outcome.into())
    }

    async fn create_directory(
        &self,
        path: &PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        trace!("remote fs create_directory");
        let client = self.client.get().await.map_err(map_remote_error)?;
        client
            .fs_create_directory(FsCreateDirectoryParams {
                path: path.clone(),
                recursive: Some(options.recursive),
                sandbox: remote_sandbox_context(sandbox),
            })
            .await
            .map_err(map_remote_error)?;
        Ok(())
    }

    async fn get_metadata(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<FileMetadata> {
        trace!("remote fs get_metadata");
        let client = self.client.get().await.map_err(map_remote_error)?;
        let response = client
            .fs_get_metadata(FsGetMetadataParams {
                path: path.clone(),
                sandbox: remote_sandbox_context(sandbox),
                include_directory_case_sensitivity: None,
            })
            .await
            .map_err(map_remote_error)?;
        Ok(FileMetadata {
            is_directory: response.is_directory,
            is_file: response.is_file,
            is_symlink: response.is_symlink,
            size: response.size,
            created_at_ms: response.created_at_ms,
            modified_at_ms: response.modified_at_ms,
        })
    }

    async fn read_directory(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Vec<ReadDirectoryEntry>> {
        trace!("remote fs read_directory");
        let client = self.client.get().await.map_err(map_remote_error)?;
        let response = client
            .fs_read_directory(FsReadDirectoryParams {
                path: path.clone(),
                sandbox: remote_sandbox_context(sandbox),
            })
            .await
            .map_err(map_remote_error)?;
        Ok(response
            .entries
            .into_iter()
            .map(|entry| ReadDirectoryEntry {
                file_name: entry.file_name,
                is_directory: entry.is_directory,
                is_file: entry.is_file,
            })
            .collect())
    }

    async fn walk(
        &self,
        path: &PathUri,
        options: WalkOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<WalkOutcome> {
        trace!("remote fs walk");
        options.filters.validate()?;
        let client = self.client.get().await.map_err(map_remote_error)?;
        let response = match client
            .fs_walk(FsWalkParams {
                path: path.clone(),
                options: options.clone(),
                sandbox: remote_sandbox_context(sandbox),
            })
            .await
        {
            Ok(response) => response,
            Err(ExecServerError::Server {
                code: METHOD_NOT_FOUND_ERROR_CODE,
                ..
            }) => {
                return <Self as ExecutorFileSystem>::walk_via_directory_reads(
                    self, path, options, sandbox,
                )
                .await;
            }
            Err(error) => return Err(map_remote_error(error)),
        };
        if !options.follow_directory_symlinks && !response.no_follow_enforced {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "remote filesystem did not enforce no-follow enumeration; update the execution server",
            ));
        }
        if !options.filters.is_empty()
            && response.applied_filters.as_ref() != Some(&options.filters)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "remote filesystem did not apply requested walk filters; update the execution server or use an unfiltered audit",
            ));
        }
        if options.apply_gitignore && !response.gitignore_applied {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "remote filesystem did not apply gitignore rules; update it or set include_ignored=true",
            ));
        }
        if options.summary_only && response.summary.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "remote filesystem did not aggregate the walk; update the execution server",
            ));
        }
        if options.include_metadata && !response.metadata_included {
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported,
                "remote filesystem did not include requested entry metadata; update the execution server"));
        }
        Ok(response)
    }

    async fn remove(
        &self,
        path: &PathUri,
        options: RemoveOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        trace!("remote fs remove");
        let client = self.client.get().await.map_err(map_remote_error)?;
        client
            .fs_remove(FsRemoveParams {
                path: path.clone(),
                recursive: Some(options.recursive),
                force: Some(options.force),
                sandbox: remote_sandbox_context(sandbox),
            })
            .await
            .map_err(map_remote_error)?;
        Ok(())
    }

    async fn copy(
        &self,
        source_path: &PathUri,
        destination_path: &PathUri,
        options: CopyOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        trace!("remote fs copy");
        let client = self.client.get().await.map_err(map_remote_error)?;
        client
            .fs_copy(FsCopyParams {
                source_path: source_path.clone(),
                destination_path: destination_path.clone(),
                recursive: options.recursive,
                sandbox: remote_sandbox_context(sandbox),
            })
            .await
            .map_err(map_remote_error)?;
        Ok(())
    }
}

impl ExecutorFileSystem for RemoteFileSystem {
    fn read_file_range<'a>(
        &'a self, path: &'a PathUri, start: u64, len: usize,
        expected_revision: Option<&'a str>, sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, codex_file_system::FileRangeRead> {
        Box::pin(async move {
            let client = self.client.get().await.map_err(map_remote_error)?;
            // No old-peer stream fallback: it cannot bind a page to a revision.
            let response = client.fs_read_file_range(crate::protocol::FsReadFileRangeParams {
                path: path.clone(), start, len, expected_revision: expected_revision.map(str::to_owned),
                sandbox: remote_sandbox_context(sandbox),
            }).await.map_err(map_remote_error)?;
            decode_file_range(response, start, len, expected_revision)
        })
    }

    fn checksum_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<codex_file_system::FileDigest>> {
        Box::pin(async move {
            let client = self.client.get().await.map_err(map_remote_error)?;
            // Bind the call to one transport so an abandoned pass is cancelled
            // where it runs; later writes and starts there queue behind it.
            let rpc_client = client.rpc_client().await.map_err(map_remote_error)?;
            let cancel_id = uuid::Uuid::new_v4().simple().to_string();
            let mut cancel_on_drop =
                CancelChecksumOnDrop(Some((Arc::clone(&rpc_client), cancel_id.clone())));
            // Older peers refuse with Unsupported; no per-page or host fallback.
            let response: Result<crate::protocol::FsChecksumFileResponse, _> = client
                .call_rpc(
                    &rpc_client,
                    crate::protocol::FS_CHECKSUM_FILE_METHOD,
                    &crate::protocol::FsChecksumFileParams {
                        path: path.clone(),
                        sandbox: remote_sandbox_context(sandbox),
                        cancel_id: Some(cancel_id),
                    },
                )
                .await;
            cancel_on_drop.0 = None;
            validate_file_digest(response.map_err(map_remote_error)?.digest)
        })
    }

    fn mutate_file_if_matches<'a>(
        &'a self,
        path: &'a PathUri,
        expected: Option<Vec<u8>>,
        replacement: Option<Vec<u8>>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, codex_file_system::FileMutationOutcome> {
        Box::pin(RemoteFileSystem::mutate_file_if_matches(
            self,
            path,
            expected,
            replacement,
            sandbox,
        ))
    }

    fn directory_case_sensitive<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<bool>> {
        Box::pin(async move {
            let client = self.client.get().await.map_err(map_remote_error)?;
            let response = client
                .fs_get_metadata(FsGetMetadataParams {
                    path: path.clone(),
                    sandbox: remote_sandbox_context(sandbox),
                    include_directory_case_sensitivity: Some(true),
                })
                .await
                .map_err(map_remote_error)?;
            Ok(response.directory_case_sensitive)
        })
    }

    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(RemoteFileSystem::canonicalize(self, path, sandbox))
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(RemoteFileSystem::read_file(self, path, sandbox))
    }

    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(RemoteFileSystem::read_file_stream(self, path, sandbox))
    }

    fn read_file_metadata_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async move {
            if sandbox.is_some_and(FileSystemSandboxContext::should_run_in_sandbox) {
                let bytes = RemoteFileSystem::read_file(self, path, sandbox).await?;
                return Ok(FileSystemReadStream::new(futures::stream::once(
                    std::future::ready(Ok(bytes.into())),
                )));
            }
            Ok(RemoteFileSystem::read_file_stream(self, path, sandbox).await?
                .with_limit(MAX_FILE_PAYLOAD_BYTES as u64))
        })
    }

    fn read_file_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(RemoteFileSystem::read_file_bounded_request(
            self, path, /*confined_root*/ None, max_bytes, sandbox,
        ))
    }

    fn read_file_bounded_confined<'a>(
        &'a self,
        path: &'a PathUri,
        root: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(RemoteFileSystem::read_file_bounded_request(
            self,
            path,
            Some(root),
            max_bytes,
            sandbox,
        ))
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(RemoteFileSystem::write_file(self, path, contents, sandbox))
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(RemoteFileSystem::create_directory(
            self, path, options, sandbox,
        ))
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(RemoteFileSystem::get_metadata(self, path, sandbox))
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Box::pin(RemoteFileSystem::read_directory(self, path, sandbox))
    }

    fn walk<'a>(
        &'a self,
        path: &'a PathUri,
        options: WalkOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        Box::pin(RemoteFileSystem::walk(self, path, options, sandbox))
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(RemoteFileSystem::remove(self, path, options, sandbox))
    }

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(RemoteFileSystem::copy(
            self,
            source_path,
            destination_path,
            options,
            sandbox,
        ))
    }
}

/// Abandons an executor checksum whose call was dropped before its response.
/// Best effort: an unknown or finished id is a no-op on the executor, and an
/// older peer's method-not-found just leaves its pass to finish as before.
struct CancelChecksumOnDrop(Option<(Arc<RpcClient>, String)>);

impl Drop for CancelChecksumOnDrop {
    fn drop(&mut self) {
        let Some((rpc_client, cancel_id)) = self.0.take() else {
            return;
        };
        // Never recover or replay a connection just to cancel obsolete work.
        rpc_client.cleanup_runtime().spawn(async move {
            let _: Result<(), _> = rpc_client
                .call_for_cleanup(
                    crate::protocol::FS_CHECKSUM_FILE_CANCEL_METHOD,
                    &crate::protocol::FsChecksumFileCancelParams { cancel_id },
                    std::time::Duration::from_secs(5),
                )
                .await;
        });
    }
}

fn remote_sandbox_context(
    sandbox: Option<&FileSystemSandboxContext>,
) -> Option<FileSystemSandboxContext> {
    sandbox
        .cloned()
        .map(FileSystemSandboxContext::drop_cwd_if_unused)
}

fn map_remote_error(error: ExecServerError) -> io::Error {
    if let ExecServerError::Server { data, message, .. } = &error
        && let Some(kind) = codex_exec_server_protocol::file_system_error_kind(data.as_ref())
    {
        if let Some(copy) = data
            .as_ref()
            .and_then(|data| data.get("copy"))
            .and_then(|copy| {
                serde_json::from_value::<codex_file_system::CopyFailure>(copy.clone()).ok()
            })
        {
            return io::Error::new(kind, copy);
        }
        return io::Error::new(kind, message.clone());
    }
    match error {
        ExecServerError::Server { code, message, .. } if code == NOT_FOUND_ERROR_CODE => {
            io::Error::new(io::ErrorKind::NotFound, message)
        }
        ExecServerError::Server {
            code: INVALID_REQUEST_ERROR_CODE | INVALID_PARAMS_ERROR_CODE,
            message,
            ..
        } => io::Error::new(io::ErrorKind::InvalidInput, message),
        ExecServerError::Server {
            code: METHOD_NOT_FOUND_ERROR_CODE,
            message,
            ..
        } => io::Error::new(io::ErrorKind::Unsupported, message),
        ExecServerError::Closed | ExecServerError::Disconnected(_) => {
            io::Error::new(io::ErrorKind::BrokenPipe, "exec-server transport closed")
        }
        error if error.is_retryable_preparation_error() => {
            io::Error::new(io::ErrorKind::BrokenPipe, error.to_string())
        }
        ExecServerError::Server { message, .. } => io::Error::other(message),
        _ => io::Error::other(error.to_string()),
    }
}


