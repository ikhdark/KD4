use codex_file_system::WalkDirectoryEntry;
use codex_file_system::WalkEntryMetadata;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::io;
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;

use crate::CopyOptions;
use crate::CreateDirectoryOptions;
use crate::ExecServerRuntimePaths;
use crate::ExecutorFileSystem;
use crate::ExecutorFileSystemFuture;
use crate::FILE_READ_CHUNK_SIZE;
use crate::FileMetadata;
use crate::FileSystemReadStream;
use crate::FileSystemResult;
use crate::FileSystemSandboxContext;
use crate::ReadDirectoryEntry;
use crate::ReadDirectoryOutcome;
use crate::RemoveOptions;
use crate::WalkOptions;
use crate::WalkOutcome;
use crate::regular_file;
use crate::sandboxed_file_system::SandboxedFileSystem;

const MAX_READ_FILE_BYTES: u64 = 512 * 1024 * 1024;

fn file_too_large_error(max_bytes: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("file is too large to read: {max_bytes}-byte read response limit"),
    )
}

pub static LOCAL_FS: LazyLock<Arc<dyn ExecutorFileSystem>> =
    LazyLock::new(|| -> Arc<dyn ExecutorFileSystem> { Arc::new(LocalFileSystem::unsandboxed()) });

#[derive(Clone, Default)]
pub(crate) struct DirectFileSystem;

#[derive(Clone, Default)]
pub(crate) struct UnsandboxedFileSystem {
    file_system: DirectFileSystem,
}

#[derive(Clone, Default)]
pub struct LocalFileSystem {
    unsandboxed: UnsandboxedFileSystem,
    sandboxed: Option<SandboxedFileSystem>,
}

impl LocalFileSystem {
    pub fn unsandboxed() -> Self {
        Self {
            unsandboxed: UnsandboxedFileSystem::default(),
            sandboxed: None,
        }
    }

    pub fn with_runtime_paths(runtime_paths: ExecServerRuntimePaths) -> Self {
        Self {
            unsandboxed: UnsandboxedFileSystem::default(),
            sandboxed: Some(SandboxedFileSystem::new(runtime_paths)),
        }
    }

    /// Override the helper's inherited environment without changing process-global state.
    pub fn with_runtime_paths_and_env(
        runtime_paths: ExecServerRuntimePaths,
        env: std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            unsandboxed: UnsandboxedFileSystem::default(),
            sandboxed: Some(SandboxedFileSystem::new(runtime_paths).with_env(env)),
        }
    }

    fn sandboxed(&self) -> io::Result<&SandboxedFileSystem> {
        self.sandboxed.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "sandboxed filesystem operations require configured runtime paths",
            )
        })
    }

    fn file_system_for<'a>(
        &'a self,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> io::Result<(
        &'a dyn ExecutorFileSystem,
        Option<&'a FileSystemSandboxContext>,
    )> {
        if sandbox.is_some_and(FileSystemSandboxContext::should_run_in_sandbox) {
            Ok((self.sandboxed()?, sandbox))
        } else {
            Ok((&self.unsandboxed, sandbox))
        }
    }
}

