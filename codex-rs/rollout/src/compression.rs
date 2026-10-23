use std::ffi::OsStr;
use std::fs::File;
use std::fs::FileTimes;
use std::fs::Permissions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::io::AsyncBufReadExt;

const COMPRESSED_SUFFIX: &str = ".zst";
const COMPRESSED_READER_CHANNEL_CAPACITY: usize = 64;
const MAX_OPEN_RETRIES: usize = 3;
const OPEN_ROLLOUT_LINE_READER_RETRY_DELAY: Duration = Duration::from_millis(50);
const TEMP_SUFFIX: &str = ".tmp";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A shared maintenance lifetime. Last-owner release stops future passes;
/// an already accepted file transaction is joined, never aborted.
pub struct RolloutCompressionWorker {
    stop: tokio::sync::watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
    runtime: tokio::runtime::Handle,
}

impl Drop for RolloutCompressionWorker {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            self.runtime.spawn(async move {
                if let Err(error) = task.await {
                    tracing::warn!(%error, "rollout compression owner failed");
                }
            });
        }
    }
}

/// Register one maintenance worker per live normalized home, without retaining
/// abandoned homes in a strong process-global registry.
pub fn spawn_rollout_compression_worker(
    codex_home: PathBuf,
) -> Option<std::sync::Arc<RolloutCompressionWorker>> {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::sync::Weak;
    static WORKERS: OnceLock<
        Mutex<std::collections::HashMap<PathBuf, Weak<RolloutCompressionWorker>>>,
    > = OnceLock::new();
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let home = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&codex_home)
        .map(|path| path.to_path_buf())
        .unwrap_or(codex_home);
    let mut workers = WORKERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    workers.retain(|_, worker| worker.strong_count() > 0);
    if let Some(worker) = workers.get(&home).and_then(Weak::upgrade) {
        return Some(worker);
    }
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let task = worker::spawn(&runtime, home.clone(), stopped);
    let worker = Arc::new(RolloutCompressionWorker {
        stop,
        task: Some(task),
        runtime,
    });
    workers.insert(home, Arc::downgrade(&worker));
    Some(worker)
}

/// Returns the modified time for the existing plain or compressed rollout file.
pub(crate) async fn file_modified_time(path: &Path) -> io::Result<Option<time::OffsetDateTime>> {
    let Some(path) = path::existing_rollout_path(path).await else {
        return Ok(None);
    };
    let meta = tokio::fs::metadata(path).await?;
    let modified = meta.modified().ok();
    Ok(modified.map(time::OffsetDateTime::from))
}

/// Opens a rollout line reader that transparently handles plain `.jsonl` and `.jsonl.zst` files.
///
/// Briefly retries missing files and Windows sharing violations during representation
/// transitions so callers do not need to know which representation is on disk.
pub async fn open_rollout_line_reader(path: &Path) -> io::Result<RolloutLineReader> {
    for _ in 0..MAX_OPEN_RETRIES {
        match reader::open_once(path).await {
            Ok(reader) => return Ok(reader),
            Err(err) if is_retryable_open_error(&err) => {
                tokio::time::sleep(OPEN_ROLLOUT_LINE_READER_RETRY_DELAY).await;
            }
            Err(err) => return Err(err),
        }
    }
    reader::open_once(path).await
}

fn is_retryable_open_error(error: &io::Error) -> bool {
    // ERROR_SHARING_VIOLATION can occur while another handle replaces a rollout.
    // Do not retry ordinary access-denied errors such as a persistent ACL failure.
    error.kind() == io::ErrorKind::NotFound || (cfg!(windows) && error.raw_os_error() == Some(32))
}

/// Returns the compressed `.jsonl.zst` path for a rollout path.
#[cfg(test)]
pub(crate) fn compressed_rollout_path(path: &Path) -> PathBuf {
    path::compressed_rollout_path(path)
}

/// Shared per-rollout lock held while an append handle remains open.
///
/// Compression takes the corresponding exclusive lock before inspecting or
/// replacing either physical representation.
pub(crate) struct RolloutAppendLock {
    _file: File,
}

/// Owns archive/unarchive decisions across both physical collections. Keep this
/// guard through lookups and movement (or an artifact reclamation decision).
pub async fn lock_rollout_moves(codex_home: &Path) -> io::Result<File> {
    try_lock_rollout_moves(codex_home).await?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "rollout movement is busy; retry after the current operation",
        )
    })
}

/// Reclamation is optional: skip it when another owner can change locations.
pub async fn try_lock_rollout_moves(codex_home: &Path) -> io::Result<Option<File>> {
    let home = codex_home.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = open_lock_file(&home.join("rollout-moves.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(error) => Err(error.into()),
        }
    })
    .await
    .map_err(io::Error::other)?
}

/// Moves the current physical rollout representation while excluding compression.
/// Both locations are exclusively owned; callers must first close live writers.
pub async fn move_rollout_to_directory(path: &Path, directory: &Path) -> io::Result<PathBuf> {
    move_rollout_to_directory_with_owner(path, directory, None).await
}

pub async fn move_rollout_to_directory_with_owner(
    path: &Path,
    directory: &Path,
    owner: Option<File>,
) -> io::Result<PathBuf> {
    let path = path.to_path_buf();
    let directory = directory.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let home = path
            .ancestors()
            .find(|ancestor| {
                ancestor.file_name().is_some_and(|name| {
                    name == crate::SESSIONS_SUBDIR || name == crate::ARCHIVED_SESSIONS_SUBDIR
                })
            })
            .and_then(Path::parent)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "rollout move source is outside session storage",
                )
            })?;
        // Owned by the blocking mover even when its async caller is cancelled.
        let _move_owner = match owner {
            Some(owner) => owner,
            None => {
                let owner = open_lock_file(&home.join("rollout-moves.lock"))?;
                owner.try_lock().map_err(io::Error::from)?;
                owner
            }
        };
        let plain_path = plain_rollout_path(&path);
        let parent = plain_path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "rollout path has no parent")
        })?;
        let plain_path =
            std::fs::canonicalize(parent)?.join(plain_path.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "rollout path has no file name")
            })?);
        std::fs::create_dir_all(&directory)?;
        let lock_directory = std::fs::canonicalize(&directory)?;
        let plain_destination = lock_directory.join(plain_path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "rollout path has no file name")
        })?);
        if plain_path == plain_destination {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rollout move has identical endpoints",
            ));
        }
        // Stable ordering prevents inverse relocations from deadlocking.
        let mut lock_paths = [&plain_path, &plain_destination];
        lock_paths.sort();
        let first_lock = open_rollout_lock_file(lock_paths[0])?;
        first_lock.try_lock().map_err(io::Error::from)?;
        let second_lock = open_rollout_lock_file(lock_paths[1])?;
        second_lock.try_lock().map_err(io::Error::from)?;
        let compressed_source = path::compressed_rollout_path(&plain_path);
        let current_source = if plain_path.try_exists()? {
            &plain_path
        } else {
            &compressed_source
        };
        let mut duplicate = None;
        for destination in [
            &plain_destination,
            &path::compressed_rollout_path(&plain_destination),
        ] {
            if destination.try_exists()? {
                if !rollouts_have_identical_bytes(current_source, destination)? {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("conflicting rollout destination: {}", destination.display()),
                    ));
                }
                duplicate = Some(destination.clone());
            }
        }
        // The path resolved by discovery may have been compressed while we waited.
        let compressed_path = path::compressed_rollout_path(&plain_path);
        let source = if plain_path.is_file() {
            // A failed materialization cleanup can leave a stale compressed sibling.
            // Remove it before moving the authoritative plain representation.
            match std::fs::remove_file(&compressed_path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            plain_path
        } else {
            compressed_path
        };
        if let Some(destination) = duplicate {
            std::fs::remove_file(source)?;
            return Ok(destination);
        }
        let file_name = source.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "rollout path has no file name")
        })?;
        std::fs::create_dir_all(&directory)?;
        let destination = directory.join(file_name);
        // Publish without clobbering a non-cooperating destination creation.
        // Archive and active collections live on the same filesystem.
        move_rollout_no_replace(&source, &destination)?;
        Ok(destination)
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(windows)]
fn move_rollout_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // No REPLACE_EXISTING or COPY_ALLOWED: an existing target or a sharing
    // violation leaves both directory entries unchanged. A hard-link/unlink
    // move would publish a duplicate before discovering a source delete denial.
    let moved = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            0,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn move_rollout_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::hard_link(source, destination)?;
    std::fs::remove_file(source)
}

