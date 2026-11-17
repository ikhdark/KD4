use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::os::windows::fs::MetadataExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::path::Prefix;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_DATA;
use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

fn invalid_output(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason)
}

/// A final-output destination bound before sandboxed tools run. Ordinary files
/// are opened without following links, held without delete sharing so they
/// cannot be renamed away, and truncated in place only when written.
pub struct BoundOutput {
    file: File,
    /// Native path of a bound regular file; `None` for a stream or device.
    path: Option<PathBuf>,
}

impl BoundOutput {
    pub fn open(path: &Path) -> io::Result<Self> {
        // Check raw syntax before absolute() discards directory markers.
        let bytes = path.as_os_str().as_encoded_bytes();
        let leaf = bytes.rsplit(|b| *b == b'/' || *b == b'\\').next();
        if matches!(leaf, None | Some(b"" | b"." | b"..")) {
            return Err(invalid_output("output must name a file"));
        }
        if bytes.eq_ignore_ascii_case(b"NUL") {
            return Ok(Self::stream(open_null_output()?));
        }
        let path = std::path::absolute(path)?;
        if let Some(Component::Prefix(prefix)) = path.components().next()
            && let Prefix::DeviceNS(device) = prefix.kind()
        {
            return open_device(device, &path).map(Self::stream);
        }
        let file = open_output_file(&path)?;
        // Fail before the turn starts if this filesystem cannot verify the output.
        verify_output_path(&file, &path)?;
        Ok(Self {
            file,
            path: Some(path),
        })
    }

    fn stream(file: File) -> Self {
        Self { file, path: None }
    }

    /// Writes through the handle bound at open. A bound file is truncated only
    /// after its pathname is confirmed to still name the same single-link file.
    pub fn write(mut self, contents: &[u8]) -> io::Result<()> {
        if let Some(path) = &self.path {
            verify_output_path(&self.file, path)?;
            validate_bound_file(&self.file)?;
            self.file.set_len(0)?;
        }
        self.file.write_all(contents)?;
        // A caller may reopen the pathname after a successful exit. Detect a visible swap.
        if let Some(path) = &self.path {
            verify_output_path(&self.file, path)?;
        }
        Ok(())
    }
}

fn open_null_output() -> io::Result<File> {
    // Use the kernel null device directly so no user-defined DOS alias is followed.
    OpenOptions::new()
        .write(true)
        .open(r"\\?\GLOBALROOT\Device\Null")
}

/// Streams cannot be substituted by a workspace reparse point. Keep the
/// console and named pipes usable; other device-namespace spellings, including
/// `\\.\C:\...`, would bypass the regular-file checks and are refused.
fn open_device(device: &std::ffi::OsStr, path: &Path) -> io::Result<File> {
    let only_device = path.components().count() == 1;
    let device = device.to_string_lossy();
    if only_device && device.eq_ignore_ascii_case("NUL") {
        return open_null_output();
    }
    let console = only_device
        && (device.eq_ignore_ascii_case("CON") || device.eq_ignore_ascii_case("CONOUT$"));
    if !console && !device.eq_ignore_ascii_case("pipe") {
        return Err(invalid_output(
            "output device is not supported; use NUL, CON, a named pipe or a file path",
        ));
    }
    OpenOptions::new()
        .write(true)
        // A pipe server must not impersonate this process.
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(path)
}

fn output_parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| invalid_output("output must name a file"))
}

fn open_output_file(path: &Path) -> io::Result<File> {
    // Pin the no-follow walk only while the leaf opens; the leaf handle then
    // denies delete sharing, so the bound file cannot be renamed or removed.
    let _parents = crate::lock_directory_chain(output_parent(path)?)?;
    let file = OpenOptions::new()
        // Create a missing file, but never truncate before the final write.
        .write(true)
        .create(true)
        .access_mode(FILE_WRITE_DATA | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(path)?;
    validate_bound_file(&file)?;
    Ok(file)
}

fn validate_bound_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid_output("output must be a regular file, not a link"));
    }
    if file_information(file)?.nNumberOfLinks > 1 {
        return Err(invalid_output("output has multiple hard links"));
    }
    Ok(())
}

/// Confirms the pathname still resolves, without following any reparse
/// component, to the file bound at open.
fn verify_output_path(file: &File, path: &Path) -> io::Result<()> {
    let _parents = crate::lock_directory_chain(output_parent(path)?)?;
    let current = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(path)?;
    if file_identity(file)? != file_identity(&current)? {
        return Err(invalid_output("output path was replaced"));
    }
    Ok(())
}

fn file_information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the file owns the handle and a successful call initializes info.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, info.as_mut_ptr()) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call above succeeded.
    Ok(unsafe { info.assume_init() })
}

/// Full 128-bit file ID (ReFS needs it). Filesystems without FileIdInfo, such
/// as FAT, fall back to the 64-bit index; an all-zero ID cannot identify a file.
fn file_identity(file: &File) -> io::Result<(u64, [u8; 16])> {
    let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
    // SAFETY: the open handle is valid and FileIdInfo fills this exact structure on success.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    let (volume, id) = if success != 0 {
        // SAFETY: the call above succeeded.
        let info = unsafe { info.assume_init() };
        (info.VolumeSerialNumber, info.FileId.Identifier)
    } else {
        let info = file_information(file)?;
        let mut id = [0u8; 16];
        id[..4].copy_from_slice(&info.nFileIndexLow.to_le_bytes());
        id[4..8].copy_from_slice(&info.nFileIndexHigh.to_le_bytes());
        (u64::from(info.dwVolumeSerialNumber), id)
    };
    if id == [0; 16] {
        return Err(invalid_output(
            "filesystem did not provide an output file ID",
        ));
    }
    Ok((volume, id))
}