impl LocalFileSystem {
    pub(crate) async fn read_file_capped(
        &self,
        path: &PathUri,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Vec<u8>> {
        if sandbox.is_some_and(FileSystemSandboxContext::should_run_in_sandbox) {
            return self
                .sandboxed()?
                .read_file_capped(path, max_bytes, sandbox)
                .await;
        }
        reject_platform_sandbox_context(sandbox)?;
        self.unsandboxed
            .file_system
            .read_file_capped(path, max_bytes, None)
            .await
    }

    pub(crate) async fn open_file_for_read(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<tokio::fs::File> {
        if sandbox.is_some_and(FileSystemSandboxContext::should_run_in_sandbox) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "streaming file reads do not support platform sandboxing",
            ));
        }
        self.unsandboxed.open_file_for_read(path, sandbox).await
    }

    async fn canonicalize(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<PathUri> {
        let (file_system, sandbox) = self.file_system_for(sandbox)?;
        file_system.canonicalize(path, sandbox).await
    }

    async fn read_file(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Vec<u8>> {
        let (file_system, sandbox) = self.file_system_for(sandbox)?;
        file_system.read_file(path, sandbox).await
    }

    async fn read_file_bounded(
        &self,
        path: &PathUri,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Option<Vec<u8>>> {
        let (file_system, sandbox) = self.file_system_for(sandbox)?;
        file_system
            .read_file_bounded(path, max_bytes, sandbox)
            .await
    }

    async fn read_file_bounded_confined(
        &self,
        path: &PathUri,
        root: &PathUri,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Option<Vec<u8>>> {
        let (file_system, sandbox) = self.file_system_for(sandbox)?;
        file_system
            .read_file_bounded_confined(path, root, max_bytes, sandbox)
            .await
    }

    async fn read_file_stream(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<FileSystemReadStream> {
        let (file_system, sandbox) = self.file_system_for(sandbox)?;
        file_system.read_file_stream(path, sandbox).await
    }

    async fn write_file(
        &self,
        path: &PathUri,
        contents: Vec<u8>,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        let (file_system, sandbox) = self.file_system_for(sandbox)?;
        file_system.write_file(path, contents, sandbox).await
    }

    async fn create_directory(
        &self,
        path: &PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        let (file_system, sandbox) = self.file_system_for(sandbox)?;
        file_system.create_directory(path, options, sandbox).await
    }

    async fn get_metadata(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<FileMetadata> {
        let (file_system, sandbox) = self.file_system_for(sandbox)?;
        file_system.get_metadata(path, sandbox).await
    }

    async fn read_directory(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Vec<ReadDirectoryEntry>> {
        let (file_system, sandbox) = self.file_system_for(sandbox)?;
        file_system.read_directory(path, sandbox).await
    }

    async fn read_directory_bounded(
        &self,
        path: &PathUri,
        max_entries: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<ReadDirectoryOutcome> {
        let (file_system, sandbox) = self.file_system_for(sandbox)?;
        file_system
            .read_directory_bounded(path, max_entries, sandbox)
            .await
    }

    async fn walk(
        &self,
        path: &PathUri,
        options: WalkOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<WalkOutcome> {
        let (file_system, sandbox) = self.file_system_for(sandbox)?;
        file_system.walk(path, options, sandbox).await
    }

    async fn remove(
        &self,
        path: &PathUri,
        options: RemoveOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        let (file_system, sandbox) = self.file_system_for(sandbox)?;
        file_system.remove(path, options, sandbox).await
    }

    async fn copy(
        &self,
        source_path: &PathUri,
        destination_path: &PathUri,
        options: CopyOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        let (file_system, sandbox) = self.file_system_for(sandbox)?;
        file_system
            .copy(source_path, destination_path, options, sandbox)
            .await
    }
}

impl ExecutorFileSystem for LocalFileSystem {
    fn lookup_batch<'a>(
        &'a self,
        requests: &'a [codex_file_system::FileSystemLookup],
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<codex_file_system::FileSystemLookupResult>> {
        Box::pin(async move {
            codex_file_system::validate_lookup_batch(requests)?;
            let (fs, sandbox) = self.file_system_for(sandbox)?;
            fs.lookup_batch(requests, sandbox).await
        })
    }

    fn read_file_range<'a>(
        &'a self,
        path: &'a PathUri,
        start: u64,
        len: usize,
        expected_revision: Option<&'a str>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, codex_file_system::FileRangeRead> {
        Box::pin(async move {
            let (fs, sandbox) = self.file_system_for(sandbox)?;
            fs.read_file_range(path, start, len, expected_revision, sandbox)
                .await
        })
    }

    fn checksum_file<'a>(
        &'a self,
        path: &'a PathUri,
        include_text_diagnostics: bool,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<codex_file_system::FileDigest>> {
        Box::pin(async move {
            let (fs, sandbox) = self.file_system_for(sandbox)?;
            fs.checksum_file(path, include_text_diagnostics, sandbox).await
        })
    }

    fn first_regular_file<'a>(
        &'a self,
        candidates: &'a [PathUri],
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<PathUri>> {
        Box::pin(async move {
            let (fs, sandbox) = self.file_system_for(sandbox)?;
            fs.first_regular_file(candidates, sandbox).await
        })
    }

    fn mutate_file_if_matches<'a>(
        &'a self,
        path: &'a PathUri,
        expected: Option<Vec<u8>>,
        replacement: Option<Vec<u8>>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, codex_file_system::FileMutationOutcome> {
        Box::pin(async move {
            let (fs, sandbox) = self.file_system_for(sandbox)?;
            fs.mutate_file_if_matches(path, expected, replacement, sandbox)
                .await
        })
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
            let (file_system, sandbox) = self.file_system_for(sandbox)?;
            file_system.directory_case_sensitive(path, sandbox).await
        })
    }

    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(LocalFileSystem::canonicalize(self, path, sandbox))
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(LocalFileSystem::read_file(self, path, sandbox))
    }

    fn read_file_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(LocalFileSystem::read_file_bounded(
            self, path, max_bytes, sandbox,
        ))
    }

    fn read_file_bounded_with_source<'a>(
        &'a self,
        path: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<(Vec<u8>, Option<PathBuf>)>> {
        Box::pin(async move {
            let (file_system, sandbox) = self.file_system_for(sandbox)?;
            file_system
                .read_file_bounded_with_source(path, max_bytes, sandbox)
                .await
        })
    }

    fn read_file_bounded_confined<'a>(
        &'a self,
        path: &'a PathUri,
        root: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(LocalFileSystem::read_file_bounded_confined(
            self, path, root, max_bytes, sandbox,
        ))
    }

    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(LocalFileSystem::read_file_stream(self, path, sandbox))
    }

    fn read_file_metadata_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async move {
            let (file_system, sandbox) = self.file_system_for(sandbox)?;
            file_system.read_file_metadata_stream(path, sandbox).await
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(LocalFileSystem::write_file(self, path, contents, sandbox))
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(LocalFileSystem::create_directory(
            self, path, options, sandbox,
        ))
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(LocalFileSystem::get_metadata(self, path, sandbox))
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Box::pin(LocalFileSystem::read_directory(self, path, sandbox))
    }

    fn read_directory_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        max_entries: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome> {
        Box::pin(LocalFileSystem::read_directory_bounded(
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
        Box::pin(LocalFileSystem::walk(self, path, options, sandbox))
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(LocalFileSystem::remove(self, path, options, sandbox))
    }

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(LocalFileSystem::copy(
            self,
            source_path,
            destination_path,
            options,
            sandbox,
        ))
    }
}

impl UnsandboxedFileSystem {
    async fn open_file_for_read(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<tokio::fs::File> {
        reject_platform_sandbox_context(sandbox)?;
        self.file_system
            .open_file_for_read(path, /*sandbox*/ None)
            .await
    }

    async fn canonicalize(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<PathUri> {
        reject_platform_sandbox_context(sandbox)?;
        self.file_system.canonicalize(path, /*sandbox*/ None).await
    }

    async fn read_file(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Vec<u8>> {
        reject_platform_sandbox_context(sandbox)?;
        self.file_system.read_file(path, /*sandbox*/ None).await
    }

    async fn read_file_bounded(
        &self,
        path: &PathUri,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Option<Vec<u8>>> {
        reject_platform_sandbox_context(sandbox)?;
        self.file_system
            .read_file_bounded(path, max_bytes, /*sandbox*/ None)
            .await
    }

    async fn read_file_bounded_confined(
        &self,
        path: &PathUri,
        root: &PathUri,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Option<Vec<u8>>> {
        reject_platform_sandbox_context(sandbox)?;
        self.file_system
            .read_file_bounded_confined(path, root, max_bytes, /*sandbox*/ None)
            .await
    }

    async fn read_file_stream(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<FileSystemReadStream> {
        reject_platform_sandbox_context(sandbox)?;
        self.file_system
            .read_file_stream(path, /*sandbox*/ None)
            .await
    }

    async fn write_file(
        &self,
        path: &PathUri,
        contents: Vec<u8>,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        reject_platform_sandbox_context(sandbox)?;
        self.file_system
            .write_file(path, contents, /*sandbox*/ None)
            .await
    }

    async fn create_directory(
        &self,
        path: &PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        reject_platform_sandbox_context(sandbox)?;
        self.file_system
            .create_directory(path, options, /*sandbox*/ None)
            .await
    }

    async fn get_metadata(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<FileMetadata> {
        reject_platform_sandbox_context(sandbox)?;
        self.file_system.get_metadata(path, /*sandbox*/ None).await
    }

    async fn read_directory(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Vec<ReadDirectoryEntry>> {
        reject_platform_sandbox_context(sandbox)?;
        self.file_system
            .read_directory(path, /*sandbox*/ None)
            .await
    }

    async fn read_directory_bounded(
        &self,
        path: &PathUri,
        max_entries: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<ReadDirectoryOutcome> {
        reject_platform_sandbox_context(sandbox)?;
        self.file_system
            .read_directory_bounded(path, max_entries, /*sandbox*/ None)
            .await
    }

    async fn remove(
        &self,
        path: &PathUri,
        options: RemoveOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        reject_platform_sandbox_context(sandbox)?;
        self.file_system
            .remove(path, options, /*sandbox*/ None)
            .await
    }

    async fn copy(
        &self,
        source_path: &PathUri,
        destination_path: &PathUri,
        options: CopyOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        reject_platform_sandbox_context(sandbox)?;
        self.file_system
            .copy(
                source_path,
                destination_path,
                options,
                /*sandbox*/ None,
            )
            .await
    }
}

impl ExecutorFileSystem for UnsandboxedFileSystem {
    fn read_file_range<'a>(
        &'a self,
        path: &'a PathUri,
        start: u64,
        len: usize,
        expected_revision: Option<&'a str>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, codex_file_system::FileRangeRead> {
        Box::pin(async move {
            reject_platform_sandbox_context(sandbox)?;
            self.file_system
                .read_file_range(path, start, len, expected_revision, None)
                .await
        })
    }

    fn checksum_file<'a>(
        &'a self,
        path: &'a PathUri,
        include_text_diagnostics: bool,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<codex_file_system::FileDigest>> {
        Box::pin(async move {
            reject_platform_sandbox_context(sandbox)?;
            self.file_system.checksum_file(path, include_text_diagnostics, None).await
        })
    }

    fn mutate_file_if_matches<'a>(
        &'a self,
        path: &'a PathUri,
        expected: Option<Vec<u8>>,
        replacement: Option<Vec<u8>>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, codex_file_system::FileMutationOutcome> {
        Box::pin(async move {
            reject_platform_sandbox_context(sandbox)?;
            self.file_system
                .mutate_file_if_matches(path, expected, replacement, None)
                .await
        })
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
            reject_platform_sandbox_context(sandbox)?;
            self.file_system.directory_case_sensitive(path, None).await
        })
    }

    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(UnsandboxedFileSystem::canonicalize(self, path, sandbox))
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(UnsandboxedFileSystem::read_file(self, path, sandbox))
    }

    fn read_file_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(UnsandboxedFileSystem::read_file_bounded(
            self, path, max_bytes, sandbox,
        ))
    }

    fn read_file_bounded_with_source<'a>(
        &'a self,
        path: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<(Vec<u8>, Option<PathBuf>)>> {
        Box::pin(async move {
            reject_platform_sandbox_context(sandbox)?;
            self.file_system
                .read_file_bounded_with_source(path, max_bytes, None)
                .await
        })
    }

    fn read_file_bounded_confined<'a>(
        &'a self,
        path: &'a PathUri,
        root: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(UnsandboxedFileSystem::read_file_bounded_confined(
            self, path, root, max_bytes, sandbox,
        ))
    }

    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(UnsandboxedFileSystem::read_file_stream(self, path, sandbox))
    }

    fn read_file_metadata_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async move {
            reject_platform_sandbox_context(sandbox)?;
            self.file_system.read_file_metadata_stream(path, None).await
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(UnsandboxedFileSystem::write_file(
            self, path, contents, sandbox,
        ))
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(UnsandboxedFileSystem::create_directory(
            self, path, options, sandbox,
        ))
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(UnsandboxedFileSystem::get_metadata(self, path, sandbox))
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Box::pin(UnsandboxedFileSystem::read_directory(self, path, sandbox))
    }

    fn read_directory_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        max_entries: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome> {
        Box::pin(UnsandboxedFileSystem::read_directory_bounded(
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
        Box::pin(async move {
            reject_platform_sandbox_context(sandbox)?;
            self.file_system.walk(path, options, None).await
        })
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(UnsandboxedFileSystem::remove(self, path, options, sandbox))
    }

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(UnsandboxedFileSystem::copy(
            self,
            source_path,
            destination_path,
            options,
            sandbox,
        ))
    }
}

impl DirectFileSystem {
    async fn open_file_for_read(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<tokio::fs::File> {
        reject_sandbox_context(sandbox)?;
        let path = path.to_abs_path()?;
        regular_file::open(path.as_path()).await
    }

    async fn canonicalize(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<PathUri> {
        reject_sandbox_context(sandbox)?;
        let path = path.to_abs_path()?;
        let canonicalized =
            AbsolutePathBuf::from_absolute_path(tokio::fs::canonicalize(path.as_path()).await?)?;
        Ok(PathUri::from_abs_path(&canonicalized))
    }

    async fn read_file(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Vec<u8>> {
        reject_sandbox_context(sandbox)?;
        self.read_file_capped(path, MAX_READ_FILE_BYTES as usize, None)
            .await
    }

    pub(crate) async fn read_file_capped(
        &self,
        path: &PathUri,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Vec<u8>> {
        reject_sandbox_context(sandbox)?;
        let max_bytes = (max_bytes as u64).min(MAX_READ_FILE_BYTES);
        let path = path.to_abs_path()?;
        let (file, metadata) = regular_file::open_with_metadata(path.as_path()).await?;
        read_file_payload_capped(file, metadata.len(), max_bytes).await
    }

    async fn read_file_bounded(
        &self,
        path: &PathUri,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Option<Vec<u8>>> {
        reject_sandbox_context(sandbox)?;
        let path = path.to_abs_path()?;
        run_cancellable_file_system_task(move |cancel| {
            read_bounded_file_sync(
                || regular_file::open_sync(path.as_path()),
                max_bytes,
                &cancel,
            )
        })
        .await
    }

    async fn read_file_bounded_confined(
        &self,
        path: &PathUri,
        root: &PathUri,
        max_bytes: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Option<Vec<u8>>> {
        reject_sandbox_context(sandbox)?;
        let path = path.to_abs_path()?;
        let root = root.to_abs_path()?;
        run_cancellable_file_system_task(move |cancel| {
            check_file_system_cancelled(&cancel)?;
            let canonical_root = std::fs::canonicalize(root.as_path())?;
            let _capture = codex_file_system::guard_patch_read(path.as_path())?;
            let canonical_path = std::fs::canonicalize(path.as_path())?;
            if !canonical_path.starts_with(&canonical_root) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "file resolves outside the confined root",
                ));
            }
            read_bounded_file_sync(
                || {
                    codex_file_system::open_confined_file_with_metadata(
                        &canonical_root,
                        &canonical_path,
                    )
                },
                max_bytes,
                &cancel,
            )
        })
        .await
    }

    async fn read_file_stream(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<FileSystemReadStream> {
        let file = self.open_file_for_read(path, sandbox).await?;
        Ok(FileSystemReadStream::new(ReaderStream::with_capacity(
            file,
            FILE_READ_CHUNK_SIZE,
        )))
    }

    async fn write_file(
        &self,
        path: &PathUri,
        contents: Vec<u8>,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        reject_sandbox_context(sandbox)?;
        let path = path.to_abs_path()?;
        tokio::task::spawn_blocking(move || {
            let paths = codex_file_system::resolve_symlink_write_paths(path.as_path())?;
            codex_file_system::write_bytes_atomically_without_sync(&paths.write_path, &contents)
        })
        .await
        .map_err(io::Error::other)?
    }

    async fn create_directory(
        &self,
        path: &PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        reject_sandbox_context(sandbox)?;
        let path = path.to_abs_path()?;
        if options.recursive {
            tokio::fs::create_dir_all(path.as_path()).await?;
        } else {
            tokio::fs::create_dir(path.as_path()).await?;
        }
        Ok(())
    }

    async fn get_metadata(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<FileMetadata> {
        reject_sandbox_context(sandbox)?;
        let path = path.to_abs_path()?;
        // One blocking task. A path that is not a link is its own target, so
        // only links need a second, target-following query.
        let (metadata, is_symlink) = tokio::task::spawn_blocking(move || {
            let _capture = codex_file_system::guard_patch_read(path.as_path())?;
            let symlink_metadata = std::fs::symlink_metadata(path.as_path())?;
            if symlink_metadata.file_type().is_symlink() {
                // Preserve a known link entry when only its target is absent.
                // Other target errors (notably permission/cycle errors) remain errors.
                let metadata = match std::fs::metadata(path.as_path()) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => symlink_metadata,
                    Err(error) => return Err(error),
                };
                Ok::<_, io::Error>((metadata, true))
            } else {
                Ok((symlink_metadata, false))
            }
        })
        .await
        .map_err(io::Error::other)??;
        Ok(file_metadata(&metadata, is_symlink))
    }

    async fn read_directory(
        &self,
        path: &PathUri,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<Vec<ReadDirectoryEntry>> {
        reject_sandbox_context(sandbox)?;
        let path = path.to_abs_path()?;
        run_cancellable_file_system_task(move |cancel| {
            read_directory_sync(path.as_path(), None, &cancel, true, directory_entry)
        })
        .await
        .map(|outcome| outcome.entries)
    }

    async fn read_directory_bounded(
        &self,
        path: &PathUri,
        max_entries: usize,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<ReadDirectoryOutcome> {
        reject_sandbox_context(sandbox)?;
        if max_entries == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bounded directory read limit must be greater than zero",
            ));
        }
        let path = path.to_abs_path()?;
        run_cancellable_file_system_task(move |cancel| {
            read_directory_sync(
                path.as_path(),
                Some(max_entries),
                &cancel,
                true,
                directory_entry,
            )
        })
        .await
    }

    async fn remove(
        &self,
        path: &PathUri,
        options: RemoveOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        reject_sandbox_context(sandbox)?;
        let path = path.to_abs_path()?;
        // Metadata and deletion share one worker handoff. Check cancellation
        // again before the destructive step, as the old await boundary allowed.
        run_cancellable_file_system_task(move |cancel| {
            remove_sync(path.as_path(), options, || {
                check_file_system_cancelled(&cancel)
            })
        })
        .await
    }

    async fn copy(
        &self,
        source_path: &PathUri,
        destination_path: &PathUri,
        options: CopyOptions,
        sandbox: Option<&FileSystemSandboxContext>,
    ) -> FileSystemResult<()> {
        reject_sandbox_context(sandbox)?;
        let source_path = source_path.to_abs_path()?.into_path_buf();
        let destination_path = destination_path.to_abs_path()?.into_path_buf();
        run_cancellable_file_system_task(move |cancel| {
            let metadata = std::fs::symlink_metadata(source_path.as_path())?;
            let file_type = metadata.file_type();

            if file_type.is_dir() {
                if !options.recursive {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "fs/copy requires recursive: true when sourcePath is a directory",
                    ));
                }
                let source_root = std::fs::canonicalize(&source_path)?;
                copy_dir_recursive(&source_path, &destination_path, &source_root, &cancel)?;
                return Ok(());
            }

            if file_type.is_symlink() {
                check_file_system_cancelled(&cancel)?;
                copy_symlink(source_path.as_path(), destination_path.as_path())?;
                return Ok(());
            }

            if file_type.is_file() {
                check_file_system_cancelled(&cancel)?;
                std::fs::copy(source_path.as_path(), destination_path.as_path())?;
                return Ok(());
            }

            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fs/copy only supports regular files, directories, and symlinks",
            ))
        })
        .await
    }
}

impl ExecutorFileSystem for DirectFileSystem {
    fn read_file_range<'a>(
        &'a self,
        path: &'a PathUri,
        start: u64,
        len: usize,
        expected_revision: Option<&'a str>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, codex_file_system::FileRangeRead> {
        Box::pin(async move {
            reject_sandbox_context(sandbox)?;
            if !(1..=codex_file_system::FILE_RANGE_MAX_BYTES).contains(&len)
                || start.checked_add(len as u64).is_none()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid byte range length or offset",
                ));
            }
            let path = path.to_abs_path()?;
            let expected = expected_revision.map(str::to_owned);
            run_cancellable_file_system_task(move |cancel| {
                read_file_range_sync(path.as_path(), start, len, expected.as_deref(), &cancel)
            })
            .await
        })
    }

    fn checksum_file<'a>(
        &'a self,
        path: &'a PathUri,
        include_text_diagnostics: bool,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<codex_file_system::FileDigest>> {
        Box::pin(async move {
            reject_sandbox_context(sandbox)?;
            let path = path.to_abs_path()?;
            run_cancellable_file_system_task(move |cancel| {
                checksum_file_sync(path.as_path(), include_text_diagnostics, &cancel)
            })
            .await
        })
    }

    fn mutate_file_if_matches<'a>(
        &'a self,
        path: &'a PathUri,
        expected: Option<Vec<u8>>,
        replacement: Option<Vec<u8>>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, codex_file_system::FileMutationOutcome> {
        Box::pin(async move {
            reject_sandbox_context(sandbox)?;
            let path = path.to_abs_path()?;
            // Like ordinary writes, this owned worker must finish after caller
            // cancellation so publication/recovery can be reconciled truthfully.
            tokio::task::spawn_blocking(move || {
                // Preserve existing final-file-symlink write semantics. The
                // patch delta remains inexact for that nonordinary endpoint.
                let target = if replacement.is_some() {
                    codex_file_system::resolve_symlink_write_paths(path.as_path())?.write_path
                } else {
                    path.as_path().to_path_buf()
                };
                codex_file_system::mutate_file_if_matches(
                    &target,
                    expected.as_deref(),
                    replacement.as_deref(),
                )
            })
            .await
            .map_err(io::Error::other)?
        })
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
            reject_sandbox_context(sandbox)?;
            let path = path.to_abs_path()?;
            run_cancellable_file_system_task(move |cancel| {
                check_file_system_cancelled(&cancel)?;
                codex_file_system::directory_case_sensitive(path.as_path())
            })
            .await
        })
    }

    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(DirectFileSystem::canonicalize(self, path, sandbox))
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(DirectFileSystem::read_file(self, path, sandbox))
    }

    fn read_file_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(DirectFileSystem::read_file_bounded(
            self, path, max_bytes, sandbox,
        ))
    }

    fn read_file_bounded_with_source<'a>(
        &'a self,
        path: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<(Vec<u8>, Option<PathBuf>)>> {
        Box::pin(async move {
            reject_sandbox_context(sandbox)?;
            let path = path.to_abs_path()?;
            run_cancellable_file_system_task(move |cancel| {
                let mut source = None;
                let bytes = read_bounded_file_sync(
                    || {
                        let (file, metadata) = regular_file::open_sync(path.as_path())?;
                        let opened = codex_file_system::resolved_file_path(&file)?;
                        if source.as_ref().is_some_and(|first| first != &opened) {
                            return Err(io::Error::other(
                                "opened source changed during acquisition",
                            ));
                        }
                        source = Some(opened);
                        Ok((file, metadata))
                    },
                    max_bytes,
                    &cancel,
                )?;
                Ok(bytes.map(|bytes| (bytes, source)))
            })
            .await
        })
    }

    fn read_file_bounded_confined<'a>(
        &'a self,
        path: &'a PathUri,
        root: &'a PathUri,
        max_bytes: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Option<Vec<u8>>> {
        Box::pin(DirectFileSystem::read_file_bounded_confined(
            self, path, root, max_bytes, sandbox,
        ))
    }

    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(DirectFileSystem::read_file_stream(self, path, sandbox))
    }

    fn read_file_metadata_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async move {
            let file = self.open_file_for_read(path, sandbox).await?;
            if file.metadata().await?.len() > MAX_READ_FILE_BYTES {
                return Err(file_too_large_error(MAX_READ_FILE_BYTES));
            }
            Ok(
                FileSystemReadStream::new(ReaderStream::with_capacity(file, FILE_READ_CHUNK_SIZE))
                    .with_limit(MAX_READ_FILE_BYTES),
            )
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(DirectFileSystem::write_file(self, path, contents, sandbox))
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(DirectFileSystem::create_directory(
            self, path, options, sandbox,
        ))
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(DirectFileSystem::get_metadata(self, path, sandbox))
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Box::pin(DirectFileSystem::read_directory(self, path, sandbox))
    }

    fn read_directory_bounded<'a>(
        &'a self,
        path: &'a PathUri,
        max_entries: usize,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome> {
        Box::pin(DirectFileSystem::read_directory_bounded(
            self,
            path,
            max_entries,
            sandbox,
        ))
    }

    fn read_directory_bounded_for_walk<'a>(
        &'a self,
        path: &'a PathUri,
        max_entries: usize,
        include_metadata: bool,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome<WalkDirectoryEntry>> {
        read_directory_for_walk(path, max_entries, include_metadata, sandbox, false)
    }

    fn read_directory_bounded_no_follow_for_walk<'a>(
        &'a self,
        path: &'a PathUri,
        max_entries: usize,
        include_metadata: bool,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome<WalkDirectoryEntry>> {
        read_directory_for_walk(path, max_entries, include_metadata, sandbox, true)
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(DirectFileSystem::remove(self, path, options, sandbox))
    }

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(DirectFileSystem::copy(
            self,
            source_path,
            destination_path,
            options,
            sandbox,
        ))
    }
}