fn rollouts_have_identical_bytes(first: &Path, second: &Path) -> io::Result<bool> {
    use std::io::BufRead;
    fn reader(path: &Path) -> io::Result<Box<dyn BufRead>> {
        if !std::fs::symlink_metadata(path)?.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rollout duplicate is not an ordinary file; preserving both copies",
            ));
        }
        let file = File::open(path)?;
        if path.extension().is_some_and(|extension| extension == "zst") {
            Ok(Box::new(std::io::BufReader::new(
                zstd::stream::read::Decoder::new(file)?,
            )))
        } else {
            Ok(Box::new(std::io::BufReader::new(file)))
        }
    }
    let mut first = reader(first)?;
    let mut second = reader(second)?;
    loop {
        let a = first.fill_buf()?;
        let b = second.fill_buf()?;
        if a.is_empty() || b.is_empty() {
            return Ok(a.is_empty() && b.is_empty());
        }
        let count = a.len().min(b.len());
        if a[..count] != b[..count] {
            return Ok(false);
        }
        first.consume(count);
        second.consume(count);
    }
}

/// Identity, not equal content, makes an interrupted hard-link restore safe.
pub fn rollout_files_same_identity(first: &Path, second: &Path) -> io::Result<bool> {
    let first = open_regular_identity_file(first)?;
    let second = open_regular_identity_file(second)?;
    rollout_handles_same_identity(&first, &second)
}

fn open_regular_identity_file(path: &Path) -> io::Result<File> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other(
            "rollout identity requires an ordinary file",
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path)?;
    validate_regular_identity_file(&file)?;
    // Also reject a substituted symlink on platforms without a no-follow std open.
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other(
            "rollout identity path changed to a non-file",
        ));
    }
    Ok(file)
}

fn validate_regular_identity_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::other(
                "rollout identity cannot follow a reparse point",
            ));
        }
    }
    if !metadata.is_file() {
        return Err(io::Error::other(
            "rollout identity requires an ordinary file",
        ));
    }
    Ok(())
}

fn rollout_handles_same_identity(first: &File, second: &File) -> io::Result<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
        use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
        let identity = |file: &File| -> io::Result<(u32, u32, u32)> {
            let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
            // SAFETY: the live file owns the handle; the API initializes info on success.
            if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, info.as_mut_ptr()) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
            let info = unsafe { info.assume_init() };
            Ok((
                info.dwVolumeSerialNumber,
                info.nFileIndexHigh,
                info.nFileIndexLow,
            ))
        };
        Ok(identity(first)? == identity(second)?)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let first = first.metadata()?;
        let second = second.metadata()?;
        Ok((first.dev(), first.ino()) == (second.dev(), second.ino()))
    }
    #[cfg(not(any(windows, unix)))]
    {
        Err(io::Error::other("filesystem identity is unavailable"))
    }
}

