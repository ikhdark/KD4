use std::io;
use std::path::Path;

pub(crate) async fn open(path: &Path) -> io::Result<tokio::fs::File> {
    let (file, _) = open_with_metadata(path).await?;
    Ok(file)
}

pub(crate) async fn open_with_metadata(
    path: &Path,
) -> io::Result<(tokio::fs::File, std::fs::Metadata)> {
    let path = path.to_path_buf();
    let (file, metadata) = tokio::task::spawn_blocking(move || open_sync(&path))
        .await
        .map_err(io::Error::other)??;
    Ok((tokio::fs::File::from_std(file), metadata))
}

/// Validate the opened object, retaining metadata for the caller's initial check.
pub(crate) fn open_sync(path: &Path) -> io::Result<(std::fs::File, std::fs::Metadata)> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    configure_open(&mut options);

    let file = options.open(path)?;
    if !is_disk_file(&file) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` is not a file", path.display()),
        ));
    }
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` is not a file", path.display()),
        ));
    }
    Ok((file, metadata))
}

fn configure_open(options: &mut std::fs::OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

    options.security_qos_flags(SECURITY_IDENTIFICATION);
}

fn is_disk_file(file: &std::fs::File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FILE_TYPE_DISK;
    use windows_sys::Win32::Storage::FileSystem::GetFileType;

    // SAFETY: `file` owns this handle for the duration of the call.
    unsafe { GetFileType(file.as_raw_handle() as HANDLE) == FILE_TYPE_DISK }
}