/// No persistent handles or retained file bytes: each page reopens under its
/// caller's policy, compares the revision, and reads only the bounded range.
/// Default sharing never blocks other processes' writers or deleters, so a
/// page counts only when two reads agree and the handle revision is unchanged:
/// NTFS write/change times have clock-tick granularity, and a same-tick
/// in-place write leaves size and times unchanged. A later page's reopen
/// refuses a replacement.
fn read_file_range_sync(
    path: &Path,
    start: u64,
    len: usize,
    expected: Option<&str>,
    cancel: &CancellationToken,
) -> io::Result<codex_file_system::FileRangeRead> {
    check_file_system_cancelled(cancel)?;
    let (file, metadata) = regular_file::open_sync(path)?;
    let revision = file_range_revision(&file, &metadata)?;
    if expected.is_some_and(|expected| expected != revision) {
        return Err(io::Error::other(
            "file revision changed between byte-range pages; discard the old continuation and start a new read",
        ));
    }
    if start > metadata.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "byte range starts past end of file",
        ));
    }
    let read_len = (metadata.len() - start).min(len as u64) as usize;
    let mut bytes = vec![0u8; read_len];
    let mut check = vec![0u8; read_len];
    let read = read_full_at(&file, start, &mut bytes)?;
    // A concurrent truncation can end either read early; that is a change.
    let agreed =
        read == read_len && read_full_at(&file, start, &mut check)? == read_len && bytes == check;
    check_file_system_cancelled(cancel)?;
    if !agreed || revision != file_range_revision(&file, &file.metadata()?)? {
        return Err(io::Error::other(
            "file changed during byte-range acquisition",
        ));
    }
    Ok(codex_file_system::FileRangeRead {
        bytes,
        file_bytes: metadata.len(),
        revision,
    })
}

