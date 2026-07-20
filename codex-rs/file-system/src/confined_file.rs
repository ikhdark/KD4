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

#[cfg(unix)]
fn open_confined_file_impl(
    root: &Path,
    path: &Path,
    components: &[&std::ffi::OsStr],
) -> io::Result<File> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "confined root must be absolute",
        ));
    }
    let root_path = CString::new("/").expect("filesystem root contains no NUL byte");
    // SAFETY: `root_path` is a valid NUL-terminated path and the returned
    // descriptor is checked before ownership is transferred to `File`.
    let root_descriptor = unsafe { libc::open(root_path.as_ptr(), directory_open_flags()) };
    if root_descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` returned a new owned descriptor.
    let mut directory = unsafe { File::from_raw_fd(root_descriptor) };
    for component in root.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(component) => {
                directory = open_directory_at(&directory, component)?;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "confined root must be an absolute normalized path",
                ));
            }
        }
    }

    for component in &components[..components.len() - 1] {
        directory = open_directory_at(&directory, component)?;
    }

    let file_name = CString::new(components[components.len() - 1].as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "confined file name contains a NUL byte",
        )
    })?;
    // SAFETY: `directory` owns a valid descriptor and `file_name` is a
    // NUL-terminated single path component.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            file_name.as_ptr(),
            libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_RDONLY,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openat` returned a new owned descriptor.
    ensure_regular_file(unsafe { File::from_raw_fd(descriptor) }, path)
}

#[cfg(unix)]
fn open_directory_at(directory: &File, component: &std::ffi::OsStr) -> io::Result<File> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    let component = CString::new(component.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "confined path component contains a NUL byte",
        )
    })?;
    // SAFETY: `directory` owns a valid descriptor and `component` is a
    // NUL-terminated single path component.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            component.as_ptr(),
            directory_open_flags(),
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openat` returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn directory_open_flags() -> libc::c_int {
    libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW | directory_search_access_mode()
}

#[cfg(all(
    unix,
    any(
        target_os = "android",
        target_os = "fuchsia",
        target_os = "linux",
        target_os = "redox"
    )
))]
fn directory_search_access_mode() -> libc::c_int {
    libc::O_PATH
}

#[cfg(all(
    unix,
    any(
        target_vendor = "apple",
        target_os = "aix",
        target_os = "freebsd",
        target_os = "illumos",
        target_os = "netbsd",
        target_os = "solaris"
    )
))]
fn directory_search_access_mode() -> libc::c_int {
    libc::O_SEARCH
}

#[cfg(all(unix, any(target_os = "hurd", target_os = "nto")))]
fn directory_search_access_mode() -> libc::c_int {
    libc::O_EXEC
}

#[cfg(all(
    unix,
    not(any(
        target_os = "android",
        target_os = "fuchsia",
        target_os = "linux",
        target_os = "redox",
        target_vendor = "apple",
        target_os = "aix",
        target_os = "freebsd",
        target_os = "illumos",
        target_os = "netbsd",
        target_os = "solaris",
        target_os = "hurd",
        target_os = "nto"
    ))
))]
fn directory_search_access_mode() -> libc::c_int {
    libc::O_RDONLY
}

#[cfg(windows)]
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

#[cfg(not(any(unix, windows)))]
fn open_confined_file_impl(
    _root: &Path,
    _path: &Path,
    _components: &[&std::ffi::OsStr],
) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "confined file opens are supported only on Unix and Windows",
    ))
}
