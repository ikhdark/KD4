//! Path normalization, symlink resolution, and atomic writes shared across Codex crates.

pub(crate) mod env;
pub use env::is_wsl;

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

pub fn normalize_for_path_comparison(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    let canonical = path.as_ref().canonicalize()?;
    Ok(normalize_for_wsl(canonical))
}

/// Compare paths after applying Codex's filesystem normalization.
///
/// If either path cannot be normalized, this falls back to direct path equality.
pub fn paths_match_after_normalization(left: impl AsRef<Path>, right: impl AsRef<Path>) -> bool {
    if let (Ok(left), Ok(right)) = (
        normalize_for_path_comparison(left.as_ref()),
        normalize_for_path_comparison(right.as_ref()),
    ) {
        return left == right;
    }
    left.as_ref() == right.as_ref()
}

pub fn normalize_for_native_workdir(path: impl AsRef<Path>) -> PathBuf {
    normalize_for_native_workdir_with_flag(path.as_ref().to_path_buf(), cfg!(windows))
}

pub struct SymlinkWritePaths {
    pub read_path: Option<PathBuf>,
    pub write_path: PathBuf,
}

/// Resolve the final filesystem target for `path` while retaining a safe write path.
///
/// This follows symlink chains (including relative symlink targets) until it reaches a
/// non-symlink path. If the chain cycles or any metadata/link resolution fails, it
/// returns `read_path: None` and uses the original absolute path as `write_path`.
/// There is no fixed max-resolution count; cycles are detected via a visited set.
pub fn resolve_symlink_write_paths(path: &Path) -> io::Result<SymlinkWritePaths> {
    let root = AbsolutePathBuf::from_absolute_path(path)
        .map(AbsolutePathBuf::into_path_buf)
        .unwrap_or_else(|_| path.to_path_buf());
    let mut current = root.clone();
    let mut visited = HashSet::new();

    // Follow symlink chains while guarding against cycles.
    loop {
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(SymlinkWritePaths {
                    read_path: Some(current.clone()),
                    write_path: current,
                });
            }
            Err(_) => {
                return Ok(SymlinkWritePaths {
                    read_path: None,
                    write_path: root,
                });
            }
        };

        if !meta.file_type().is_symlink() {
            return Ok(SymlinkWritePaths {
                read_path: Some(current.clone()),
                write_path: current,
            });
        }

        // If we've already seen this path, the chain cycles.
        if !visited.insert(current.clone()) {
            return Ok(SymlinkWritePaths {
                read_path: None,
                write_path: root,
            });
        }

        let target = match std::fs::read_link(&current) {
            Ok(target) => target,
            Err(_) => {
                return Ok(SymlinkWritePaths {
                    read_path: None,
                    write_path: root,
                });
            }
        };

        let next = if target.is_absolute() {
            AbsolutePathBuf::from_absolute_path(&target)
        } else if let Some(parent) = current.parent() {
            Ok(AbsolutePathBuf::resolve_path_against_base(&target, parent))
        } else {
            return Ok(SymlinkWritePaths {
                read_path: None,
                write_path: root,
            });
        };

        let next = match next {
            Ok(path) => path.into_path_buf(),
            Err(_) => {
                return Ok(SymlinkWritePaths {
                    read_path: None,
                    write_path: root,
                });
            }
        };

        current = next;
    }
}

/// An advisory, cross-process lock for a single persistence target.
///
/// All writers of a shared file must acquire this lock before re-reading and
/// mutating that file. The lock is released when the guard is dropped.
pub struct AtomicWriteLock {
    file: File,
}

impl Drop for AtomicWriteLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// Return the canonical lock-file path shared by writers of `write_path`.
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

/// Acquire the canonical advisory lock for `write_path`.
pub fn acquire_atomic_write_lock(write_path: &Path) -> io::Result<AtomicWriteLock> {
    let lock_path = atomic_write_lock_path(write_path)?;
    if let Some(parent) = lock_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(AtomicWriteLock { file })
}

/// Durably replace `write_path` with `contents`.
///
/// `NamedTempFile` creates the temporary file with create-new semantics in the
/// target directory. Persisting it is an atomic same-filesystem replacement;
/// syncing both the file and its parent makes the replacement durable across a
/// crash. Callers that perform read-modify-write must hold
/// [`acquire_atomic_write_lock`] across the read, validation, and this write.
pub fn write_atomically(write_path: &Path, contents: &str) -> io::Result<()> {
    write_bytes_atomically(write_path, contents.as_bytes())
}

pub fn write_bytes_atomically(write_path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = write_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} has no parent directory", write_path.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(contents)?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    tmp.persist(write_path).map_err(|error| error.error)?;
    sync_parent_directory(parent)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    // FILE_FLAG_BACKUP_SEMANTICS permits opening a directory handle.
    // Some Windows filesystems still reject flushing directory handles; the
    // replacement file itself has already been synced in that case.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let result = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(parent)
        .and_then(|directory| directory.sync_all());
    match result {
        Ok(()) => Ok(()),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::Unsupported
                    | io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(())
        }
        Err(err) => Err(err),
    }
}

#[cfg(not(any(unix, windows)))]
fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

fn normalize_for_wsl(path: PathBuf) -> PathBuf {
    normalize_for_wsl_with_flag(path, env::is_wsl())
}

fn normalize_for_native_workdir_with_flag(path: PathBuf, is_windows: bool) -> PathBuf {
    if is_windows {
        dunce::simplified(&path).to_path_buf()
    } else {
        path
    }
}

fn normalize_for_wsl_with_flag(path: PathBuf, is_wsl: bool) -> PathBuf {
    if !is_wsl {
        return path;
    }

    if !is_wsl_case_insensitive_path(&path) {
        return path;
    }

    lower_ascii_path(path)
}

fn is_wsl_case_insensitive_path(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        use std::path::Component;

        let mut components = path.components();
        let Some(Component::RootDir) = components.next() else {
            return false;
        };
        let Some(Component::Normal(mnt)) = components.next() else {
            return false;
        };
        if !ascii_eq_ignore_case(mnt.as_bytes(), b"mnt") {
            return false;
        }
        let Some(Component::Normal(drive)) = components.next() else {
            return false;
        };
        let drive_bytes = drive.as_bytes();
        drive_bytes.len() == 1 && drive_bytes[0].is_ascii_alphabetic()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

#[cfg(target_os = "linux")]
fn ascii_eq_ignore_case(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(lhs, rhs)| lhs.to_ascii_lowercase() == *rhs)
}

#[cfg(target_os = "linux")]
fn lower_ascii_path(path: PathBuf) -> PathBuf {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::ffi::OsStringExt;

    // WSL mounts Windows drives under /mnt/<drive>, which are case-insensitive.
    let bytes = path.as_os_str().as_bytes();
    let mut lowered = Vec::with_capacity(bytes.len());
    for byte in bytes {
        lowered.push(byte.to_ascii_lowercase());
    }
    PathBuf::from(OsString::from_vec(lowered))
}

#[cfg(not(target_os = "linux"))]
fn lower_ascii_path(path: PathBuf) -> PathBuf {
    path
}

#[cfg(test)]
#[path = "path_utils_tests.rs"]
mod tests;
