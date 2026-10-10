use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tempfile::NamedTempFile;

pub struct SymlinkWritePaths {
    pub read_path: Option<PathBuf>,
    pub write_path: PathBuf,
}

pub fn resolve_symlink_write_paths(path: &Path) -> io::Result<SymlinkWritePaths> {
    let mut current = AbsolutePathBuf::from_absolute_path(path)
        .map(AbsolutePathBuf::into_path_buf)
        .unwrap_or_else(|_| path.to_path_buf());
    let mut visited = HashSet::new();

    loop {
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(SymlinkWritePaths {
                    read_path: Some(current.clone()),
                    write_path: current,
                });
            }
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_symlink() {
            return Ok(SymlinkWritePaths {
                read_path: Some(current.clone()),
                write_path: current,
            });
        }
        if !visited.insert(current.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("symlink cycle while resolving {}", path.display()),
            ));
        }
        let target = std::fs::read_link(&current)?;
        let next = if target.is_absolute() {
            AbsolutePathBuf::from_absolute_path(&target)
        } else if let Some(parent) = current.parent() {
            Ok(AbsolutePathBuf::resolve_path_against_base(&target, parent))
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("symlink {} has no parent directory", current.display()),
            ));
        };
        current = next?.into_path_buf();
    }
}

pub struct AtomicWriteLock {
    file: File,
}

impl Drop for AtomicWriteLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

pub fn atomic_write_lock_path(write_path: &Path) -> io::Result<PathBuf> {
    let file_name = write_path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} has no file name", write_path.display()),
        )
    })?;
    let mut lock_name = OsString::from(".");
    lock_name.push(file_name);
    lock_name.push(".lock");
    Ok(write_path.with_file_name(lock_name))
}

pub fn acquire_atomic_write_lock(write_path: &Path) -> io::Result<AtomicWriteLock> {
    let file = open_atomic_write_lock(write_path)?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(AtomicWriteLock { file })
}

/// Attempts the same transaction lock without waiting for another writer.
/// `None` means contention; filesystem errors are still reported to the caller.
pub fn try_acquire_atomic_write_lock(write_path: &Path) -> io::Result<Option<AtomicWriteLock>> {
    let file = open_atomic_write_lock(write_path)?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(AtomicWriteLock { file })),
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Ok(None),
        Err(error) => Err(error),
    }
}

fn open_atomic_write_lock(write_path: &Path) -> io::Result<File> {
    let lock_path = atomic_write_lock_path(write_path)?;
    if let Some(parent) = lock_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
}

pub fn write_atomically(write_path: &Path, contents: &str) -> io::Result<()> {
    write_bytes_atomically(write_path, contents.as_bytes())
}

/// Replaces the destination after synchronizing a same-directory temporary file.
/// Parent-directory synchronization is best-effort on Windows. An error after
/// replacement does not imply that the old contents remain installed.
pub fn write_bytes_atomically(write_path: &Path, contents: &[u8]) -> io::Result<()> {
    write_bytes_atomically_impl(write_path, contents, true)
}

/// Same-directory replacement without durability barriers or parent creation.
/// Preserves existing permissions. Sharing/access failures leave the original
/// intact; never fall back to truncating an existing file.
/// The caller resolves symlinks first. Replacement changes only the addressed
/// directory entry: other hardlinks deliberately retain the original bytes.
pub fn write_bytes_atomically_without_sync(write_path: &Path, contents: &[u8]) -> io::Result<()> {
    write_bytes_atomically_impl(write_path, contents, false)
}

fn write_bytes_atomically_impl(write_path: &Path, contents: &[u8], sync: bool) -> io::Result<()> {
    stage_and_replace(write_path, sync, |temporary| temporary.write_all(contents))
}

