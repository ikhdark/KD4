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
    let root = AbsolutePathBuf::from_absolute_path(path)
        .map(AbsolutePathBuf::into_path_buf)
        .unwrap_or_else(|_| path.to_path_buf());
    let mut current = root.clone();
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
            Err(_) => {
                return Ok(SymlinkWritePaths {
                    read_path: None,
                    write_path: root,
                });
            }
        };
        if !metadata.file_type().is_symlink() {
            return Ok(SymlinkWritePaths {
                read_path: Some(current.clone()),
                write_path: current,
            });
        }
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
        current = match next {
            Ok(path) => path.into_path_buf(),
            Err(_) => {
                return Ok(SymlinkWritePaths {
                    read_path: None,
                    write_path: root,
                });
            }
        };
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
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(write_path).map_err(|error| error.error)?;
    sync_parent_directory(parent)?;
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
