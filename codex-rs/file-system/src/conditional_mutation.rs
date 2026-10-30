use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::FileExt;
use std::os::windows::fs::MetadataExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::path::PathBuf;
use tempfile::NamedTempFile;
use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows_sys::Win32::Foundation::ERROR_INVALID_OWNER;
use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FileDispositionInfo;
use windows_sys::Win32::Storage::FileSystem::FileRenameInfo;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;

/// An error after capture retains the actual displaced object, never just a
/// copy of an earlier observation. Callers must not retry/rollback it as exact.
#[derive(Debug)]
pub enum FileMutationOutcome {
    Committed,
    Conflict,
    RecoveryRequired {
        retained_path: std::path::PathBuf,
        message: String,
    },
}

// Native readers must not mistake the deliberate capture/publication interval
// for stable absence. Keep this advisory namespace outside the workspace and
// never unlink a lock file while another process can still own its inode.
fn patch_capture_lock_path(path: &Path) -> io::Result<PathBuf> {
    use std::hash::Hash;
    use std::hash::Hasher;
    let parent = std::fs::canonicalize(
        path.parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing parent"))?,
    )?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?;
    let identity = parent.join(name).to_string_lossy().to_lowercase();
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    identity.hash(&mut hash);
    // Bound the shared namespace without ever unlinking a live lock.
    // Collisions conservatively return transient contention, not absence.
    Ok(patch_capture_directory()
        .join(patch_capture_bucket(hash.finish() & (PATCH_CAPTURE_BUCKETS - 1))))
}

const PATCH_CAPTURE_BUCKETS: u64 = 0x1000;

fn patch_capture_directory() -> PathBuf {
    std::env::temp_dir().join("codex-patch-capture-v1")
}

fn patch_capture_bucket(bucket: u64) -> String {
    format!("{bucket:03x}.lock")
}

fn open_patch_capture_file(path: &Path) -> io::Result<File> {
    let lock_path = patch_capture_lock_path(path)?;
    open_patch_capture_lock(&lock_path)
}

fn open_patch_capture_lock(lock_path: &Path) -> io::Result<File> {
    // LockFileEx needs read OR write access, even for an exclusive lock. Never
    // request write access for an existing admission object: read-only agents
    // may read source files but cannot write this shared temp namespace.
    match File::open(lock_path) {
        Ok(file) => return Ok(file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::create_dir_all(
        lock_path
            .parent()
            .ok_or_else(|| io::Error::other("capture lock has no parent"))?,
    )?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
}

/// Initialize only the temp-side admission object before entering a sandbox.
/// The returned path may be granted read access; it contains no source bytes.
pub fn prepare_patch_capture(path: &Path) -> io::Result<PathBuf> {
    let resolved = crate::atomic_write::resolve_symlink_write_paths(path)?;
    let lock_path = patch_capture_lock_path(&resolved.write_path)?;
    drop(open_patch_capture_lock(&lock_path)?);
    Ok(lock_path)
}

/// Initialize every admission bucket before a sandboxed helper guards paths
/// unknown here, so it only opens existing locks read-only, and return the
/// directory it may be granted read access to. Blocking. Buckets are created
/// without truncation; one left missing fails only a probe that needs it. A
/// complete listing is trusted until the directory's last-write time changes,
/// as deleting any bucket (such as temp cleanup) does.
pub fn prepare_patch_capture_namespace() -> io::Result<PathBuf> {
    static PREPARED: std::sync::Mutex<Option<(PathBuf, std::time::SystemTime)>> =
        std::sync::Mutex::new(None);
    let directory = patch_capture_directory();
    let modified = || std::fs::metadata(&directory)?.modified();
    // Concurrent helper launches share one listing.
    let mut prepared = PREPARED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Ok(current) = modified()
        && prepared
            .as_ref()
            .is_some_and(|(path, at)| *path == directory && *at == current)
    {
        return Ok(directory);
    }
    std::fs::create_dir_all(&directory)?;
    // Sampled before listing, so a deletion racing the listing changes it.
    let listed_at = modified()?;
    let existing = std::fs::read_dir(&directory)?
        .flatten()
        .map(|entry| entry.file_name())
        .collect::<std::collections::HashSet<_>>();
    let mut missing = false;
    for bucket in (0..PATCH_CAPTURE_BUCKETS).map(patch_capture_bucket) {
        if !existing.contains(std::ffi::OsStr::new(&bucket)) {
            missing = true;
            let _ = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(directory.join(bucket));
        }
    }
    // Creating buckets changed the time; the next call records the settled listing.
    *prepared = (!missing).then(|| (directory.clone(), listed_at));
    Ok(directory)
}

/// Nonblocking capture admission for cooperating native writers. This is not
/// exclusion against arbitrary editors, nor permission to overwrite a name.
pub fn try_acquire_patch_capture(path: &Path) -> io::Result<File> {
    let file = open_patch_capture_file(path)?;
    fs2::FileExt::try_lock_exclusive(&file).map_err(|error| {
        if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "native patch capture is in progress; retry after its owner finishes",
            )
        } else {
            error
        }
    })?;
    Ok(file)
}