/// Fill `buffer` from `offset`, stopping early only at end of file.
fn read_full_at(file: &std::fs::File, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;

    let mut filled = 0;
    while filled < buffer.len() {
        let read = file.seek_read(&mut buffer[filled..], offset + filled as u64)?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

/// One open, one pass, no retained bytes. Each chunk is hashed only when two
/// reads agree (as for range pages); the handle revision is compared before
/// and after, then the path is reopened so a replacement or deletion during
/// the pass is refused. `None` reports a change.
fn checksum_file_sync(
    path: &Path,
    include_text_diagnostics: bool,
    cancel: &CancellationToken,
) -> io::Result<Option<codex_file_system::FileDigest>> {
    use sha2::Digest as _;
    use std::fmt::Write as _;

    check_file_system_cancelled(cancel)?;
    let (file, metadata) = regular_file::open_sync(path)?;
    let revision = file_range_revision(&file, &metadata)?;
    let mut hasher = sha2::Sha256::new();
    let mut diagnostics = include_text_diagnostics
        .then(codex_file_system::TextDiagnosticsScanner::default);
    let mut buffer = vec![0u8; FILE_READ_CHUNK_SIZE];
    let mut check = vec![0u8; FILE_READ_CHUNK_SIZE];
    let mut file_bytes = 0u64;
    loop {
        check_file_system_cancelled(cancel)?;
        let read = read_full_at(&file, file_bytes, &mut buffer)?;
        if read == 0 {
            break;
        }
        if read_full_at(&file, file_bytes, &mut check[..read])? != read
            || buffer[..read] != check[..read]
        {
            return Ok(None);
        }
        file_bytes += read as u64;
        if file_bytes > metadata.len() {
            return Ok(None);
        }
        hasher.update(&buffer[..read]);
        if let Some(diagnostics) = &mut diagnostics {
            diagnostics.push(&buffer[..read]);
        }
    }
    if file_bytes != metadata.len() || revision != file_range_revision(&file, &file.metadata()?)? {
        return Ok(None);
    }
    match regular_file::open_sync(path) {
        Ok((current, current_metadata)) => {
            if revision != file_range_revision(&current, &current_metadata)? {
                return Ok(None);
            }
        }
        Err(error) if is_changed_file_race_error(error.kind()) => return Ok(None),
        Err(error) => return Err(error),
    }
    let sha256 = hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    Ok(Some(codex_file_system::FileDigest {
        sha256,
        file_bytes,
        revision,
        text_diagnostics: diagnostics.map(codex_file_system::TextDiagnosticsScanner::finish),
    }))
}

fn file_range_revision(file: &std::fs::File, metadata: &std::fs::Metadata) -> io::Result<String> {
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
    };
    let identity = native_file_identity(file, metadata)?;
    let mut basic = std::mem::MaybeUninit::<FILE_BASIC_INFO>::uninit();
    // SAFETY: the live file handle and correctly sized output remain valid.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as _,
            FileBasicInfo,
            basic.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call initialized the output.
    let basic = unsafe { basic.assume_init() };
    Ok(format!(
        "v1:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}",
        identity.volume_serial_number,
        identity.file_index,
        metadata.len(),
        metadata.creation_time(),
        metadata.last_write_time(),
        basic.ChangeTime,
        basic.FileAttributes
    ))
}

struct NativeFileState {
    metadata: std::fs::Metadata,
    identity: NativeFileIdentity,
}

/// Keep the drop guard in the awaiting task: dropping a caller stops queued work
/// before it starts, and running work at its next cooperative checkpoint.
async fn run_cancellable_file_system_task<T: Send + 'static>(
    work: impl FnOnce(CancellationToken) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();
    tokio::task::spawn_blocking(move || {
        check_file_system_cancelled(&cancel)?;
        work(cancel)
    })
    .await
    .map_err(io::Error::other)?
}

