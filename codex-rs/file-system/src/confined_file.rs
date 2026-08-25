use std::fs::File;
use std::io;
use std::path::Component;
use std::path::Path;

/// Opens a regular file through a path that is already resolved beneath
/// `root`, without following a concurrently introduced Unix symlink.
///
/// Both paths must be absolute and normalized. Callers that accept user paths
/// should canonicalize them and check confinement before calling this helper.
pub fn open_confined_file(root: &Path, path: &Path) -> io::Result<File> {
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

    open_confined_file_impl(root, path, &components)
}

fn outside_root_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "file resolves outside the confined root",
    )
}

fn ensure_regular_file(file: File, path: &Path) -> io::Result<File> {
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` is not a regular file", path.display()),
        ));
    }
    Ok(file)
}

fn open_confined_file_impl(
    root: &Path,
    path: &Path,
    _components: &[&std::ffi::OsStr],
) -> io::Result<File> {
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .security_qos_flags(SECURITY_IDENTIFICATION);
    let file = ensure_regular_file(options.open(path)?, path)?;
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
    if !resolved.starts_with(root) {
        return Err(outside_root_error());
    }
    Ok(file)
}