/// Pins native open admission, not a multi-file snapshot. Both readers and
/// writers create the temp-only lock before admission, closing the first-writer
/// race. Temp storage failures fail closed; contention is WouldBlock, not absence.
pub fn guard_patch_read(path: &Path) -> io::Result<Option<File>> {
    let resolved = crate::atomic_write::resolve_symlink_write_paths(path)?;
    let file = match open_patch_capture_file(&resolved.write_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    fs2::FileExt::try_lock_shared(&file).map_err(|error| {
        if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "native patch capture is in progress; this read does not establish that the file is absent",
            )
        } else {
            error
        }
    })?;
    Ok(Some(file))
}

/// Windows guarantee: deny concurrent write/delete opens on the actual old
/// object, compare under that mandatory share policy, then mutate that handle.
/// Replacement captures the old object and publishes with NO replace-existing
/// flag. There is a brief absent-name interval, never a torn file or overwrite
/// of a competing creator. Failures in that interval retain a recovery object.
/// A token that cannot install the old object's owner and DACL on a new file
/// rewrites a single-link old object instead, see `rewrite_captured_original`.
pub fn mutate_file_if_matches(
    path: &Path,
    expected: Option<&[u8]>,
    replacement: Option<&[u8]>,
) -> io::Result<FileMutationOutcome> {
    let _capture = try_acquire_patch_capture(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing parent"))?;
    let _parents = crate::lock_directory_chain(parent)?;
    let mut old = match OpenOptions::new()
        .read(true)
        .access_mode(0x8000_0000 | DELETE)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(file) => Some(file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let preimage = match (&old, expected) {
        (Some(file), Some(expected)) => {
            if !preimage_matches(file, expected)? {
                return Ok(FileMutationOutcome::Conflict);
            }
            Some(expected)
        }
        (None, None) => None,
        _ => return Ok(FileMutationOutcome::Conflict),
    };
    let Some(replacement) = replacement else {
        if let Some(file) = &old {
            delete_handle(file)?;
        }
        return Ok(FileMutationOutcome::Committed);
    };
    let mut staged = NamedTempFile::new_in(parent)?;
    // Preserve Windows security before the replacement bytes exist.
    if let Some(expected) = preimage
        && let Err(denied) = crate::atomic_write::copy_file_security(path, staged.path())
    {
        if !security_install_denied(&denied) {
            return Err(denied);
        }
        drop(staged);
        // The rewrite opens the object exclusively, so this pin must close first.
        drop(old.take());
        return rewrite_captured_original(path, parent, expected, replacement, denied);
    }
    staged.write_all(replacement)?;
    staged.flush()?;
    if let Some(file) = &old {
        staged
            .as_file()
            .set_permissions(file.metadata()?.permissions())?;
    }
    // Clear temporary attributes, retaining cleanup if publication fails.
    let (_, staged_path) = staged.keep().map_err(|error| error.error)?;
    let staged_path = tempfile::TempPath::try_from_path(staged_path)?;
    let retained = if let Some(file) = &old {
        let retained = recovery_path(parent)?;
        rename_handle(file, &retained)?;
        Some(retained)
    } else {
        None
    };
    let source: Vec<u16> = staged_path.as_os_str().encode_wide().chain([0]).collect();
    let destination: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let published = retry_transient_denial(|| {
        // SAFETY: both paths are live terminated UTF-16. Flags zero explicitly
        // refuses replacement; a creator in the capture/publication gap wins.
        if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0) } != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    });
    if let Err(error) = published {
        return match retained {
            Some(retained_path) => Ok(FileMutationOutcome::RecoveryRequired {
                retained_path,
                message: error.to_string(),
            }),
            None => Err(error),
        };
    }
    if let (Some(file), Some(retained_path)) = (&old, retained)
        && let Err(error) = retry_transient_denial(|| delete_handle(file))
    {
        return Ok(FileMutationOutcome::RecoveryRequired {
            retained_path,
            message: error.to_string(),
        });
    }
    Ok(FileMutationOutcome::Committed)
}