fn check_file_system_cancelled(cancel: &CancellationToken) -> io::Result<()> {
    if cancel.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "filesystem operation cancelled",
        ));
    }
    Ok(())
}

fn read_bounded_file_sync(
    mut open: impl FnMut() -> io::Result<(std::fs::File, std::fs::Metadata)>,
    max_bytes: usize,
    cancel: &CancellationToken,
) -> io::Result<Option<Vec<u8>>> {
    check_file_system_cancelled(cancel)?;
    let (file, metadata) = open()?;
    let expected_len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    let capacity = if expected_len <= max_bytes {
        expected_len.min(FILE_READ_CHUNK_SIZE)
    } else {
        0
    };
    let mut bytes = Vec::with_capacity(capacity);
    let Some(first) = read_bounded_file_snapshot(file, metadata, max_bytes, cancel, |chunk| {
        bytes.extend_from_slice(chunk)
    })?
    else {
        return Ok(None);
    };
    // A replacement/disappearance after the first read is a changed snapshot.
    let verification: io::Result<bool> = (|| {
        check_file_system_cancelled(cancel)?;
        let (file, metadata) = open()?;
        let mut offset = 0usize;
        let mut matches = true;
        let Some(second) =
            read_bounded_file_snapshot(file, metadata, max_bytes, cancel, |chunk| {
                let end = offset + chunk.len();
                matches &= bytes.get(offset..end) == Some(chunk);
                offset = end;
                // Do not short-circuit: later I/O errors still take precedence.
            })?
        else {
            return Ok(false);
        };
        check_file_system_cancelled(cancel)?;
        let (file, metadata) = open()?;
        let final_state = NativeFileState {
            identity: native_file_identity(&file, &metadata)?,
            metadata,
        };
        Ok(matches
            && offset == bytes.len()
            && !native_file_metadata_changed(&first.metadata, &second.metadata)
            && first.identity == second.identity
            && !native_file_metadata_changed(&second.metadata, &final_state.metadata)
            && second.identity == final_state.identity)
    })();
    match verification {
        Ok(true) => Ok(Some(bytes)),
        Ok(false) => Ok(None),
        Err(err) if is_changed_file_race_error(err.kind()) => Ok(None),
        Err(err) => Err(err),
    }
}

