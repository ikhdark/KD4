//! Commit an already flushed same-directory temporary file without reopening or rewriting its bytes.
#![deny(clippy::print_stdout, clippy::print_stderr)]
use std::io;
use std::path::Path;

#[cfg(windows)]
pub fn persist_synced_file(
    temporary: tempfile::NamedTempFile,
    cache_path: &Path,
    _parent: &Path,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let temporary = temporary.into_temp_path();
    let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = cache_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // The contents were flushed above. Commit the same-directory rename with
    // write-through; flushing a read-only directory handle fails on Windows.
    // SAFETY: both paths are live NUL-terminated strings for this call.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn persist_synced_file(
    temporary: tempfile::NamedTempFile,
    cache_path: &Path,
    parent: &Path,
) -> io::Result<()> {
    temporary.persist(cache_path).map_err(|error| error.error)?;
    std::fs::File::open(parent)?.sync_all()
}