/// Compare through the pinned handle using fixed scratch storage. Drain the
/// same bounded read after a mismatch so a later I/O failure still takes priority.
fn preimage_matches(file: &File, expected: &[u8]) -> io::Result<bool> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "conditional mutation requires an ordinary file",
        ));
    }
    if metadata.permissions().readonly() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "destination is read-only",
        ));
    }
    if metadata.len() != expected.len() as u64 {
        return Ok(false);
    }
    let mut reader = file.take(expected.len() as u64 + 1);
    let mut scratch = [0u8; 64 * 1024];
    let mut offset = 0usize;
    let mut matches = true;
    loop {
        let length = match reader.read(&mut scratch) {
            Ok(0) => break,
            Ok(length) => length,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        matches &= expected.get(offset..)
            .is_some_and(|remaining| remaining.starts_with(&scratch[..length]));
        offset = offset.saturating_add(length);
    }
    Ok(matches && offset == expected.len())
}

/// The token cannot give a new file the old object's owner or DACL: setting
/// them needs WRITE_OWNER and WRITE_DAC on the staged file, and an owner other
/// than the token's own identities is not assignable without SeRestorePrivilege.
fn security_install_denied(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
        || error.raw_os_error() == Some(ERROR_INVALID_OWNER as i32)
}

/// Fallback for tokens that cannot install the old object's security on a new
/// file. A write-restricted sandbox token stages a file owned by the sandbox
/// identity, and its writable-root grants carry neither WRITE_OWNER nor
/// WRITE_DAC, so the replaced file's owner and DACL cannot be reproduced.
///
/// The old object itself receives the new bytes instead, so its owner, group,
/// DACL, attributes and streams never change. No other handle with data access
/// can coexist with the exclusive handle, so nobody observes partial bytes. The
/// bytes change only while the object is captured under a recovery name, so the
/// name never shows a torn file and a competing creator is never replaced. A
/// complete preimage copy exists until the object is republished, so a killed
/// writer cannot lose the original bytes. Linked objects keep the fail-closed
/// error: an in-place rewrite would also change their other names.
fn rewrite_captured_original(
    path: &Path,
    parent: &Path,
    expected: &[u8],
    replacement: &[u8],
    denied: io::Error,
) -> io::Result<FileMutationOutcome> {
    let file = match OpenOptions::new()
        .read(true)
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(FileMutationOutcome::Conflict);
        }
        // Another open handle: keep the fail-closed result instead of waiting.
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => {
            return Err(io::Error::new(
                denied.kind(),
                format!("{denied}; in-place rewrite needs exclusive access: {error}"),
            ));
        }
        Err(error) => return Err(error),
    };
    // The pin closed before this exclusive open: compare again under this handle.
    if !preimage_matches(&file, expected)? {
        return Ok(FileMutationOutcome::Conflict);
    }
    if link_count(&file)? != 1 {
        return Err(denied);
    }
    let mut preimage = tempfile::Builder::new()
        .prefix(".codex-patch-preimage-")
        .tempfile_in(parent)?;
    preimage.write_all(expected)?;
    let retained = recovery_path(parent)?;
    // Capture: the name stays absent until this same object is published back.
    rename_handle(&file, &retained)?;
    let failure = match overwrite(&file, replacement) {
        Ok(()) => match rename_handle(&file, path) {
            Ok(()) => return Ok(FileMutationOutcome::Committed),
            Err(error) => error,
        },
        Err(error) => error,
    };
    restore_captured_original(&file, path, retained, expected, preimage, failure)
}