fn read_bounded_file_snapshot(
    mut file: std::fs::File,
    metadata_before: std::fs::Metadata,
    max_bytes: usize,
    cancel: &CancellationToken,
    consume: impl FnMut(&[u8]),
) -> io::Result<Option<NativeFileState>> {
    let identity_before = native_file_identity(&file, &metadata_before)?;
    let expected_len = usize::try_from(metadata_before.len()).unwrap_or(usize::MAX);
    if expected_len > max_bytes {
        return Ok(None);
    }

    let Some(bytes_read) =
        read_bounded_chunks(&mut file, expected_len, max_bytes, cancel, consume)?
    else {
        return Ok(None);
    };

    let metadata_after = file.metadata()?;
    let identity_after = native_file_identity(&file, &metadata_after)?;
    if bytes_read != expected_len
        || native_file_metadata_changed(&metadata_before, &metadata_after)
        || identity_before != identity_after
    {
        return Ok(None);
    }
    Ok(Some(NativeFileState {
        metadata: metadata_before,
        identity: identity_before,
    }))
}

fn read_bounded_chunks(
    reader: &mut impl std::io::Read,
    expected_len: usize,
    max_bytes: usize,
    cancel: &CancellationToken,
    mut consume: impl FnMut(&[u8]),
) -> io::Result<Option<usize>> {
    // Small files should not allocate a full transfer block. Grow if the file
    // grows, but never retain a second file-sized buffer.
    let capacity = expected_len
        .saturating_add(1)
        .clamp(32, FILE_READ_CHUNK_SIZE)
        .min(max_bytes.saturating_add(1));
    let mut scratch = vec![0u8; capacity];
    let mut total = 0usize;
    loop {
        check_file_system_cancelled(cancel)?;
        let len = scratch
            .len()
            .min(max_bytes.saturating_sub(total).saturating_add(1));
        let read = match reader.read(&mut scratch[..len]) {
            Ok(0) => return Ok(Some(total)),
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if read > max_bytes.saturating_sub(total) {
            return Ok(None);
        }
        consume(&scratch[..read]);
        total += read;
        if read == scratch.len() && scratch.len() < FILE_READ_CHUNK_SIZE && total < max_bytes {
            scratch.resize((scratch.len() * 2).min(FILE_READ_CHUNK_SIZE), 0);
        }
    }
}

fn native_file_metadata_changed(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || before.created().ok() != after.created().ok()
        || before.is_file() != after.is_file()
        || before.is_dir() != after.is_dir()
        || before.is_symlink() != after.is_symlink()
        || platform_file_metadata_changed(before, after)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeFileIdentity {
    volume_serial_number: u32,
    file_index: u64,
}

fn native_file_identity(
    file: &std::fs::File,
    _metadata: &std::fs::Metadata,
) -> io::Result<NativeFileIdentity> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: `file` owns a valid handle and `information` points to writable,
    // correctly sized storage for the duration of the call.
    let succeeded = unsafe {
        GetFileInformationByHandle(file.as_raw_handle() as HANDLE, information.as_mut_ptr())
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful call initialized the complete structure.
    let information = unsafe { information.assume_init() };
    Ok(NativeFileIdentity {
        volume_serial_number: information.dwVolumeSerialNumber,
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
    })
}

fn platform_file_metadata_changed(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    before.file_attributes() != after.file_attributes()
        || before.creation_time() != after.creation_time()
        || before.last_write_time() != after.last_write_time()
}

fn is_changed_file_race_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidInput
    )
}