fn stage_and_replace(
    write_path: &Path,
    sync: bool,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = write_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} has no parent directory", write_path.display()),
        )
    })?;
    if sync {
        std::fs::create_dir_all(parent)?;
    }
    let permissions = if sync {
        None
    } else {
        match std::fs::metadata(write_path) {
            Ok(metadata) => {
                if metadata.permissions().readonly() {
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "destination is read-only"));
                }
                Some(metadata.permissions())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        }
    };
    let mut temporary = NamedTempFile::new_in(parent)?;
    if permissions.is_some() {
        copy_file_security(write_path, temporary.path())?;
    }
    write(temporary.as_file_mut())?;
    temporary.flush()?;
    if let Some(permissions) = permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    if sync {
        temporary.as_file().sync_all()?;
    }
    // Clear Windows temporary attributes, then retain cleanup on rename failure.
    let (_file, path) = temporary.keep().map_err(|error| error.error)?;
    let path = tempfile::TempPath::try_from_path(path)?;
    // std's Windows rename supports replacing a file held by an open reader.
    std::fs::rename(&path, write_path)?;
    if !sync {
        return Ok(());
    }
    sync_parent_directory(parent).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "{} was replaced, but synchronizing its parent directory failed: {error}",
                write_path.display()
            ),
        )
    })?;
    Ok(())
}

// std::fs::Permissions carries only the read-only flag on Windows. Preserve
// ownership and the DACL too, before any replacement can make the new file live.
// Inability to read or install the descriptor is a failure, not permission to
// replace a protected file with a differently secured temporary file.
fn copy_file_security(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::GROUP_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::GetFileSecurityW;
    use windows_sys::Win32::Security::GetSecurityDescriptorControl;
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::SE_DACL_PROTECTED;
    use windows_sys::Win32::Security::SetFileSecurityW;
    use windows_sys::Win32::Security::UNPROTECTED_DACL_SECURITY_INFORMATION;

    let source: Vec<u16> = source.as_os_str().encode_wide().chain([0]).collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().chain([0]).collect();
    let information =
        OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut needed = 0;
    // SAFETY: both paths are NUL-terminated and the size output is writable.
    let result = unsafe {
        GetFileSecurityW(
            source.as_ptr(),
            information,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if result == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(error);
        }
    }
    if needed == 0 {
        return Err(io::Error::other("empty destination security descriptor"));
    }
    // A self-relative security descriptor requires DWORD alignment.
    let mut descriptor = vec![0_u32; (needed as usize).div_ceil(size_of::<u32>())];
    // SAFETY: the buffer is aligned, live, and at least `needed` bytes long.
    if unsafe {
        GetFileSecurityW(
            source.as_ptr(),
            information,
            descriptor.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: GetFileSecurityW initialized the descriptor and outputs are valid.
    if unsafe {
        GetSecurityDescriptorControl(
            descriptor.as_mut_ptr().cast(),
            &mut control,
            &mut revision,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let protection = if control & SE_DACL_PROTECTED != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    // SAFETY: the destination path and initialized descriptor remain live.
    if unsafe {
        SetFileSecurityW(
            destination.as_ptr(),
            information | protection,
            descriptor.as_mut_ptr().cast(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let result = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(parent)
        .and_then(|directory| directory.sync_all());
    match result {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::Unsupported
                    | io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonblocking_transaction_lock_observes_contention_and_releases_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("config.toml");
        let first = acquire_atomic_write_lock(&destination).unwrap();
        assert!(try_acquire_atomic_write_lock(&destination).unwrap().is_none());
        drop(first);
        let second = try_acquire_atomic_write_lock(&destination).unwrap().expect("released lock");
        assert!(try_acquire_atomic_write_lock(&destination).unwrap().is_none());
        drop(second);
        assert!(try_acquire_atomic_write_lock(&destination).unwrap().is_some());
        assert!(try_acquire_atomic_write_lock(Path::new("")).is_err());
    }

    #[test]
    fn partial_staged_write_failure_preserves_original() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("target");
        std::fs::write(&destination, b"entire old contents").unwrap();
        let error = stage_and_replace(&destination, false, |file| {
            file.write_all(b"partial new contents")?;
            Err(io::Error::other("injected mid-write failure"))
        }).unwrap_err();
        assert!(error.to_string().contains("injected mid-write"));
        assert_eq!(std::fs::read(&destination).unwrap(), b"entire old contents");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
