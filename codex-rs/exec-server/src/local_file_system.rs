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

fn file_too_large_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("file is too large to read: limit is {MAX_READ_FILE_BYTES} bytes"),
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
        let path = path.to_abs_path()?;
        let (file, metadata) = regular_file::open_with_metadata(path.as_path()).await?;
        if metadata.len() > MAX_READ_FILE_BYTES {
            return Err(file_too_large_error());
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_READ_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() as u64 > MAX_READ_FILE_BYTES {
            return Err(file_too_large_error());
        }
        Ok(bytes)
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
            let symlink_metadata = std::fs::symlink_metadata(path.as_path())?;
            if symlink_metadata.file_type().is_symlink() {
                Ok::<_, io::Error>((std::fs::metadata(path.as_path())?, true))
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
            read_directory_sync(path.as_path(), None, &cancel, directory_entry)
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
            read_directory_sync(path.as_path(), Some(max_entries), &cancel, directory_entry)
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
            match std::fs::symlink_metadata(path.as_path()) {
                Ok(metadata) => {
                    check_file_system_cancelled(&cancel)?;
                    let file_type = metadata.file_type();
                    use std::os::windows::fs::FileTypeExt;

                    if file_type.is_symlink_dir() {
                        std::fs::remove_dir(path.as_path())?;
                    } else if file_type.is_dir() {
                        if options.recursive {
                            std::fs::remove_dir_all(path.as_path())?;
                        } else {
                            std::fs::remove_dir(path.as_path())?;
                        }
                    } else {
                        std::fs::remove_file(path.as_path())?;
                    }
                    Ok(())
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound && options.force => Ok(()),
                Err(err) => Err(err),
            }
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
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ReadDirectoryOutcome<WalkDirectoryEntry>> {
        Box::pin(async move {
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
                    |name, metadata, is_symlink| {
                        // Lossy names must still be probed through the URI the
                        // walker will report, not classified as a different file.
                        let snapshot = name.to_str().and_then(|_| {
                            is_symlink.map(|is_symlink| WalkEntryMetadata {
                                is_directory: metadata.is_dir(),
                                is_file: metadata.is_file(),
                                is_symlink,
                            })
                        });
                        WalkDirectoryEntry {
                            file_name: name.to_string_lossy().into_owned(),
                            metadata: snapshot,
                        }
                    },
                )
            })
            .await
        })
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

fn directory_entry(
    name: std::ffi::OsString,
    metadata: &std::fs::Metadata,
    _is_symlink: Option<bool>,
) -> ReadDirectoryEntry {
    ReadDirectoryEntry {
        file_name: name.to_string_lossy().into_owned(),
        is_directory: metadata.is_dir(),
        is_file: metadata.is_file(),
    }
}

fn read_directory_sync<T>(
    path: &Path,
    max_entries: Option<usize>,
    cancel: &CancellationToken,
    mut classify: impl FnMut(std::ffi::OsString, &std::fs::Metadata, Option<bool>) -> T,
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
        // Enumeration already describes an entry that is not a link. Links
        // follow their target; entries whose target cannot be read (dangling
        // links) are skipped but still counted.
        let is_symlink = entry
            .file_type()
            .ok()
            .map(|file_type| file_type.is_symlink());
        let metadata = match is_symlink {
            Some(false) => entry.metadata(),
            _ => std::fs::metadata(entry.path()),
        };
        let Ok(metadata) = metadata else {
            continue;
        };
        entries.push(classify(entry.file_name(), &metadata, is_symlink));
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
    check_file_system_cancelled(cancel)?;
    reject_destination_in_source(target, source_root)?;
    check_file_system_cancelled(cancel)?;
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        check_file_system_cancelled(cancel)?;
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &target_path, source_root, cancel)?;
        } else if file_type.is_file() {
            reject_destination_in_source(&target_path, source_root)?;
            check_file_system_cancelled(cancel)?;
            std::fs::copy(&source_path, &target_path)?;
        } else if file_type.is_symlink() {
            check_file_system_cancelled(cancel)?;
            copy_symlink(&source_path, &target_path)?;
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

#[cfg(test)]
#[path = "local_file_system_path_uri_tests.rs"]
mod path_uri_tests;

#[cfg(test)]
#[path = "local_file_system_latency_tests.rs"]
mod latency_tests;

#[cfg(test)]
#[path = "local_copy_cancellation_tests.rs"]
mod copy_cancellation_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn metadata_preserves_file_times_before_unix_epoch() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("historical-file");
        let file = std::fs::File::create(&path)?;
        let modified = UNIX_EPOCH - std::time::Duration::from_millis(1_234);
        file.set_times(std::fs::FileTimes::new().set_modified(modified))?;
        assert_eq!(file.metadata()?.modified()?, modified);
        let uri = PathUri::from(AbsolutePathBuf::from_absolute_path(path)?);

        let metadata = LocalFileSystem::unsandboxed()
            .get_metadata(&uri, None)
            .await?;

        assert!(metadata.is_file);
        assert_eq!(metadata.modified_at_ms, -1_234);
        Ok(())
    }

    #[test]
    fn symlink_points_to_directory_handles_dangling_directory_symlinks() -> io::Result<()> {
        use std::os::windows::fs::symlink_dir;

        let temp_dir = tempfile::TempDir::new()?;
        let source_dir = temp_dir.path().join("source");
        let link_path = temp_dir.path().join("source-link");
        std::fs::create_dir(&source_dir)?;

        symlink_dir(&source_dir, &link_path)?;

        std::fs::remove_dir(&source_dir)?;

        assert_eq!(symlink_points_to_directory(&link_path)?, true);
        Ok(())
    }
    #[test_case::test_case(false; "non_recursive")]
    #[test_case::test_case(true; "recursive")]
    #[tokio::test]
    async fn remove_directory_link_preserves_target(recursive: bool) -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("target");
        let link = temp.path().join("link");
        std::fs::create_dir(&target)?;
        std::fs::write(target.join("keep.txt"), b"keep")?;
        std::os::windows::fs::symlink_dir(&target, &link)?;
        DirectFileSystem
            .remove(
                &PathUri::from_host_native_path(&link)?,
                RemoveOptions {
                    recursive,
                    force: false,
                },
                None,
            )
            .await?;
        assert_eq!(
            std::fs::symlink_metadata(&link).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(std::fs::read(target.join("keep.txt"))?, b"keep");
        Ok(())
    }

    #[test_case::test_case(false; "directory_alias")]
    #[test_case::test_case(true; "file_alias")]
    #[tokio::test]
    async fn recursive_copy_rejects_destination_alias_into_source(
        file_alias: bool,
    ) -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        std::fs::create_dir_all(source.join("sub"))?;
        std::fs::create_dir(&destination)?;
        std::fs::write(source.join("a.txt"), b"original")?;
        std::fs::write(source.join("sub/a.txt"), b"replacement")?;
        if file_alias {
            std::fs::create_dir(destination.join("sub"))?;
            std::os::windows::fs::symlink_file(
                source.join("a.txt"),
                destination.join("sub/a.txt"),
            )?;
        } else {
            std::os::windows::fs::symlink_dir(&source, destination.join("sub"))?;
        }
        let error = DirectFileSystem
            .copy(
                &PathUri::from_host_native_path(&source)?,
                &PathUri::from_host_native_path(&destination)?,
                CopyOptions { recursive: true },
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read(source.join("a.txt"))?, b"original");
        assert_eq!(std::fs::read(source.join("sub/a.txt"))?, b"replacement");
        Ok(())
    }

    #[tokio::test]
    async fn directory_reads_follow_links_and_count_skipped_entries() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let directory = temp.path().join("directory");
        std::fs::create_dir(&directory)?;
        std::fs::write(temp.path().join("target"), b"target")?;
        std::os::windows::fs::symlink_file(
            temp.path().join("target"),
            directory.join("file-link"),
        )?;
        std::os::windows::fs::symlink_file(
            temp.path().join("missing"),
            directory.join("dangling"),
        )?;
        std::fs::create_dir(directory.join("child"))?;
        let uri = PathUri::from_host_native_path(&directory)?;
        let mut all = DirectFileSystem.read_directory(&uri, None).await?;
        all.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        assert_eq!(
            all,
            vec![
                ReadDirectoryEntry {
                    file_name: "child".into(),
                    is_directory: true,
                    is_file: false
                },
                ReadDirectoryEntry {
                    file_name: "file-link".into(),
                    is_directory: false,
                    is_file: true
                },
            ]
        );
        let mut bounded = DirectFileSystem
            .read_directory_bounded(&uri, 3, None)
            .await?;
        bounded
            .entries
            .sort_by(|a, b| a.file_name.cmp(&b.file_name));
        assert_eq!(bounded.entries, all);
        assert_eq!(bounded.entries_examined, 3);
        assert!(bounded.limit_reached);
        let exhausted = DirectFileSystem
            .read_directory_bounded(&uri, 4, None)
            .await?;
        assert_eq!(exhausted.entries_examined, 3);
        assert!(!exhausted.limit_reached);
        Ok(())
    }
}