fn reject_sandbox_context(sandbox: Option<&FileSystemSandboxContext>) -> io::Result<()> {
    if sandbox.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "direct filesystem operations do not accept sandbox context",
        ));
    }
    Ok(())
}

fn reject_platform_sandbox_context(sandbox: Option<&FileSystemSandboxContext>) -> io::Result<()> {
    if sandbox.is_some_and(FileSystemSandboxContext::should_run_in_sandbox) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sandboxed filesystem operations require configured runtime paths",
        ));
    }
    Ok(())
}

fn file_metadata(metadata: &std::fs::Metadata, is_symlink: bool) -> FileMetadata {
    FileMetadata {
        is_directory: metadata.is_dir(),
        is_file: metadata.is_file(),
        is_symlink,
        size: metadata.len(),
        created_at_ms: metadata.created().ok().map_or(0, system_time_to_unix_ms),
        modified_at_ms: metadata.modified().ok().map_or(0, system_time_to_unix_ms),
    }
}

pub(crate) async fn read_file_payload_capped<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    metadata_len: u64,
    max_bytes: u64,
) -> io::Result<Vec<u8>> {
    if metadata_len > max_bytes {
        return Err(file_too_large_error(max_bytes));
    }
    let mut bytes = Vec::with_capacity(metadata_len as usize);
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > max_bytes {
        return Err(file_too_large_error(max_bytes));
    }
    Ok(bytes)
}

fn remove_sync(
    path: &Path,
    options: RemoveOptions,
    before_remove: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    use std::os::windows::fs::FileTypeExt;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if options.force && error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    before_remove()?;
    let file_type = metadata.file_type();
    let result = if file_type.is_symlink_dir() {
        std::fs::remove_dir(path)
    } else if file_type.is_dir() {
        if options.recursive {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_dir(path)
        }
    } else {
        std::fs::remove_file(path)
    };
    finish_remove(path, options.force, result)
}

fn finish_remove(path: &Path, force: bool, result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if force && error.kind() == io::ErrorKind::NotFound => {
            // Recursive deletion can report a missing child while the root
            // remains. Check only this failure path, without retrying deletion
            // against a potentially recreated object.
            match std::fs::symlink_metadata(path) {
                Err(absence) if absence.kind() == io::ErrorKind::NotFound => Ok(()),
                _ => Err(error),
            }
        }
        result => result,
    }
}

fn directory_entry(
    name: std::ffi::OsString,
    metadata: Option<&std::fs::Metadata>,
    _is_symlink: Option<bool>,
    _error: Option<&io::Error>,
) -> io::Result<Option<ReadDirectoryEntry>> {
    // A display approximation must never become another entry's identity.
    let file_name = name.into_string().map_err(|name| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("directory entry has an unrepresentable native name: {name:?}"),
        )
    })?;
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    Ok(Some(ReadDirectoryEntry {
        file_name,
        is_directory: metadata.is_dir(),
        is_file: metadata.is_file(),
    }))
}

fn read_directory_for_walk<'a>(
    path: &'a PathUri,
    max_entries: usize,
    include_metadata: bool,
    sandbox: Option<&'a FileSystemSandboxContext>,
    no_follow: bool,
) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome<WalkDirectoryEntry>> {
    Box::pin(async move {
        reject_sandbox_context(sandbox)?;
        if max_entries == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bounded directory read limit must be greater than zero",
            ));
        }
        // An opaque parent can contain Unicode children too. Their labels
        // cannot always be joined, so retain native child locators. The
        // reserved prefix check adds no parse/allocation for ordinary roots.
        let native_children = path.encoded_path().starts_with("/%00/bad/path/");
        let path = path.to_abs_path()?;
        run_cancellable_file_system_task(move |cancel| {
            let _directories = if no_follow {
                codex_file_system::lock_directory_chain(path.as_path())
                    .map_err(|error| refused_walk_directory_error(path.as_path(), error))?
            } else {
                Vec::new()
            };
            read_directory_sync(
                path.as_path(),
                Some(max_entries),
                &cancel,
                !no_follow,
                |name, metadata, is_symlink, error| {
                    use std::os::windows::fs::FileTypeExt;
                    // A followed link whose target is absent is neither a file
                    // nor a directory, so the walker skips it as it always has;
                    // other failures keep their exact path.
                    let absent_target = !no_follow
                        && is_symlink == Some(true)
                        && error.is_some_and(|error| error.kind() == io::ErrorKind::NotFound);
                    let snapshot = Some(match metadata {
                        Some(metadata) => {
                            let is_symlink = is_symlink.unwrap_or_else(|| metadata.is_symlink());
                            WalkEntryMetadata {
                                is_directory: metadata.is_dir()
                                    || metadata.file_type().is_symlink_dir(),
                                is_file: metadata.is_file()
                                    || metadata.file_type().is_symlink_file(),
                                is_symlink,
                                details: include_metadata.then(|| {
                                    Box::new(codex_file_system::WalkFileMetadata {
                                        size: metadata.len(),
                                        created_at_ms: metadata
                                            .created()
                                            .ok()
                                            .map(system_time_to_unix_ms),
                                        modified_at_ms: metadata
                                            .modified()
                                            .ok()
                                            .map(system_time_to_unix_ms),
                                        readonly: metadata.permissions().readonly(),
                                        is_symlink,
                                    })
                                }),
                            }
                        }
                        None if absent_target => WalkEntryMetadata {
                            is_directory: false,
                            is_file: false,
                            is_symlink: true,
                            details: None,
                        },
                        None => {
                            return Ok(Some(WalkDirectoryEntry::Error {
                                path: PathUri::from_host_native_path(path.as_path().join(&name))?,
                                message: error.map_or_else(
                                    || "entry metadata unavailable".into(),
                                    ToString::to_string,
                                ),
                            }));
                        }
                    });
                    let file_name = name.to_str();
                    let Some(unicode_name) = file_name.filter(|_| !native_children) else {
                        let entry = codex_file_system::NativeWalkDirectoryEntry {
                            path: PathUri::from_host_native_path(path.as_path().join(&name))?,
                            file_name: name.to_string_lossy().into_owned(),
                            name_is_lossless: file_name.is_some(),
                            metadata: snapshot,
                        };
                        return Ok(Some(WalkDirectoryEntry::NativeEntry(Box::new(entry))));
                    };
                    Ok(Some(WalkDirectoryEntry::Entry {
                        file_name: unicode_name.to_owned(),
                        metadata: snapshot,
                    }))
                },
            )
        })
        .await
    })
}

