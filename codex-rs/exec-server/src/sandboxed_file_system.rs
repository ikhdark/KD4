use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use codex_exec_server_protocol::JSONRPCErrorError;
use codex_utils_path_uri::PathUri;
use tokio::io;

use crate::CopyOptions;
use crate::CreateDirectoryOptions;
use crate::ExecServerRuntimePaths;
use crate::ExecutorFileSystem;
use crate::ExecutorFileSystemFuture;
use crate::FileMetadata;
use crate::FileSystemReadStream;
use crate::FileSystemResult;
use crate::FileSystemSandboxContext;
use crate::ReadDirectoryEntry;
use crate::ReadDirectoryOutcome;
use crate::RemoveOptions;
use crate::WalkOptions;
use crate::WalkOutcome;
use crate::fs_helper::FS_PERMISSION_DENIED_ERROR_CODE;
use crate::fs_helper::FsHelperPayload;
use crate::fs_helper::FsHelperReadDirectoryBoundedParams;
use crate::fs_helper::FsHelperRequest;
use crate::fs_sandbox::FileSystemSandboxRunner;
use crate::protocol::FsCanonicalizeParams;
use crate::protocol::FsCopyParams;
use crate::protocol::FsCreateDirectoryParams;
use crate::protocol::FsGetMetadataParams;
use crate::protocol::FsMutateFileIfMatchesParams;
use crate::protocol::FsReadDirectoryParams;
use crate::protocol::FsReadFileBoundedParams;
use crate::protocol::FsReadFileParams;
use crate::protocol::FsRemoveParams;
use crate::protocol::FsWalkParams;
use crate::protocol::FsWriteFileParams;

#[derive(Clone)]
pub struct SandboxedFileSystem {
    sandbox_runner: FileSystemSandboxRunner,
}

impl SandboxedFileSystem {
    pub fn new(runtime_paths: ExecServerRuntimePaths) -> Self {
        Self {
            sandbox_runner: FileSystemSandboxRunner::new(runtime_paths),
        }
    }

    pub(crate) fn with_env(mut self, env: std::collections::HashMap<String, String>) -> Self {
        self.sandbox_runner = self.sandbox_runner.with_env(env);
        self
    }

    async fn run_sandboxed(
        &self,
        sandbox: &FileSystemSandboxContext,
        request: FsHelperRequest,
    ) -> FileSystemResult<FsHelperPayload> {
        self.sandbox_runner
            .run(sandbox, request)
            .await
            .map_err(map_sandbox_error)
    }
}