/// After a failure inside the capture interval, puts the original bytes back
/// into the actual object and republishes it. Whenever the name is not restored,
/// that actual object is the recovery object; the complete preimage copy is kept
/// beside it only when the object itself could not be restored.
fn restore_captured_original(
    file: &File,
    path: &Path,
    retained: PathBuf,
    expected: &[u8],
    preimage: NamedTempFile,
    failure: io::Error,
) -> io::Result<FileMutationOutcome> {
    if let Err(restore) = overwrite(file, expected) {
        let copy = keep_preimage_copy(preimage);
        return Ok(FileMutationOutcome::RecoveryRequired {
            retained_path: retained,
            message: format!(
                "{failure}; restoring the original bytes failed ({restore}); the retained file may hold replacement bytes; the complete original bytes are at {}",
                copy.display()
            ),
        });
    }
    match rename_handle(file, path) {
        // Fully rolled back: the same object holds the same bytes at its name.
        Ok(()) => Err(failure),
        Err(publish) => Ok(FileMutationOutcome::RecoveryRequired {
            retained_path: retained,
            message: format!("{failure}; republishing the restored original failed: {publish}"),
        }),
    }
}

/// Keeps the complete preimage copy even if clearing its temporary attribute fails.
fn keep_preimage_copy(copy: NamedTempFile) -> PathBuf {
    match copy.into_temp_path().keep() {
        Ok(path) => path,
        Err(error) => {
            let mut path = error.path;
            path.disable_cleanup(true);
            path.to_path_buf()
        }
    }
}

/// Writes `bytes` over the whole object through `file`. Growing first allocates
/// the space, so a full volume normally fails before any existing byte changes.
fn overwrite(file: &File, bytes: &[u8]) -> io::Result<()> {
    let length = bytes.len() as u64;
    if file.metadata()?.len() < length {
        file.set_len(length)?;
    }
    let mut written = 0;
    while written < bytes.len() {
        match file.seek_write(&bytes[written..], written as u64) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    file.set_len(length)
}

fn link_count(file: &File) -> io::Result<u32> {
    let handle = file.as_raw_handle() as HANDLE;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` keeps the handle live and `information` is writable for the call.
    if unsafe { GetFileInformationByHandle(handle, &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(information.nNumberOfLinks)
}

/// Reserves an unused same-directory name; publication never replaces it.
fn recovery_path(parent: &Path) -> io::Result<PathBuf> {
    let reservation = tempfile::Builder::new()
        .prefix(".codex-patch-recovery-")
        .tempfile_in(parent)?;
    let retained = reservation.path().to_path_buf();
    reservation.close()?;
    Ok(retained)
}

/// Scanners and other short-lived holders of a mapped section or handle deny
/// publication and cleanup briefly after a write. Retry only those denials,
/// within a bound, before reporting the retained recovery object.
fn retry_transient_denial(mut operation: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    for delay_ms in [10, 40, 100, 250] {
        match operation() {
            Err(error)
                if error.raw_os_error().is_some_and(|code| {
                    code == ERROR_ACCESS_DENIED as i32 || code == ERROR_SHARING_VIOLATION as i32
                }) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            result => return result,
        }
    }
    operation()
}

fn delete_handle(file: &File) -> io::Result<()> {
    let mut info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the owned handle has DELETE access and info has the exact native layout.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfo,
            (&mut info as *mut FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn rename_handle(file: &File, destination: &Path) -> io::Result<()> {
    let name: Vec<u16> = destination.as_os_str().encode_wide().collect();
    // FileName is documented as NUL-terminated; FileNameLength excludes the
    // terminator. Reserve it explicitly instead of relying on rounding slack.
    let bytes = std::mem::offset_of!(FILE_RENAME_INFO, FileName) + (name.len() + 1) * 2;
    let mut buffer = vec![0usize; bytes.div_ceil(size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: pointer-aligned storage includes the variable-length filename;
    // zero initialization sets ReplaceIfExists=false and RootDirectory=null.
    unsafe {
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        if SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileRenameInfo,
            info.cast(),
            bytes as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