/// Settle only the object this compressor published, never a substituted name.
fn remove_owned_rollout_file(path: &Path, published: &File) -> io::Result<()> {
    #[cfg(windows)]
    let current = {
        use std::os::windows::fs::OpenOptionsExt;
        // Exclude rename/delete while checking and deleting this exact handle.
        std::fs::OpenOptions::new()
            .access_mode(0x0001_0080)
            .share_mode(0x0000_0003)
            .custom_flags(0x0020_0000)
            .open(path)
    };
    #[cfg(not(windows))]
    let current = open_regular_identity_file(path);
    let current = match current {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    validate_regular_identity_file(&current)?;
    if !rollout_handles_same_identity(&current, published)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "compressed rollout was replaced; replacement retained",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO;
        use windows_sys::Win32::Storage::FileSystem::FileDispositionInfo;
        use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: current owns the delete-capable handle and disposition has
        // exactly the layout and size required by FileDispositionInfo.
        if unsafe {
            SetFileInformationByHandle(
                current.as_raw_handle() as _,
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of_val(&disposition) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::remove_file(path)
    }
}

/// Exclusive per-rollout lock held only while an append transaction can mutate the plain file.
pub(crate) struct RolloutWriteLock {
    _file: File,
}

/// Acquires the append lock and materializes the canonical plain rollout while
/// compression is excluded. The returned guard must live as long as the append
/// handle that is opened for the returned path.
pub(crate) fn lock_rollout_for_append_blocking(
    path: &Path,
) -> io::Result<(PathBuf, RolloutAppendLock)> {
    let plain_path = plain_rollout_path(path);
    let append_lock = lock_rollout_before_append_blocking(&plain_path)?;
    let plain_path = materialize_rollout_for_append_locked(plain_path.as_path())?;
    Ok((plain_path, append_lock))
}

/// Reserve the representation without materializing or repairing source bytes.
pub(crate) fn lock_rollout_before_append_blocking(path: &Path) -> io::Result<RolloutAppendLock> {
    let lock_file = open_rollout_lock_file(plain_rollout_path(path).as_path())?;
    lock_file.lock_shared()?;
    Ok(RolloutAppendLock { _file: lock_file })
}

pub(crate) fn lock_rollout_for_write_blocking(path: &Path) -> io::Result<RolloutWriteLock> {
    let lock_file = open_rollout_write_lock_file(plain_rollout_path(path).as_path())?;
    lock_file.lock()?;
    Ok(RolloutWriteLock { _file: lock_file })
}

#[cfg(test)]
pub(crate) fn try_lock_rollout_for_write_blocking(
    path: &Path,
) -> io::Result<Option<RolloutWriteLock>> {
    let lock_file = open_rollout_write_lock_file(plain_rollout_path(path).as_path())?;
    match lock_file.try_lock() {
        Ok(()) => Ok(Some(RolloutWriteLock { _file: lock_file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn try_lock_rollout_for_compression(path: &Path) -> io::Result<Option<File>> {
    let lock_file = open_rollout_lock_file(plain_rollout_path(path).as_path())?;
    match lock_file.try_lock() {
        Ok(()) => Ok(Some(lock_file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Exclude append handles and compression before discovering representations.
/// Maintenance is nonblocking to avoid waiting on a live writer it must stop first.
pub fn try_lock_rollout_for_maintenance(path: &Path) -> io::Result<File> {
    try_lock_rollout_for_compression(path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "rollout has a live writer or compressor",
        )
    })
}

fn open_rollout_lock_file(path: &Path) -> io::Result<File> {
    open_lock_file(rollout_lock_path(path).as_path())
}

fn open_rollout_write_lock_file(path: &Path) -> io::Result<File> {
    open_lock_file(rollout_write_lock_path(path).as_path())
}

fn open_lock_file(lock_path: &Path) -> io::Result<File> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);

    #[cfg(windows)]
    for _ in 0..3 {
        match options.open(lock_path) {
            // An orphan-lock cleanup briefly holds a no-sharing delete handle.
            Err(error) if matches!(error.raw_os_error(), Some(5 | 32 | 303)) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
    options.open(lock_path)
}

#[cfg(windows)]
fn cleanup_orphan_rollout_lock(lock_path: &Path) -> io::Result<bool> {
    use std::os::windows::fs::OpenOptionsExt;
    let Some(name) = lock_path.file_name().and_then(OsStr::to_str) else {
        return Ok(false);
    };
    let Some(plain_name) = name
        .strip_prefix('.')
        .and_then(|name| {
            name.strip_suffix(".write.lock")
                .or_else(|| name.strip_suffix(".lock"))
        })
        .filter(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
    else {
        return Ok(false);
    };
    let plain_path = lock_path.with_file_name(plain_name);
    if plain_path.exists() || path::compressed_rollout_path(&plain_path).exists() {
        return Ok(false);
    }
    let metadata = std::fs::symlink_metadata(lock_path)?;
    if !metadata.is_file() || metadata.len() != 0 {
        return Ok(false);
    }
    // Windows denies this open while *any* read/write handle exists, even
    // one waiting for an OS lock. Delete-on-close excludes new opens until the
    // name is removed. Never unlink POSIX lock inodes: waiters may still own them.
    const DELETE: u32 = 0x0001_0000;
    const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    match std::fs::OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(0)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(lock_path)
    {
        Ok(file) => {
            drop(file);
            Ok(true)
        }
        Err(error) if matches!(error.raw_os_error(), Some(2 | 5 | 32 | 303)) => Ok(false),
        Err(error) => Err(error),
    }
}

fn rollout_lock_path(path: &Path) -> PathBuf {
    let plain_path = plain_rollout_path(path);
    let file_name = plain_path
        .file_name()
        .map(OsStr::to_os_string)
        .unwrap_or_else(|| OsStr::new("rollout").to_os_string());
    plain_path.with_file_name(format!(".{}.lock", file_name.to_string_lossy()))
}

fn rollout_write_lock_path(path: &Path) -> PathBuf {
    let plain_path = plain_rollout_path(path);
    let file_name = plain_path
        .file_name()
        .map(OsStr::to_os_string)
        .unwrap_or_else(|| OsStr::new("rollout").to_os_string());
    plain_path.with_file_name(format!(".{}.write.lock", file_name.to_string_lossy()))
}

pub(crate) fn materialize_rollout_for_append_locked(path: &Path) -> io::Result<PathBuf> {
    let plain_path = plain_rollout_path(path);
    if plain_path.exists() {
        metrics::materialize("plain_exists");
        return Ok(plain_path);
    }
    let compressed_path = path::compressed_rollout_path(plain_path.as_path());
    if !compressed_path.exists() {
        metrics::materialize("missing");
        return Ok(plain_path);
    }

    let temp_path = temp_path_for(plain_path.as_path(), "decompress");
    if let Some(parent) = plain_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let result: io::Result<()> = (|| {
        let compressed_metadata = std::fs::metadata(compressed_path.as_path())?;
        let modified = compressed_metadata.modified()?;
        let permissions = compressed_metadata.permissions();
        {
            let input = File::open(compressed_path.as_path())?;
            let mut decoder = zstd::stream::read::Decoder::new(input)?;
            let mut output = create_file_with_permissions(temp_path.as_path(), &permissions)?;
            io::copy(&mut decoder, &mut output)?;
            output.flush()?;
            output.set_times(FileTimes::new().set_modified(modified))?;
            output.sync_all()?;
        }
        match std::fs::hard_link(temp_path.as_path(), plain_path.as_path()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => persist_temp_file_noclobber(temp_path.as_path(), plain_path.as_path())?,
        }
        let _ = std::fs::remove_file(temp_path.as_path());
        match std::fs::remove_file(compressed_path.as_path()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(
                path = %compressed_path.display(),
                error = %err,
                "rollout materialized but compressed source cleanup failed"
            ),
        }
        Ok(())
    })();
    if let Err(err) = result {
        let _ = std::fs::remove_file(temp_path.as_path());
        if err.kind() == io::ErrorKind::NotFound && plain_path.exists() {
            metrics::materialize("materialized_concurrently");
            return Ok(plain_path);
        }
        metrics::materialize("failed");
        tracing::warn!(path = %plain_path.display(), error = %err, "rollout materialization failed");
        return Err(err);
    }
    metrics::materialize("decompressed");
    Ok(plain_path)
}

fn persist_temp_file_noclobber(temp_path: &Path, destination: &Path) -> io::Result<()> {
    let temp_path = tempfile::TempPath::try_from_path(temp_path)?;
    match temp_path.persist_noclobber(destination) {
        Ok(()) => Ok(()),
        Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err.error),
    }
}

/// Returns the plain `.jsonl` path for a plain or compressed rollout path.
pub fn plain_rollout_path(path: &Path) -> PathBuf {
    path::plain_rollout_path(path)
}

/// Parses a rollout file name, returning its plain `.jsonl` name when valid.
pub(crate) fn parse_rollout_file_name(name: &str) -> Option<&str> {
    file_name::parse_rollout_file_name(name)
}

/// A discovered rollout file, represented by exactly one physical path.
///
/// This keeps directory walkers from reimplementing the plain/compressed
/// precedence rules. The physical path may point at either `.jsonl` or
/// `.jsonl.zst`, while `plain_file_name` is always the canonical `.jsonl`
/// filename used for timestamp and id parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RolloutFile {
    path: PathBuf,
    plain_file_name: String,
}

impl RolloutFile {
    /// Creates a logical rollout file from a physical path found during discovery.
    ///
    /// Returns `None` for non-rollout names and for compressed siblings hidden by
    /// an existing plain `.jsonl` file.
    pub(crate) fn from_path(path: PathBuf) -> Option<Self> {
        let file_name = path.file_name().and_then(|name| name.to_str())?;
        let plain_file_name = file_name::parse_rollout_file_name(file_name)?.to_string();
        if path::should_skip_compressed_sibling(path.as_path()) {
            return None;
        }

        Some(Self {
            path,
            plain_file_name,
        })
    }

    /// Returns the physical path that should be opened for reads.
    pub(crate) fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// Returns the canonical `.jsonl` filename for timestamp and id parsing.
    pub(crate) fn plain_file_name(&self) -> &str {
        self.plain_file_name.as_str()
    }

    /// Returns whether the physical path is the compressed representation.
    pub(crate) fn is_compressed(&self) -> bool {
        path::is_compressed_rollout_path(self.path.as_path())
    }

    /// Consumes the entry and returns the physical path that should be read.
    pub(crate) fn into_path(self) -> PathBuf {
        self.path
    }
}

/// Line-oriented rollout reader returned by [`open_rollout_line_reader`].
pub struct RolloutLineReader {
    inner: RolloutLineReaderInner,
    path: PathBuf,
    last_line_terminated: bool,
    enroll_payloads: bool,
    unresolved_payloads: usize,
}

enum RolloutLineReaderInner {
    Plain(tokio::io::BufReader<tokio::fs::File>),
    Blocking(BlockingRolloutLineReader),
}

struct BlockingRolloutLineReader {
    receiver: tokio::sync::mpsc::Receiver<io::Result<(String, bool)>>,
    task: Option<tokio::task::JoinHandle<io::Result<()>>>,
}

impl RolloutLineReader {
    pub(crate) fn enroll_payload_dependencies(&mut self) {
        self.enroll_payloads = true;
    }
    pub(crate) fn last_line_terminated(&self) -> bool {
        self.last_line_terminated
    }

    pub(crate) fn unresolved_payloads(&self) -> usize {
        self.unresolved_payloads
    }

    /// Reads the next JSONL record from the rollout.
    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        let line = match &mut self.inner {
            RolloutLineReaderInner::Plain(reader) => {
                let mut line = Vec::new();
                if reader.read_until(b'\n', &mut line).await? == 0 {
                    return Ok(None);
                }
                let terminated = line.last() == Some(&b'\n');
                Ok(Some((decode_rollout_line(line), terminated)))
            }
            RolloutLineReaderInner::Blocking(reader) => match reader.receiver.recv().await {
                Some(line) => line.map(Some),
                None => {
                    if let Some(task) = reader.task.take() {
                        task.await.map_err(io::Error::other)??;
                    }
                    Ok(None)
                }
            },
        }?;
        let Some((line, terminated)) = line else {
            return Ok(None);
        };
        self.last_line_terminated = terminated;
        if !crate::payload_artifact::may_be_reference(&line) {
            return Ok(Some(line));
        }
        let path = self.path.clone();
        let enroll = self.enroll_payloads;
        match tokio::task::spawn_blocking(move || {
            if enroll {
                crate::payload_artifact::hydrate_line_with_enrollment(&path, line, true)
            } else {
                crate::payload_artifact::hydrate_line(&path, line)
            }
        })
        .await
        .map_err(io::Error::other)?
        {
            Ok(line) => Ok(Some(line)),
            Err(error) => {
                self.unresolved_payloads = self.unresolved_payloads.saturating_add(1);
                tracing::warn!(path = %self.path.display(), %error, "skipping unreadable rollout payload");
                // Keep the record slot, like torn UTF-8/JSON: consumers continue
                // and the recorder counts the malformed record.
                Ok(Some("invalid rollout payload record".to_owned()))
            }
        }
    }
}

/// Converts one raw JSONL record, without its line ending, into text.
///
/// Records are written as complete UTF-8 lines, but a process killed mid-append can
/// tear the last one inside a multi-byte character, and the next append then
/// terminates that fragment in place. Emit a non-JSON record for invalid UTF-8 so
/// callers count and skip it without losing later records. Lossy decoding could
/// instead turn corrupted bytes inside a JSON string into apparently valid evidence.
fn decode_rollout_line(mut line: Vec<u8>) -> String {
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    String::from_utf8(line).unwrap_or_else(|_| "invalid UTF-8 rollout record".to_owned())
}

/// Hydrates a selected rollout for feedback on a blocking thread. Both raw
/// records and referenced bodies are bounded before allocation. On any gap the
/// caller omits this attachment with the record-specific error, never silently
/// uploads a smaller stub-only history. The source and sidecars are read-only.
pub fn read_rollout_for_feedback(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    use std::io::BufRead;
    use std::io::Read;

    let file = File::open(path)?;
    let input: Box<dyn Read> = if path::is_compressed_rollout_path(path) {
        Box::new(zstd::stream::read::Decoder::new(file)?)
    } else {
        Box::new(file)
    };
    let mut reader = io::BufReader::new(input);
    let mut output = Vec::new();
    let mut record = 0_u64;
    loop {
        let remaining = max_bytes.saturating_sub(output.len());
        let mut line = Vec::new();
        let read = reader
            .by_ref()
            .take(remaining as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(output);
        }
        record = record.saturating_add(1);
        if read > remaining {
            return Err(io::Error::other(format!(
                "rollout record {record} exceeds the remaining {remaining}-byte export budget"
            )));
        }
        let line = String::from_utf8(line)
            .map_err(|_| io::Error::other(format!("rollout record {record} has invalid UTF-8")))?;
        let line = crate::payload_artifact::hydrate_line_bounded(path, line, remaining as u64)
            .map_err(|error| io::Error::other(format!("rollout record {record}: {error}")))?;
        let needs_newline = !line.ends_with('\n');
        if line.len().saturating_add(usize::from(needs_newline)) > remaining {
            return Err(io::Error::other(format!(
                "hydrated rollout record {record} exceeds the remaining {remaining}-byte export budget"
            )));
        }
        output.extend_from_slice(line.as_bytes());
        if needs_newline {
            output.push(b'\n');
        }
    }
}

mod worker {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::fs::FileTimes;
    use std::fs::Permissions;
    use std::io;
    use std::io::Write;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;
    use std::time::Instant;
    use std::time::SystemTime;

    use tracing::debug;
    use tracing::info;
    use tracing::warn;

    use tokio::task::JoinSet;

    use crate::ARCHIVED_SESSIONS_SUBDIR;
    use crate::SESSIONS_SUBDIR;

    use super::RolloutFile;
    use super::metrics;
    use super::path;
    use super::try_lock_rollout_for_compression;

    const TEMP_SUFFIX: &str = ".tmp";
    const COMPRESSION_LEVEL: i32 = 3;
    const MIN_ROLLOUT_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    const RUN_MARKER_STALE_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
    const TEMP_FILE_STALE_AFTER: Duration = RUN_MARKER_STALE_AFTER;
    // Stop scheduling at this deadline, then drain in-flight compression jobs.
    const WORKER_MAX_RUNTIME: Duration = Duration::from_secs(5 * 60 * 60);
    const RUN_MARKER_FILE_NAME: &str = "rollout-compression.lock";
    const MAX_CONCURRENT_COMPRESSION_JOBS: usize = 2;

    #[derive(Default)]
    struct CompressionStats {
        scanned: usize,
        compressed: usize,
        skipped: usize,
        failed: usize,
    }

    // Keep the inode stable: ownership is the OS lock, and nonempty contents
    // record the cooldown after a completed run.
    pub(super) struct CompressionRunMarker {
        file: File,
        clear_on_drop: bool,
    }

    impl CompressionRunMarker {
        pub(super) fn try_claim(codex_home: &Path) -> io::Result<Option<Self>> {
            let marker_dir = codex_home.join(".tmp");
            std::fs::create_dir_all(&marker_dir)?;
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(marker_dir.join(RUN_MARKER_FILE_NAME))?;
            match file.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
                Err(err) => return Err(err.into()),
            }
            let metadata = file.metadata()?;
            let stale = metadata
                .modified()
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age >= RUN_MARKER_STALE_AFTER);
            if metadata.len() > 0 && !stale {
                return Ok(None);
            }
            file.set_len(0)?;
            writeln!(
                file,
                "pid={} started_at={:?}",
                std::process::id(),
                SystemTime::now()
            )?;
            Ok(Some(Self {
                file,
                clear_on_drop: true,
            }))
        }

        pub(super) fn persist(mut self) {
            self.clear_on_drop = false;
        }
    }

    impl Drop for CompressionRunMarker {
        fn drop(&mut self) {
            if self.clear_on_drop {
                let _ = self.file.set_len(0);
            }
        }
    }

    pub(super) fn spawn(
        handle: &tokio::runtime::Handle,
        codex_home: PathBuf,
        stopped: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        handle.spawn(run_periodically_until(
            codex_home,
            RUN_MARKER_STALE_AFTER,
            stopped,
        ))
    }

    /// Workers start with their process, and a long-lived app-server can start
    /// inside another process's cooldown. Retry every cooldown period so cold
    /// rollouts are still compressed while such a process keeps running.
    #[cfg(test)]
    pub(super) async fn run_periodically(codex_home: PathBuf, interval: Duration) {
        let (_stop, stopped) = tokio::sync::watch::channel(false);
        run_periodically_until(codex_home, interval, stopped).await;
    }

    async fn run_periodically_until(
        codex_home: PathBuf,
        interval: Duration,
        mut stopped: tokio::sync::watch::Receiver<bool>,
    ) {
        loop {
            if *stopped.borrow() {
                break;
            }
            if let Err(err) = run(codex_home.clone()).await {
                warn!(
                    "rollout compression worker failed for {}: {err}",
                    codex_home.display()
                );
            }
            if let Err(error) =
                crate::payload_artifact::reclaim_payload_artifacts(&codex_home).await
            {
                warn!(%error, "rollout payload reclamation incomplete; blobs retained");
            }
            tokio::select! {
                _ = stopped.changed() => break,
                _ = tokio::time::sleep(interval) => {}
            }
        }
    }

    pub(super) async fn run(codex_home: PathBuf) -> io::Result<()> {
        let marker_home = codex_home.clone();
        let marker = match tokio::task::spawn_blocking(move || {
            CompressionRunMarker::try_claim(marker_home.as_path())
        })
        .await
        .map_err(io::Error::other)?
        {
            Ok(Some(marker)) => marker,
            Ok(None) => {
                metrics::run("skipped_already_running");
                debug!(
                    "rollout compression worker recently ran or is already running for {}",
                    codex_home.display()
                );
                return Ok(());
            }
            Err(err) => {
                metrics::run("failed");
                return Err(err);
            }
        };

        metrics::run("started");
        let started_at = Instant::now();
        let result = async {
            let mut stats = CompressionStats::default();
            for root in [
                codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
                codex_home.join(SESSIONS_SUBDIR),
            ] {
                if started_at.elapsed() >= WORKER_MAX_RUNTIME {
                    break;
                }
                compress_rollouts_in_root(root.as_path(), started_at, &mut stats).await?;
            }
            Ok::<_, io::Error>(stats)
        }
        .await;
        let stats = match result {
            Ok(stats) => stats,
            Err(err) => {
                metrics::run("failed");
                metrics::run_duration("failed", started_at.elapsed());
                tokio::task::spawn_blocking(move || drop(marker))
                    .await
                    .map_err(io::Error::other)?;
                return Err(err);
            }
        };
        info!(
            "rollout compression worker finished: scanned={}, compressed={}, skipped={}, failed={}",
            stats.scanned, stats.compressed, stats.skipped, stats.failed
        );
        metrics::run("completed");
        metrics::run_duration("completed", started_at.elapsed());
        marker.persist();
        Ok(())
    }

    async fn compress_rollouts_in_root(
        root: &Path,
        started_at: Instant,
        stats: &mut CompressionStats,
    ) -> io::Result<()> {
        if !tokio::fs::try_exists(root).await.unwrap_or(false) {
            return Ok(());
        }
        let mut stack = vec![root.to_path_buf()];
        let mut jobs = JoinSet::new();
        while let Some(dir) = stack.pop() {
            if started_at.elapsed() >= WORKER_MAX_RUNTIME {
                break;
            }
            let mut read_dir = match tokio::fs::read_dir(dir.as_path()).await {
                Ok(read_dir) => read_dir,
                Err(err) => {
                    warn!(
                        "failed to read rollout compression directory {}: {err}",
                        dir.display()
                    );
                    continue;
                }
            };
            loop {
                let entry = match read_dir.next_entry().await {
                    Ok(Some(entry)) => entry,
                    Ok(None) => break,
                    Err(err) => {
                        drain_compression_jobs(&mut jobs, stats).await;
                        return Err(err);
                    }
                };
                if started_at.elapsed() >= WORKER_MAX_RUNTIME {
                    break;
                }
                let path = entry.path();
                let file_type = match entry.file_type().await {
                    Ok(file_type) => file_type,
                    Err(err) => {
                        warn!(
                            "failed to read rollout compression file type {}: {err}",
                            path.display()
                        );
                        continue;
                    }
                };
                if file_type.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                cleanup_stale_temp(&entry).await;
                #[cfg(windows)]
                if path
                    .extension()
                    .is_some_and(|extension| extension == "lock")
                {
                    let lock_path = path.clone();
                    match tokio::task::spawn_blocking(move || {
                        super::cleanup_orphan_rollout_lock(&lock_path)
                    })
                    .await
                    {
                        Ok(Ok(_)) => {}
                        result => debug!(?result, "orphan rollout lock cleanup skipped"),
                    }
                }
                let Some(rollout_file) = RolloutFile::from_path(path) else {
                    continue;
                };
                if rollout_file.is_compressed() {
                    continue;
                }
                let path = rollout_file.into_path();
                stats.scanned = stats.scanned.saturating_add(1);
                metrics::file("scanned");
                while jobs.len() >= MAX_CONCURRENT_COMPRESSION_JOBS {
                    collect_next_compression_job(&mut jobs, stats).await;
                }
                jobs.spawn_blocking(move || {
                    let started_at = Instant::now();
                    let result = compress_rollout_if_cold_blocking(path.as_path());
                    let duration = started_at.elapsed();
                    (path, duration, result)
                });
            }
        }
        drain_compression_jobs(&mut jobs, stats).await;
        Ok(())
    }

    type CompressionJobResult = (PathBuf, Duration, io::Result<CompressionMeasurement>);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CompressionOutcome {
        Compressed,
        SkippedNotCold,
        SkippedChanged,
        SkippedAlreadyCompressed,
        SkippedLocked,
    }

    impl CompressionOutcome {
        fn tag(self) -> &'static str {
            match self {
                CompressionOutcome::Compressed => "compressed",
                CompressionOutcome::SkippedNotCold => "skipped_not_cold",
                CompressionOutcome::SkippedChanged => "skipped_changed",
                CompressionOutcome::SkippedAlreadyCompressed => "skipped_already_compressed",
                CompressionOutcome::SkippedLocked => "skipped_locked",
            }
        }
    }

    struct CompressionMeasurement {
        outcome: CompressionOutcome,
        source_bytes: Option<u64>,
        compressed_bytes: Option<u64>,
    }

    impl CompressionMeasurement {
        fn new(
            outcome: CompressionOutcome,
            source_bytes: Option<u64>,
            compressed_bytes: Option<u64>,
        ) -> Self {
            Self {
                outcome,
                source_bytes,
                compressed_bytes,
            }
        }
    }

    enum ColdFileState {
        Cold(FileState),
        NotCold(Option<FileState>),
    }

    async fn drain_compression_jobs(
        jobs: &mut JoinSet<CompressionJobResult>,
        stats: &mut CompressionStats,
    ) {
        while !jobs.is_empty() {
            collect_next_compression_job(jobs, stats).await;
        }
    }

    async fn collect_next_compression_job(
        jobs: &mut JoinSet<CompressionJobResult>,
        stats: &mut CompressionStats,
    ) {
        let Some(result) = jobs.join_next().await else {
            return;
        };
        match result {
            Ok((_, duration, Ok(measurement))) => {
                let outcome = measurement.outcome;
                match outcome {
                    CompressionOutcome::Compressed => {
                        stats.compressed = stats.compressed.saturating_add(1);
                    }
                    CompressionOutcome::SkippedNotCold
                    | CompressionOutcome::SkippedChanged
                    | CompressionOutcome::SkippedAlreadyCompressed
                    | CompressionOutcome::SkippedLocked => {
                        stats.skipped = stats.skipped.saturating_add(1);
                    }
                }
                metrics::file(outcome.tag());
                metrics::file_duration(outcome.tag(), duration);
                if let Some(source_bytes) = measurement.source_bytes {
                    metrics::source_bytes(outcome.tag(), source_bytes);
                }
                if let Some(compressed_bytes) = measurement.compressed_bytes {
                    metrics::compressed_bytes(outcome.tag(), compressed_bytes);
                    if let Some(source_bytes) = measurement.source_bytes {
                        metrics::compression_ratio(outcome.tag(), source_bytes, compressed_bytes);
                    }
                }
            }
            Ok((path, duration, Err(err))) => {
                stats.failed = stats.failed.saturating_add(1);
                metrics::file("failed");
                metrics::file_duration("failed", duration);
                warn!("failed to compress rollout {}: {err}", path.display());
            }
            Err(err) => {
                stats.failed = stats.failed.saturating_add(1);
                metrics::file("failed");
                warn!("rollout compression task failed: {err}");
            }
        }
    }

    fn compress_rollout_if_cold_blocking(path: &Path) -> io::Result<CompressionMeasurement> {
        compress_rollout_if_cold_blocking_with_hook(path, || Ok(()))
    }

    fn compress_rollout_if_cold_blocking_with_hook(
        path: &Path,
        after_final_check: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<CompressionMeasurement> {
        let Some(_rollout_lock) = try_lock_rollout_for_compression(path)? else {
            return Ok(CompressionMeasurement::new(
                CompressionOutcome::SkippedLocked,
                /*source_bytes*/ None,
                /*compressed_bytes*/ None,
            ));
        };
        let before = match cold_file_state(path)? {
            ColdFileState::Cold(state) => state,
            ColdFileState::NotCold(state) => {
                return Ok(CompressionMeasurement::new(
                    CompressionOutcome::SkippedNotCold,
                    state.map(|state| state.len),
                    /*compressed_bytes*/ None,
                ));
            }
        };
        let source_bytes = Some(before.len);
        let compressed_path = path::compressed_rollout_path(path);
        if compressed_path.exists() {
            return Ok(CompressionMeasurement::new(
                CompressionOutcome::SkippedAlreadyCompressed,
                source_bytes,
                /*compressed_bytes*/ None,
            ));
        }

        let temp_dir = compressed_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(temp_dir)?;
        let mut temp_file = tempfile::Builder::new()
            .prefix("rollout-compress-")
            .suffix(TEMP_SUFFIX)
            .tempfile_in(temp_dir)?;
        encode_zstd_to_writer(path, temp_file.as_file_mut())?;
        temp_file.as_file_mut().flush()?;
        verify_zstd(temp_file.path())?;
        if !same_file_state(path, &before)? {
            return Ok(CompressionMeasurement::new(
                CompressionOutcome::SkippedChanged,
                source_bytes,
                /*compressed_bytes*/ None,
            ));
        }
        set_file_metadata(temp_file.as_file(), before.modified, &before.permissions)?;
        temp_file.as_file().sync_all()?;
        let compressed_bytes = temp_file.as_file().metadata()?.len();

        let published = match temp_file.persist_noclobber(compressed_path.as_path()) {
            Ok(file) => file,
            Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => {
                return Ok(CompressionMeasurement::new(
                    CompressionOutcome::SkippedAlreadyCompressed,
                    source_bytes,
                    /*compressed_bytes*/ None,
                ));
            }
            Err(err) => return Err(err.error),
        };
        if !same_file_state(path, &before)? {
            super::remove_owned_rollout_file(&compressed_path, &published)?;
            return Ok(CompressionMeasurement::new(
                CompressionOutcome::SkippedChanged,
                source_bytes,
                /*compressed_bytes*/ None,
            ));
        }
        after_final_check()?;
        if let Err(error) = std::fs::remove_file(path) {
            if error.kind() == io::ErrorKind::NotFound {
                super::remove_owned_rollout_file(&compressed_path, &published).map_err(|cleanup| {
                    io::Error::other(format!("rollout source disappeared; compressed publication cleanup failed: {cleanup}"))
                })?;
            }
            return Err(error);
        }
        Ok(CompressionMeasurement::new(
            CompressionOutcome::Compressed,
            source_bytes,
            Some(compressed_bytes),
        ))
    }

    #[cfg(test)]
    pub(super) fn compress_rollout_if_cold_blocking_paused(
        path: &Path,
        reached_final_check: tokio::sync::oneshot::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) -> io::Result<()> {
        compress_rollout_if_cold_blocking_with_hook(path, move || {
            reached_final_check
                .send(())
                .map_err(|()| io::Error::other("compression pause receiver was dropped"))?;
            resume.recv().map_err(|err| {
                io::Error::other(format!("compression pause sender was dropped: {err}"))
            })?;
            Ok(())
        })
        .map(|_| ())
    }

    struct FileState {
        len: u64,
        modified: SystemTime,
        permissions: Permissions,
    }

    fn cold_file_state(path: &Path) -> io::Result<ColdFileState> {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(ColdFileState::NotCold(None));
            }
            Err(err) => return Err(err),
        };
        if !metadata.is_file() {
            return Ok(ColdFileState::NotCold(None));
        }
        let modified = metadata.modified()?;
        let state = FileState {
            len: metadata.len(),
            modified,
            permissions: metadata.permissions(),
        };
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);
        if age < MIN_ROLLOUT_AGE {
            return Ok(ColdFileState::NotCold(Some(state)));
        }
        Ok(ColdFileState::Cold(state))
    }

    fn same_file_state(path: &Path, expected: &FileState) -> io::Result<bool> {
        match std::fs::metadata(path) {
            Ok(metadata) => Ok(metadata.len() == expected.len
                && metadata.modified()? == expected.modified
                && metadata.permissions() == expected.permissions),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn encode_zstd_to_writer(source: &Path, output: impl Write) -> io::Result<()> {
        let mut input = File::open(source)?;
        let mut encoder = zstd::stream::write::Encoder::new(output, COMPRESSION_LEVEL)?;
        io::copy(&mut input, &mut encoder)?;
        encoder.finish()?;
        Ok(())
    }

    fn verify_zstd(path: &Path) -> io::Result<()> {
        let input = File::open(path)?;
        let mut decoder = zstd::stream::read::Decoder::new(input)?;
        let mut sink = io::sink();
        io::copy(&mut decoder, &mut sink)?;
        Ok(())
    }

    fn set_file_metadata(
        file: &File,
        modified: SystemTime,
        permissions: &Permissions,
    ) -> io::Result<()> {
        file.set_times(FileTimes::new().set_modified(modified))?;
        file.set_permissions(permissions.clone())
    }

    fn is_rollout_temp_file_name(name: &str) -> bool {
        let Some(stem) = name.strip_suffix(TEMP_SUFFIX) else {
            return false;
        };
        if let Some(random) = stem.strip_prefix("rollout-compress-") {
            return !random.is_empty() && random.bytes().all(|byte| byte.is_ascii_alphanumeric());
        }
        if !stem.starts_with("rollout-") {
            return false;
        }
        // Retain cleanup of the legacy compression temporary name.
        if stem.ends_with(".jsonl.zst") {
            return true;
        }
        let Some((_, process_and_counter)) = stem.rsplit_once(".jsonl.decompress.") else {
            return false;
        };
        let Some((process, counter)) = process_and_counter.split_once('.') else {
            return false;
        };
        [process, counter]
            .into_iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    }

    async fn cleanup_stale_temp(entry: &tokio::fs::DirEntry) {
        let path = entry.path();
        let cleanup_path = path.clone();
        // The worker owns physical exclusion through unlink, including when its
        // async observer is cancelled while storage is blocked.
        match tokio::task::spawn_blocking(move || cleanup_stale_temp_blocking(&cleanup_path)).await
        {
            Ok(Ok(true)) => metrics::temp_cleanup("removed"),
            Ok(Ok(false)) => {}
            result => {
                metrics::temp_cleanup("failed");
                warn!(?result, path = %path.display(), "stale rollout temporary cleanup deferred");
            }
        }
    }

    pub(super) fn cleanup_stale_temp_blocking(path: &Path) -> io::Result<bool> {
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        if !is_rollout_temp_file_name(name) {
            return Ok(false);
        }
        // Materializers inherit the cold source's mtime before publication.
        // Age cannot establish that the corresponding producer has finished.
        let rollout = name
            .rsplit_once(".decompress.")
            .map(|(rollout, _)| rollout)
            .or_else(|| {
                name.strip_suffix(TEMP_SUFFIX)
                    .filter(|name| name.ends_with(".jsonl.zst"))
            });
        let _owner = if let Some(rollout) = rollout {
            match super::try_lock_rollout_for_maintenance(&path.with_file_name(rollout)) {
                Ok(owner) => Some(owner),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) => return Err(error),
            }
        } else {
            None
        }; // Random compression stages are covered by the run marker.
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !metadata.is_file()
            || !SystemTime::now()
                .duration_since(metadata.modified()?)
                .is_ok_and(|age| age >= TEMP_FILE_STALE_AFTER)
        {
            return Ok(false);
        }
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

mod metrics {
    use std::time::Duration;

    const FILE_COMPRESSED_BYTES_HISTOGRAM: &str = "codex.rollout_compression.file.compressed_bytes";
    const FILE_COUNTER: &str = "codex.rollout_compression.file";
    const FILE_DURATION_HISTOGRAM: &str = "codex.rollout_compression.file.duration_ms";
    const FILE_SOURCE_BYTES_HISTOGRAM: &str = "codex.rollout_compression.file.source_bytes";
    const FILE_COMPRESSION_RATIO_HISTOGRAM: &str =
        "codex.rollout_compression.file.compression_ratio";
    const MATERIALIZE_COUNTER: &str = "codex.rollout_compression.materialize";
    const RUN_COUNTER: &str = "codex.rollout_compression.run";
    const RUN_DURATION_HISTOGRAM: &str = "codex.rollout_compression.run.duration_ms";
    const RATIO_BASIS_POINTS: u128 = 10_000;
    const TEMP_CLEANUP_COUNTER: &str = "codex.rollout_compression.temp_cleanup";

    pub(super) fn file(outcome: &'static str) {
        counter(FILE_COUNTER, &[("outcome", outcome)]);
    }

    pub(super) fn file_duration(outcome: &'static str, duration: Duration) {
        duration_histogram(FILE_DURATION_HISTOGRAM, duration, &[("outcome", outcome)]);
    }

    pub(super) fn source_bytes(outcome: &'static str, bytes: u64) {
        histogram(
            FILE_SOURCE_BYTES_HISTOGRAM,
            saturating_i64(bytes),
            &[("outcome", outcome)],
        );
    }

    pub(super) fn compressed_bytes(outcome: &'static str, bytes: u64) {
        histogram(
            FILE_COMPRESSED_BYTES_HISTOGRAM,
            saturating_i64(bytes),
            &[("outcome", outcome)],
        );
    }

    pub(super) fn compression_ratio(
        outcome: &'static str,
        source_bytes: u64,
        compressed_bytes: u64,
    ) {
        if source_bytes == 0 {
            return;
        }
        // Keep the ratio histogram integer-valued while preserving sub-percent precision.
        let ratio = (u128::from(compressed_bytes) * RATIO_BASIS_POINTS) / u128::from(source_bytes);
        histogram(
            FILE_COMPRESSION_RATIO_HISTOGRAM,
            saturating_i64(ratio),
            &[("outcome", outcome)],
        );
    }

    pub(super) fn materialize(outcome: &'static str) {
        counter(MATERIALIZE_COUNTER, &[("outcome", outcome)]);
    }

    pub(super) fn run(status: &'static str) {
        counter(RUN_COUNTER, &[("status", status)]);
    }

    pub(super) fn run_duration(status: &'static str, duration: Duration) {
        duration_histogram(RUN_DURATION_HISTOGRAM, duration, &[("status", status)]);
    }

    pub(super) fn temp_cleanup(outcome: &'static str) {
        counter(TEMP_CLEANUP_COUNTER, &[("outcome", outcome)]);
    }

    fn counter(name: &str, tags: &[(&str, &str)]) {
        let Some(metrics) = codex_otel::global() else {
            return;
        };
        let _ = metrics.counter(name, /*inc*/ 1, tags);
    }

    fn histogram(name: &str, value: i64, tags: &[(&str, &str)]) {
        let Some(metrics) = codex_otel::global() else {
            return;
        };
        let _ = metrics.histogram(name, value, tags);
    }

    fn duration_histogram(name: &str, duration: Duration, tags: &[(&str, &str)]) {
        let Some(metrics) = codex_otel::global() else {
            return;
        };
        let _ = metrics.record_duration(name, duration, tags);
    }

    fn saturating_i64(value: impl TryInto<i64>) -> i64 {
        value.try_into().unwrap_or(i64::MAX)
    }
}

/// Returns the existing rollout path, preferring the plain `.jsonl` file over
/// its `.jsonl.zst` compressed sibling.
pub async fn existing_rollout_path(path: &Path) -> Option<PathBuf> {
    path::existing_rollout_path(path).await
}

mod path {
    use std::ffi::OsStr;
    use std::path::Path;
    use std::path::PathBuf;

    use super::COMPRESSED_SUFFIX;

    pub(super) fn compressed_rollout_path(path: &Path) -> PathBuf {
        if is_compressed_rollout_path(path) {
            return path.to_path_buf();
        }
        let mut file_name = path
            .file_name()
            .map(OsStr::to_os_string)
            .unwrap_or_else(|| OsStr::new("rollout.jsonl").to_os_string());
        file_name.push(COMPRESSED_SUFFIX);
        path.with_file_name(file_name)
    }

    pub(super) fn plain_rollout_path(path: &Path) -> PathBuf {
        let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
            return path.to_path_buf();
        };
        let Some(plain_file_name) = file_name.strip_suffix(COMPRESSED_SUFFIX) else {
            return path.to_path_buf();
        };
        path.with_file_name(plain_file_name)
    }

    pub(super) fn is_compressed_rollout_path(path: &Path) -> bool {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.ends_with(".jsonl.zst"))
    }

    pub(super) fn should_skip_compressed_sibling(path: &Path) -> bool {
        is_compressed_rollout_path(path) && plain_rollout_path(path).exists()
    }

    pub(super) async fn existing_rollout_path(path: &Path) -> Option<PathBuf> {
        let plain_path = plain_rollout_path(path);
        if matches!(tokio::fs::metadata(plain_path.as_path()).await, Ok(metadata) if metadata.is_file())
        {
            return Some(plain_path);
        }
        let compressed_path = compressed_rollout_path(plain_path.as_path());
        if matches!(tokio::fs::metadata(compressed_path.as_path()).await, Ok(metadata) if metadata.is_file())
        {
            return Some(compressed_path);
        }
        None
    }
}

mod file_name {
    use super::COMPRESSED_SUFFIX;

    pub(super) fn parse_rollout_file_name(name: &str) -> Option<&str> {
        let name = name.strip_suffix(COMPRESSED_SUFFIX).unwrap_or(name);
        if name.starts_with("rollout-") && name.ends_with(".jsonl") {
            Some(name)
        } else {
            None
        }
    }
}

mod reader {
    use std::fs::File;
    use std::io;
    use std::io::BufRead;
    use std::io::Read;
    use std::path::Path;

    use super::BlockingRolloutLineReader;
    use super::COMPRESSED_READER_CHANNEL_CAPACITY;
    use super::RolloutLineReader;
    use super::RolloutLineReaderInner;
    use super::decode_rollout_line;
    use super::path;

    pub(super) async fn open_once(path: &Path) -> io::Result<RolloutLineReader> {
        let path = path::existing_rollout_path(path)
            .await
            .unwrap_or_else(|| path.to_path_buf());
        let source_path = path.clone();
        if path::is_compressed_rollout_path(path.as_path()) {
            let mut reader = tokio::task::spawn_blocking(move || {
                let input = File::open(path.as_path())?;
                let decoder = zstd::stream::read::Decoder::new(input)?;
                Ok::<_, io::Error>(io::BufReader::new(Box::new(decoder) as Box<dyn Read + Send>))
            })
            .await
            .map_err(io::Error::other)??;
            let (sender, receiver) = tokio::sync::mpsc::channel(COMPRESSED_READER_CHANNEL_CAPACITY);
            let task = tokio::task::spawn_blocking(move || {
                loop {
                    let mut line = Vec::new();
                    let line = match reader.read_until(b'\n', &mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            let terminated = line.last() == Some(&b'\n');
                            Ok((decode_rollout_line(line), terminated))
                        }
                        Err(error) => Err(error),
                    };
                    let failed = line.is_err();
                    if sender.blocking_send(line).is_err() || failed {
                        break;
                    }
                }
                Ok(())
            });
            return Ok(RolloutLineReader {
                path: source_path,
                last_line_terminated: true,
                enroll_payloads: false,
                unresolved_payloads: 0,
                inner: RolloutLineReaderInner::Blocking(BlockingRolloutLineReader {
                    receiver,
                    task: Some(task),
                }),
            });
        }
        let file = tokio::fs::File::open(path).await?;
        Ok(RolloutLineReader {
            path: source_path,
            last_line_terminated: true,
            enroll_payloads: false,
            unresolved_payloads: 0,
            inner: RolloutLineReaderInner::Plain(tokio::io::BufReader::new(file)),
        })
    }
}

fn create_file_with_permissions(path: &Path, permissions: &Permissions) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.set_permissions(permissions.clone())?;
    Ok(file)
}

fn temp_path_for(path: &Path, operation: &str) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(OsStr::to_os_string)
        .unwrap_or_else(|| OsStr::new("rollout").to_os_string());
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    file_name.push(format!(
        ".{operation}.{}.{counter}{TEMP_SUFFIX}",
        std::process::id()
    ));
    path.with_file_name(file_name)
}

#[cfg(test)]
#[path = "compression_tests.rs"]
mod tests;