impl SandboxedFileSystem {
    async fn canonicalize(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<PathUri> {
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(path)?;
        let response = self
            .run_sandboxed(
                sandbox,
                FsHelperRequest::Canonicalize(FsCanonicalizeParams {
                    path: path.clone(),
                    sandbox: None,
                }),
            )
            .await?
            .expect_canonicalize()
            .map_err(map_sandbox_error)?;
        Ok(response.path)
    }

    async fn read_file(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Vec<u8>> {
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(path)?;
        let response = self
            .run_sandboxed(
                sandbox,
                FsHelperRequest::ReadFile(FsReadFileParams {
                    path: path.clone(),
                    sandbox: None,
                }),
            )
            .await?
            .expect_read_file()
            .map_err(map_sandbox_error)?;
        STANDARD.decode(response.data_base64).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("fs/readFile returned invalid base64 dataBase64: {err}"),
            )
        })
    }

    pub(crate) async fn read_file_capped(
        &self,
        path: &PathUri,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Vec<u8>> {
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(path)?;
        let response = self
            .run_sandboxed(
                sandbox,
                FsHelperRequest::ReadFileCapped {
                    path: path.clone(),
                    max_bytes,
                },
            )
            .await?
            .expect_read_file()
            .map_err(map_sandbox_error)?;
        STANDARD
            .decode(response.data_base64)
            .map_err(io::Error::other)
    }

    async fn read_file_bounded(
        &self,
        path: &PathUri,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Option<Vec<u8>>> {
        self.read_file_bounded_request(path, None, max_bytes, sandbox)
            .await
    }

    async fn read_file_bounded_with_source(
        &self,
        path: &PathUri,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Option<(Vec<u8>, Option<std::path::PathBuf>)>> {
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(path)?;
        let (data_base64, source) = self
            .run_sandboxed(
                sandbox,
                FsHelperRequest::ReadFileBoundedWithSource(FsReadFileBoundedParams {
                    path: path.clone(),
                    max_bytes,
                    confined_root: None,
                    sandbox: None,
                }),
            )
            .await?
            .expect_read_file_bounded_with_source()
            .map_err(map_sandbox_error)?;
        data_base64
            .map(|data| {
                STANDARD
                    .decode(data)
                    .map(|bytes| (bytes, source))
                    .map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid bounded source bytes: {error}"),
                        )
                    })
            })
            .transpose()
    }

    async fn read_file_bounded_confined(
        &self,
        path: &PathUri,
        root: &PathUri,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Option<Vec<u8>>> {
        self.read_file_bounded_request(path, Some(root), max_bytes, sandbox)
            .await
    }

    async fn read_file_bounded_request(
        &self,
        path: &PathUri,
        confined_root: Option<&PathUri>,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Option<Vec<u8>>> {
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(path)?;
        if let Some(root) = confined_root {
            validate_native_path(root)?;
        }
        let response = self
            .run_sandboxed(
                sandbox,
                FsHelperRequest::ReadFileBounded(FsReadFileBoundedParams {
                    path: path.clone(),
                    max_bytes,
                    confined_root: confined_root.cloned(),
                    sandbox: None,
                }),
            )
            .await?
            .expect_read_file_bounded()
            .map_err(map_sandbox_error)?;
        response
            .data_base64
            .map(|data_base64| {
                STANDARD.decode(data_base64).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("fs/readFileBounded returned invalid base64 dataBase64: {err}"),
                    )
                })
            })
            .transpose()
    }

    async fn write_file(
        &self,
        path: &PathUri,
        contents: Vec<u8>,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(path)?;
        self.run_sandboxed(
            sandbox,
            FsHelperRequest::WriteFile(FsWriteFileParams {
                path: path.clone(),
                data_base64: STANDARD.encode(contents),
                sandbox: None,
            }),
        )
        .await?
        .expect_write_file()
        .map_err(map_sandbox_error)?;
        Ok(())
    }

    async fn mutate_file_if_matches(
        &self,
        path: &PathUri,
        expected: Option<Vec<u8>>,
        replacement: Option<Vec<u8>>,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<codex_file_system::FileMutationOutcome> {
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(path)?;
        // The helper compares and mutates under the sandbox, keeping the
        // preimage check at the mutation boundary instead of in this process.
        let response = self
            .run_sandboxed(
                sandbox,
                FsHelperRequest::MutateFileIfMatches(FsMutateFileIfMatchesParams {
                    path: path.clone(),
                    expected_base64: expected.map(|bytes| STANDARD.encode(bytes)),
                    replacement_base64: replacement.map(|bytes| STANDARD.encode(bytes)),
                    sandbox: None,
                }),
            )
            .await?
            .expect_mutate_file_if_matches()
            .map_err(map_sandbox_error)?;
        Ok(response.outcome.into())
    }

    async fn create_directory(
        &self,
        path: &PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(path)?;
        self.run_sandboxed(
            sandbox,
            FsHelperRequest::CreateDirectory(FsCreateDirectoryParams {
                path: path.clone(),
                recursive: Some(options.recursive),
                sandbox: None,
            }),
        )
        .await?
        .expect_create_directory()
        .map_err(map_sandbox_error)?;
        Ok(())
    }

    async fn get_metadata(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<FileMetadata> {
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(path)?;
        let response = self
            .run_sandboxed(
                sandbox,
                FsHelperRequest::GetMetadata(FsGetMetadataParams {
                    path: path.clone(),
                    sandbox: None,
                    include_directory_case_sensitivity: None,
                }),
            )
            .await?
            .expect_get_metadata()
            .map_err(map_sandbox_error)?;
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
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(path)?;
        let response = self
            .run_sandboxed(
                sandbox,
                FsHelperRequest::ReadDirectory(FsReadDirectoryParams {
                    path: path.clone(),
                    sandbox: None,
                }),
            )
            .await?
            .expect_read_directory()
            .map_err(map_sandbox_error)?;
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

    async fn read_directory_bounded(
        &self,
        path: &PathUri,
        max_entries: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<ReadDirectoryOutcome> {
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(path)?;
        let response = self
            .run_sandboxed(
                sandbox,
                FsHelperRequest::ReadDirectoryBounded(FsHelperReadDirectoryBoundedParams {
                    path: path.clone(),
                    max_entries,
                }),
            )
            .await?
            .expect_read_directory_bounded()
            .map_err(map_sandbox_error)?;
        Ok(ReadDirectoryOutcome {
            entries: response
                .entries
                .into_iter()
                .map(|entry| ReadDirectoryEntry {
                    file_name: entry.file_name,
                    is_directory: entry.is_directory,
                    is_file: entry.is_file,
                })
                .collect(),
            entries_examined: response.entries_examined,
            limit_reached: response.limit_reached,
        })
    }

    async fn walk(
        &self,
        path: &PathUri,
        options: WalkOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<WalkOutcome> {
        options.filters.validate()?;
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(path)?;
        let response = self
            .run_sandboxed(
                sandbox,
                FsHelperRequest::Walk(FsWalkParams {
                    path: path.clone(),
                    options: options.clone(),
                    sandbox: None,
                }),
            )
            .await?
            .expect_walk()
            .map_err(map_sandbox_error)?;
        if !options.follow_directory_symlinks && !response.no_follow_enforced {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "filesystem helper did not enforce no-follow enumeration; update the helper",
            ));
        }
        if !options.filters.is_empty()
            && response.applied_filters.as_ref() != Some(&options.filters)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "filesystem helper did not apply requested walk filters; update the helper or use an unfiltered audit",
            ));
        }
        if options.apply_gitignore && !response.gitignore_applied {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "filesystem helper did not apply gitignore rules; update it or set include_ignored=true",
            ));
        }
        if options.summary_only && response.summary.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "filesystem helper did not aggregate the walk; update the helper",
            ));
        }
        if options.include_metadata && !response.metadata_included {
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported,
                "filesystem helper did not include requested entry metadata; update the helper"));
        }
        Ok(response)
    }

    async fn remove(
        &self,
        path: &PathUri,
        remove_options: RemoveOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(path)?;
        self.run_sandboxed(
            sandbox,
            FsHelperRequest::Remove(FsRemoveParams {
                path: path.clone(),
                recursive: Some(remove_options.recursive),
                force: Some(remove_options.force),
                sandbox: None,
            }),
        )
        .await?
        .expect_remove()
        .map_err(map_sandbox_error)?;
        Ok(())
    }

    async fn copy(
        &self,
        source_path: &PathUri,
        destination_path: &PathUri,
        options: CopyOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        let sandbox = require_platform_sandbox(sandbox)?;
        validate_native_path(source_path)?;
        validate_native_path(destination_path)?;
        self.run_sandboxed(
            sandbox,
            FsHelperRequest::Copy(FsCopyParams {
                source_path: source_path.clone(),
                destination_path: destination_path.clone(),
                recursive: options.recursive,
                sandbox: None,
            }),
        )
        .await?
        .expect_copy()
        .map_err(map_sandbox_error)?;
        Ok(())
    }
}

