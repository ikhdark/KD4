use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_TRAVERSE;

/// Pin every directory in a native absolute spelling against rename/removal.
/// Opening each component without following reparse points while retaining its
/// parents closes both queued-child and ancestor substitution intervals.
/// Sharing violations fail immediately; this never waits on another writer.
pub fn lock_directory_chain(path: &Path) -> io::Result<Vec<File>> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory guard requires an absolute path",
        ));
    }
    let mut current = PathBuf::new();
    let mut handles = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) => {
                current.push(component);
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "parent traversal is not allowed in a guarded path",
                ));
            }
            _ => current.push(component),
        }
        handles.push(pin_directory(&current)?);
    }
    Ok(handles)
}

/// Re-check directories already pinned by [`lock_directory_chain`] through
/// their retained handles: each must still be a directory, not a reparse point.
/// The retained handles deny rename and delete, so their names still resolve to
/// the same objects; this replaces reopening every component from the root.
pub fn validate_pinned_directories(handles: &[File]) -> io::Result<()> {
    handles.iter().try_for_each(check_pinned_directory)
}

/// Pin only the components of `path` below `base`, whose own chain the caller
/// already holds (from [`lock_directory_chain`]). Each component gets the same
/// no-follow open and checks as [`lock_directory_chain`].
pub fn lock_directory_components_below(base: &Path, path: &Path) -> io::Result<Vec<File>> {
    let rest = path.strip_prefix(base).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "guarded path is outside its pinned base",
        )
    })?;
    let mut current = base.to_path_buf();
    let mut handles = Vec::new();
    for component in rest.components() {
        match component {
            Component::Normal(_) => current.push(component),
            Component::CurDir => continue,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "parent traversal is not allowed in a guarded path",
                ));
            }
        }
        handles.push(pin_directory(&current)?);
    }
    Ok(handles)
}

fn pin_directory(current: &Path) -> io::Result<File> {
    let handle = OpenOptions::new()
        .read(true)
        // Attribute-only handles do not participate in Windows sharing
        // checks. Traverse access pins the directory without requiring
        // permission to enumerate every ancestor's contents.
        .access_mode(FILE_READ_ATTRIBUTES | FILE_TRAVERSE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(current)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot pin directory {} without following links: {error}",
                    current.display()
                ),
            )
        })?;
    check_pinned_directory(&handle)?;
    Ok(handle)
}

fn check_pinned_directory(handle: &File) -> io::Result<()> {
    let metadata = handle.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "guarded directory traversal refuses a reparse point or non-directory",
        ));
    }
    Ok(())
}
