use std::fs::File;
use std::io;
use std::path::Component;
use std::path::Path;

/// Opens a regular file on Windows and verifies its resolved handle path is
/// beneath `root` before returning the opened file.
///
/// Both paths must be canonical absolute paths in the Windows verbatim path
/// representation returned by `std::fs::canonicalize`. Callers should check
/// confinement before calling; this helper checks again using the opened handle.
pub fn open_confined_file(root: &Path, path: &Path) -> io::Result<File> {
    open_confined_file_with_metadata(root, path).map(|(file, _)| file)
}

/// Like [`open_confined_file`], retaining the opened file's validation metadata.
pub fn open_confined_file_with_metadata(
    root: &Path,
    path: &Path,
) -> io::Result<(File, std::fs::Metadata)> {
    let relative = path.strip_prefix(root).map_err(|_| outside_root_error())?;
    let components = relative
        .components()
        .map(|component| match component {
            Component::Normal(component) => Ok(component),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "confined file path must contain only normal relative components",
            )),
        })
        .collect::<io::Result<Vec<_>>>()?;
    if components.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "confined file path must name a file below the root",
        ));
    }

    let _capture = crate::guard_patch_read(path)?;
    open_confined_file_impl(root, path, &components)
}

fn outside_root_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "file resolves outside the confined root",
    )
}

/// Queries the opened directory's child-name policy. Unsupported filesystems
/// return unknown rather than inferring insensitivity from the host platform.
pub fn directory_case_sensitive(path: &Path) -> io::Result<Option<bool>> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FILE_CASE_SENSITIVE_INFO;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FileCaseSensitiveInfo;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
    use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

    let directory = OpenOptions::new()
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(path)?;
    if !directory.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "case policy requires a directory",
        ));
    }
    let mut info = FILE_CASE_SENSITIVE_INFO { Flags: 0 };
    // SAFETY: directory owns the handle and info is writable for the exact
    // size required by FileCaseSensitiveInfo for the duration of the call.
    let success = unsafe {
        GetFileInformationByHandleEx(
            directory.as_raw_handle() as HANDLE,
            FileCaseSensitiveInfo,
            (&mut info as *mut FILE_CASE_SENSITIVE_INFO).cast(),
            std::mem::size_of::<FILE_CASE_SENSITIVE_INFO>() as u32,
        )
    };
    if success == 0 {
        let error = io::Error::last_os_error();
        // INVALID_FUNCTION, NOT_SUPPORTED, INVALID_PARAMETER: older systems
        // and some network filesystems do not implement this information class.
        return match error.raw_os_error() {
            Some(1 | 50 | 87) => Ok(None),
            _ => Err(error),
        };
    }
    // FILE_CS_FLAG_CASE_SENSITIVE_DIR (the only defined flag).
    Ok(Some(info.Flags & 1 != 0))
}

fn ensure_regular_file(file: File, path: &Path) -> io::Result<(File, std::fs::Metadata)> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` is not a regular file", path.display()),
        ));
    }
    Ok((file, metadata))
}

fn open_confined_file_impl(
    root: &Path,
    path: &Path,
    _components: &[&std::ffi::OsStr],
) -> io::Result<(File, std::fs::Metadata)> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .security_qos_flags(SECURITY_IDENTIFICATION);
    let (file, metadata) = ensure_regular_file(options.open(path)?, path)?;
    if !resolved_file_path(&file)?.starts_with(root) {
        return Err(outside_root_error());
    }
    Ok((file, metadata))
}

/// Returns the canonical path of the opened object, not a later lexical lookup.
pub fn resolved_file_path(file: &File) -> io::Result<std::path::PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    let handle = file.as_raw_handle() as HANDLE;
    let mut capacity = 260u32;
    let resolved = loop {
        let mut buffer = vec![0u16; capacity as usize];
        // SAFETY: `file` owns `handle`, and `buffer` is writable for `capacity`
        // UTF-16 code units for the duration of the call.
        let written =
            unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), capacity, 0) };
        if written == 0 {
            return Err(io::Error::last_os_error());
        }
        if written < capacity {
            buffer.truncate(written as usize);
            break std::path::PathBuf::from(OsString::from_wide(&buffer));
        }
        capacity = written.saturating_add(1);
    };
    Ok(resolved)
}