impl ExecutorFileSystem for SandboxedFileSystem {
    fn read_file_range<'a>(
        &'a self, path: &'a PathUri, start: u64, len: usize,
        expected_revision: Option<&'a str>, sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, codex_file_system::FileRangeRead> {
        Box::pin(async move {
            let sandbox = require_platform_sandbox(sandbox)?;
            validate_native_path(path)?;
            let response = self.run_sandboxed(sandbox,
                FsHelperRequest::ReadFileRange(crate::protocol::FsReadFileRangeParams {
                    path: path.clone(), start, len, expected_revision: expected_revision.map(str::to_owned), sandbox: None,
                })).await?.expect_read_file_range().map_err(map_sandbox_error)?;
            crate::remote_file_system::decode_file_range(response, start, len, expected_revision)
        })
    }

    /// One helper launch hashes the whole file; dropping this future
    /// terminates the helper, so cancellation does not wait for the pass.
    fn checksum_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<codex_file_system::FileDigest>> {
        Box::pin(async move {
            let sandbox = require_platform_sandbox(sandbox)?;
            validate_native_path(path)?;
            let response = self
                .run_sandboxed(
                    sandbox,
                    FsHelperRequest::ChecksumFile(crate::protocol::FsChecksumFileParams {
                        path: path.clone(),
                        sandbox: None,
                        cancel_id: None,
                    }),
                )
                .await?
                .expect_checksum_file()
                .map_err(map_sandbox_error)?;
            crate::remote_file_system::validate_file_digest(response.digest)
        })
    }

    /// One helper launch probes every candidate instead of one per candidate.
    fn first_regular_file<'a>(
        &'a self,
        candidates: &'a [PathUri],
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<PathUri>> {
        Box::pin(async move {
            let sandbox = require_platform_sandbox(sandbox)?;
            if candidates.is_empty() {
                return Ok(None);
            }
            for candidate in candidates {
                validate_native_path(candidate)?;
            }
            self.run_sandboxed(
                sandbox,
                FsHelperRequest::FirstRegularFile {
                    candidates: candidates.to_vec(),
                },
            )
            .await?
            .expect_first_regular_file()
            .map_err(map_sandbox_error)
        })
    }

    fn mutate_file_if_matches<'a>(
        &'a self,
        path: &'a PathUri,
        expected: Option<Vec<u8>>,
        replacement: Option<Vec<u8>>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, codex_file_system::FileMutationOutcome> {
        Box::pin(SandboxedFileSystem::mutate_file_if_matches(
            self,
            path,
            expected,
            replacement,
            sandbox,
        ))
    }

    fn uses_host_native_paths(&self) -> bool {
        true
    }

    fn directory_case_sensitive<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<bool>> {
        Box::pin(async move {
            let sandbox = require_platform_sandbox(sandbox)?;
            validate_native_path(path)?;
            let response = self
                .run_sandboxed(
                    sandbox,
                    FsHelperRequest::GetMetadata(FsGetMetadataParams {
                        path: path.clone(),
                        sandbox: None,
                        include_directory_case_sensitivity: Some(true),
                    }),
                )
                .await?
                .expect_get_metadata()
                .map_err(map_sandbox_error)?;
            Ok(response.directory_case_sensitive)
        })
    }

    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(SandboxedFileSystem::canonicalize(self, path, sandbox))
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(SandboxedFileSystem::read_file(self, path, sandbox))
    }

    fn read_file_stream<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "streaming file reads do not support platform sandboxing",
            ))
        })
    }

    fn read_file_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(SandboxedFileSystem::read_file_bounded(
            self, path, max_bytes, sandbox,
        ))
    }

    fn read_file_bounded_with_source<'a>(
        &'a self,
        path: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<(Vec<u8>, Option<std::path::PathBuf>)>> {
        Box::pin(SandboxedFileSystem::read_file_bounded_with_source(
            self, path, max_bytes, sandbox,
        ))
    }

    fn read_file_bounded_confined<'a>(
        &'a self,
        path: &'a PathUri,
        root: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(SandboxedFileSystem::read_file_bounded_confined(
            self, path, root, max_bytes, sandbox,
        ))
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(SandboxedFileSystem::write_file(
            self, path, contents, sandbox,
        ))
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(SandboxedFileSystem::create_directory(
            self, path, options, sandbox,
        ))
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(SandboxedFileSystem::get_metadata(self, path, sandbox))
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Box::pin(SandboxedFileSystem::read_directory(self, path, sandbox))
    }

    fn read_directory_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        max_entries: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome> {
        Box::pin(SandboxedFileSystem::read_directory_bounded(
            self,
            path,
            max_entries,
            sandbox,
        ))
    }

    fn walk<'a>(
        &'a self,
        path: &'a PathUri,
        options: WalkOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        Box::pin(SandboxedFileSystem::walk(self, path, options, sandbox))
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        remove_options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(SandboxedFileSystem::remove(
            self,
            path,
            remove_options,
            sandbox,
        ))
    }

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(SandboxedFileSystem::copy(
            self,
            source_path,
            destination_path,
            options,
            sandbox,
        ))
    }
}