/// The no-follow guard refuses links and non-directories as PermissionDenied,
/// which reads like a sandbox denial. Classify a refused directory itself with
/// a query that does not follow it; ancestor and access failures are unchanged.
fn refused_walk_directory_error(path: &Path, error: io::Error) -> io::Error {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    if error.kind() != io::ErrorKind::PermissionDenied {
        return error;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is a symlink or reparse point; links are not followed or listed",
            )
        }
        Ok(metadata) if !metadata.is_dir() => {
            io::Error::new(io::ErrorKind::NotADirectory, "path is not a directory")
        }
        _ => error,
    }
}

fn read_directory_sync<T>(
    path: &Path,
    max_entries: Option<usize>,
    cancel: &CancellationToken,
    follow_symlinks: bool,
    mut classify: impl FnMut(
        std::ffi::OsString,
        Option<&std::fs::Metadata>,
        Option<bool>,
        Option<&io::Error>,
    ) -> io::Result<Option<T>>,
) -> io::Result<ReadDirectoryOutcome<T>> {
    check_file_system_cancelled(cancel)?;
    let mut entries = Vec::new();
    let mut entries_examined = 0;
    let mut read_dir = std::fs::read_dir(path)?;
    while max_entries.is_none_or(|limit| entries_examined < limit) {
        check_file_system_cancelled(cancel)?;
        let Some(entry) = read_dir.next() else {
            return Ok(ReadDirectoryOutcome {
                entries,
                entries_examined,
                limit_reached: false,
            });
        };
        let entry = entry?;
        entries_examined += 1;
        // Windows DirEntry metadata describes the link itself. Only legacy or
        // explicitly following consumers may query a link's target.
        let is_symlink = entry
            .file_type()
            .ok()
            .map(|file_type| file_type.is_symlink());
        let metadata = match (follow_symlinks, is_symlink) {
            (true, Some(true) | None) => std::fs::metadata(entry.path()),
            _ => entry.metadata(),
        };
        if let Some(entry) = classify(
            entry.file_name(),
            metadata.as_ref().ok(),
            is_symlink,
            metadata.as_ref().err(),
        )? {
            entries.push(entry);
        }
    }
    Ok(ReadDirectoryOutcome {
        entries,
        entries_examined,
        limit_reached: true,
    })
}

fn copy_dir_recursive(
    source: &Path,
    target: &Path,
    source_root: &Path,
    cancel: &CancellationToken,
) -> io::Result<()> {
    // An invalid destination is a request error that precedes every
    // destination effect, so it carries no partial-copy recovery context.
    reject_destination_in_source(target, source_root)?;
    let mut progress = codex_file_system::CopyFailure {
        completed_entries: 0,
        target_may_have_changed: false,
        destination: target.display().to_string(),
        cause: String::new(),
    };
    copy_dir_recursive_with_progress(source, target, source_root, cancel, &mut progress).map_err(
        |error| {
            progress.cause = error.to_string();
            io::Error::new(error.kind(), progress)
        },
    )
}

fn copy_dir_recursive_with_progress(
    source: &Path,
    target: &Path,
    source_root: &Path,
    cancel: &CancellationToken,
    progress: &mut codex_file_system::CopyFailure,
) -> io::Result<()> {
    progress.target_may_have_changed = false;
    check_file_system_cancelled(cancel)?;
    reject_destination_in_source(target, source_root)?;
    check_file_system_cancelled(cancel)?;
    let existed = target.try_exists()?;
    progress.target_may_have_changed = !existed;
    std::fs::create_dir_all(target)?;
    progress.completed_entries += u64::from(!existed);
    progress.target_may_have_changed = false;
    for entry in std::fs::read_dir(source)? {
        check_file_system_cancelled(cancel)?;
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            copy_dir_recursive_with_progress(
                &source_path,
                &target_path,
                source_root,
                cancel,
                progress,
            )?;
        } else if file_type.is_file() {
            reject_destination_in_source(&target_path, source_root)?;
            check_file_system_cancelled(cancel)?;
            progress.target_may_have_changed = true;
            std::fs::copy(&source_path, &target_path)?;
            progress.completed_entries += 1;
            progress.target_may_have_changed = false;
        } else if file_type.is_symlink() {
            check_file_system_cancelled(cancel)?;
            progress.target_may_have_changed = true;
            copy_symlink(&source_path, &target_path)?;
            progress.completed_entries += 1;
            progress.target_may_have_changed = false;
        }
    }
    check_file_system_cancelled(cancel)
}

fn reject_destination_in_source(destination: &Path, source_root: &Path) -> io::Result<()> {
    let destination = resolve_existing_path(destination)?;
    if destination.starts_with(source_root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fs/copy cannot copy a directory to itself or one of its descendants",
        ));
    }
    Ok(())
}

pub(crate) fn resolve_existing_path(path: &Path) -> io::Result<PathBuf> {
    let mut unresolved_suffix = Vec::new();
    let mut existing_path = path;
    while !existing_path.exists() {
        let Some(file_name) = existing_path.file_name() else {
            break;
        };
        unresolved_suffix.push(file_name.to_os_string());
        let Some(parent) = existing_path.parent() else {
            break;
        };
        existing_path = parent;
    }

    let mut resolved = std::fs::canonicalize(existing_path)?;
    for file_name in unresolved_suffix.iter().rev() {
        resolved.push(file_name);
    }
    Ok(resolved)
}

pub(crate) fn current_sandbox_cwd() -> io::Result<PathBuf> {
    let cwd = std::env::current_dir()
        .map_err(|err| io::Error::other(format!("failed to read current dir: {err}")))?;
    resolve_existing_path(cwd.as_path())
}

fn copy_symlink(source: &Path, target: &Path) -> io::Result<()> {
    let link_target = std::fs::read_link(source)?;

    {
        if symlink_points_to_directory(source)? {
            std::os::windows::fs::symlink_dir(&link_target, target)
        } else {
            std::os::windows::fs::symlink_file(&link_target, target)
        }
    }
}

fn symlink_points_to_directory(source: &Path) -> io::Result<bool> {
    use std::os::windows::fs::FileTypeExt;

    Ok(std::fs::symlink_metadata(source)?
        .file_type()
        .is_symlink_dir())
}

fn system_time_to_unix_ms(time: SystemTime) -> i64 {
    let millis = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i128,
        Err(error) => -(error.duration().as_millis() as i128),
    };
    millis.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}