fn validate_native_path(path: &PathUri) -> FileSystemResult<()> {
    path.to_abs_path().map(drop)
}

fn require_platform_sandbox(
    sandbox: Option<&FileSystemSandboxContext>,
) -> FileSystemResult<&FileSystemSandboxContext> {
    sandbox
        .filter(|sandbox| sandbox.should_run_in_sandbox())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "sandboxed filesystem operations require ReadOnly or WorkspaceWrite sandbox policy",
            )
        })
}

fn map_sandbox_error(error: JSONRPCErrorError) -> io::Error {
    if let Some(kind) = codex_exec_server_protocol::file_system_error_kind(error.data.as_ref()) {
        if let Some(copy) = error
            .data
            .as_ref()
            .and_then(|data| data.get("copy"))
            .and_then(|copy| {
                serde_json::from_value::<codex_file_system::CopyFailure>(copy.clone()).ok()
            })
        {
            return io::Error::new(kind, copy);
        }
        return io::Error::new(kind, error.message);
    }
    match error.code {
        -32601 => io::Error::new(io::ErrorKind::Unsupported, error.message),
        -32004 => io::Error::new(io::ErrorKind::NotFound, error.message),
        FS_PERMISSION_DENIED_ERROR_CODE => {
            io::Error::new(io::ErrorKind::PermissionDenied, error.message)
        }
        -32600 | -32602 => io::Error::new(io::ErrorKind::InvalidInput, error.message),
        _ => io::Error::other(error.message),
    }
}

