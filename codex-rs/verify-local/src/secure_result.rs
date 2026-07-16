use crate::model::CommandResultV2;
use rand::TryRngCore;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;
use std::fs;
use std::fs::File;
#[cfg(not(any(unix, windows)))]
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

pub const RESULT_ROOT: &str = ".codex/verify-local/results";

pub fn random_hex_128() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(lower_hex(&bytes))
}

pub fn command_token(command_id: &str) -> String {
    lower_hex(&Sha256::digest(command_id.as_bytes()))
}

pub fn create_invocation_dir(
    repository_root: &Path,
    invocation_id: &str,
    nonce: &str,
) -> Result<PathBuf, String> {
    validate_hex_128(invocation_id, "invocation id")?;
    validate_hex_128(nonce, "invocation nonce")?;
    let root = repository_root.join(RESULT_ROOT);
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    ensure_private_directory(&root)?;
    let path = root.join(format!("{invocation_id}-{nonce}"));
    create_private_dir(&path)?;
    ensure_private_directory(&path)?;
    Ok(path)
}

pub fn write_result_file(
    result_dir: &Path,
    result: &CommandResultV2,
) -> Result<CommandResultV2, String> {
    let directory = ResultDirectory::open(result_dir)?;
    let file_name = result_filename(
        result.command_ordinal,
        &result.command_id,
        &result.invocation_id,
        &result.runner_nonce,
    );
    validate_relative_name(&file_name)?;
    let temporary = format!("{file_name}.tmp");
    require_absent_at(&directory, &file_name, "result destination")?;
    require_absent_at(&directory, &temporary, "temporary result destination")?;
    let payload = serde_json::to_vec(result).map_err(|error| error.to_string())?;
    let mut file = open_new_private_file_at(&directory, &temporary)
        .map_err(|error| format!("create temporary result: {error}"))?;
    file.write_all(&payload)
        .map_err(|error| error.to_string())?;
    file.flush().map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    let temporary_identity =
        file_identity(&file).map_err(|error| format!("inspect temporary result: {error}"))?;
    atomic_rename_no_replace_at(&directory, &temporary, &file_name, &file)
        .map_err(|error| format!("publish result: {error}"))?;
    fsync_dir(&directory).map_err(|error| format!("sync result directory: {error}"))?;
    drop(file);
    let mut reopened = open_existing_private_file_at(&directory, &file_name)
        .map_err(|error| format!("reopen published result: {error}"))?;
    let destination_identity =
        file_identity(&reopened).map_err(|error| format!("inspect published result: {error}"))?;
    if temporary_identity != destination_identity {
        return Err("result file identity changed during publication".to_string());
    }
    let mut bytes = Vec::new();
    reopened
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let parsed = parse_exact_json(&bytes)?;
    verify_result_identity(result, &parsed)?;
    Ok(parsed)
}

#[cfg(test)]
pub fn read_result_file(path: &Path) -> Result<CommandResultV2, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "result path has no parent directory".to_string())?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "result path has no UTF-8 file name".to_string())?;
    validate_relative_name(name)?;
    let directory = ResultDirectory::open(parent)?;
    let mut file = open_existing_private_file_at(&directory, name)?;
    file_identity(&file)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    parse_exact_json(&bytes)
}

pub fn result_filename(
    ordinal: usize,
    command_id: &str,
    invocation_id: &str,
    nonce: &str,
) -> String {
    format!(
        "{ordinal:04}-{}-{invocation_id}-{nonce}.json",
        command_token(command_id)
    )
}

pub fn parse_exact_json<T>(bytes: &[u8]) -> Result<T, String>
where
    T: DeserializeOwned,
{
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = T::deserialize(&mut deserializer).map_err(|error| error.to_string())?;
    deserializer
        .end()
        .map_err(|_| "result file contains trailing non-whitespace bytes".to_string())?;
    Ok(value)
}

pub fn verify_result_identity(
    expected: &CommandResultV2,
    actual: &CommandResultV2,
) -> Result<(), String> {
    if actual.schema_version == expected.schema_version
        && actual.invocation_id == expected.invocation_id
        && actual.command_id == expected.command_id
        && actual.command_ordinal == expected.command_ordinal
        && actual.runner_nonce == expected.runner_nonce
    {
        Ok(())
    } else {
        Err("result readback identity mismatch".to_string())
    }
}

fn validate_hex_128(value: &str, label: &str) -> Result<(), String> {
    if value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(format!(
            "{label} must be 32 lowercase hexadecimal characters"
        ))
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn validate_relative_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        Err("result file name is not a single safe path component".to_string())
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    volume: u64,
    file: u64,
}

struct ResultDirectory {
    handle: File,
    #[cfg(not(any(unix, windows)))]
    path: PathBuf,
}

impl ResultDirectory {
    fn open(path: &Path) -> Result<Self, String> {
        ensure_private_directory(path)?;
        let handle = open_directory_handle(path)?;
        directory_identity(&handle)?;
        Ok(Self {
            handle,
            #[cfg(not(any(unix, windows)))]
            path: path.to_path_buf(),
        })
    }
}

fn require_absent_at(directory: &ResultDirectory, name: &str, label: &str) -> Result<(), String> {
    match exists_at(directory, name)? {
        true => Err(format!("{label} already exists")),
        false => Ok(()),
    }
}

#[cfg(unix)]
fn relative_c_string(name: &str) -> Result<std::ffi::CString, String> {
    validate_relative_name(name)?;
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| "result file name contains a NUL byte".to_string())
}

#[cfg(unix)]
fn open_directory_handle(path: &Path) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| error.to_string())
}

#[cfg(unix)]
fn directory_identity(directory: &File) -> Result<FileIdentity, String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = directory.metadata().map_err(|error| error.to_string())?;
    if !metadata.file_type().is_dir() {
        return Err("result directory handle is not a directory".to_string());
    }
    Ok(FileIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
    })
}

#[cfg(unix)]
fn exists_at(directory: &ResultDirectory, name: &str) -> Result<bool, String> {
    use std::os::fd::AsRawFd;

    let name = relative_c_string(name)?;
    let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
    let status = unsafe {
        libc::fstatat(
            directory.handle.as_raw_fd(),
            name.as_ptr(),
            &mut metadata,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(false)
    } else {
        Err(error.to_string())
    }
}

#[cfg(unix)]
fn open_new_private_file_at(directory: &ResultDirectory, name: &str) -> Result<File, String> {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;

    let name = relative_c_string(name)?;
    let descriptor = unsafe {
        libc::openat(
            directory.handle.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if descriptor == -1 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

#[cfg(unix)]
fn open_existing_private_file_at(directory: &ResultDirectory, name: &str) -> Result<File, String> {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;

    let name = relative_c_string(name)?;
    let descriptor = unsafe {
        libc::openat(
            directory.handle.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor == -1 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn atomic_rename_no_replace_at(
    directory: &ResultDirectory,
    source: &str,
    destination: &str,
    _source_file: &File,
) -> Result<(), String> {
    use std::os::fd::AsRawFd;

    let source = relative_c_string(source)?;
    let destination = relative_c_string(destination)?;
    let status = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            directory.handle.as_raw_fd(),
            source.as_ptr(),
            directory.handle.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if status == -1 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn atomic_rename_no_replace_at(
    directory: &ResultDirectory,
    source: &str,
    destination: &str,
    _source_file: &File,
) -> Result<(), String> {
    use std::os::fd::AsRawFd;

    let source = relative_c_string(source)?;
    let destination = relative_c_string(destination)?;
    let linked = unsafe {
        libc::linkat(
            directory.handle.as_raw_fd(),
            source.as_ptr(),
            directory.handle.as_raw_fd(),
            destination.as_ptr(),
            0,
        )
    };
    if linked == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if unsafe { libc::unlinkat(directory.handle.as_raw_fd(), source.as_ptr(), 0) } == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

#[cfg(windows)]
fn open_directory_handle(path: &Path) -> Result<File, String> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::CreateFileW;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;

    let path = windows_verbatim_path(path)?;
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(unsafe { File::from_raw_handle(handle as _) })
    }
}

#[cfg(windows)]
fn directory_identity(directory: &File) -> Result<FileIdentity, String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;

    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        GetFileInformationByHandle(
            directory.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            &mut information,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err("result directory handle is not a real directory".to_string());
    }
    Ok(FileIdentity {
        volume: u64::from(information.dwVolumeSerialNumber),
        file: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    })
}

#[cfg(windows)]
fn nt_open_relative(
    directory: &ResultDirectory,
    name: &str,
    create_new: bool,
) -> std::io::Result<File> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::FILE_CREATE;
    use windows_sys::Wdk::Storage::FileSystem::FILE_NON_DIRECTORY_FILE;
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_REPARSE_POINT;
    use windows_sys::Wdk::Storage::FileSystem::FILE_SYNCHRONOUS_IO_NONALERT;
    use windows_sys::Wdk::Storage::FileSystem::NtCreateFile;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
    use windows_sys::Win32::Foundation::UNICODE_STRING;
    use windows_sys::Win32::Storage::FileSystem::DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_DATA;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_DATA;
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    validate_relative_name(name)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let mut name = name.encode_utf16().collect::<Vec<_>>();
    let byte_length = name
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| std::io::Error::other("result file name is too long"))?;
    let unicode_name = UNICODE_STRING {
        Length: byte_length,
        MaximumLength: byte_length,
        Buffer: name.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: directory.handle.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
        ObjectName: &unicode_name,
        Attributes: 0x40,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut io_status: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    let mut handle = INVALID_HANDLE_VALUE;
    let desired_access = if create_new {
        DELETE | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES | FILE_READ_ATTRIBUTES | SYNCHRONIZE
    } else {
        FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE
    };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access,
            &attributes,
            &mut io_status,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            if create_new { FILE_CREATE } else { FILE_OPEN },
            FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(std::io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

#[cfg(windows)]
fn exists_at(directory: &ResultDirectory, name: &str) -> Result<bool, String> {
    match nt_open_relative(directory, name, false) {
        Ok(file) => {
            drop(file);
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(windows)]
fn open_new_private_file_at(directory: &ResultDirectory, name: &str) -> Result<File, String> {
    nt_open_relative(directory, name, true).map_err(|error| error.to_string())
}

#[cfg(windows)]
fn open_existing_private_file_at(directory: &ResultDirectory, name: &str) -> Result<File, String> {
    nt_open_relative(directory, name, false).map_err(|error| error.to_string())
}

#[cfg(windows)]
fn atomic_rename_no_replace_at(
    directory: &ResultDirectory,
    source: &str,
    destination: &str,
    source_file: &File,
) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Wdk::Storage::FileSystem::FILE_RENAME_INFORMATION;
    use windows_sys::Wdk::Storage::FileSystem::NtSetInformationFile;
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    validate_relative_name(source)?;
    validate_relative_name(destination)?;
    let source_identity = file_identity(source_file)?;
    let source_readback = open_existing_private_file_at(directory, source)?;
    if source_identity != file_identity(&source_readback)? {
        return Err("temporary result identity changed before publication".to_string());
    }
    drop(source_readback);

    let destination = destination.encode_utf16().collect::<Vec<_>>();
    let name_bytes = destination
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| "result destination name is too long".to_string())?;
    let header_bytes = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    let buffer_size = header_bytes
        .checked_add(name_bytes)
        .ok_or_else(|| "result rename buffer is too large".to_string())?;
    let mut buffer = vec![0_u8; buffer_size];
    let information = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    unsafe {
        (*information).Anonymous.ReplaceIfExists = 0;
        (*information).RootDirectory =
            directory.handle.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        (*information).FileNameLength = u32::try_from(name_bytes)
            .map_err(|_| "result destination name is too long".to_string())?;
        std::ptr::copy_nonoverlapping(
            destination.as_ptr(),
            (*information).FileName.as_mut_ptr(),
            destination.len(),
        );
    }
    let mut io_status: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    let status = unsafe {
        NtSetInformationFile(
            source_file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            &mut io_status,
            buffer.as_ptr().cast(),
            u32::try_from(buffer_size)
                .map_err(|_| "result rename buffer is too large".to_string())?,
            10,
        )
    };
    if status < 0 {
        Err(
            std::io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
                .to_string(),
        )
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir(path).map_err(|error| error.to_string())
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err("result directory is not a real directory".to_string());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| error.to_string())
}

#[cfg(windows)]
fn ensure_private_directory(path: &Path) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_dir()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err("result directory is not a real directory".to_string());
    }
    apply_private_acl(path, true)
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err("result directory is not a real directory".to_string())
    }
}

#[cfg(unix)]
fn file_identity(file: &File) -> Result<FileIdentity, String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("result path is not a regular file".to_string());
    }
    Ok(FileIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
    })
}

#[cfg(windows)]
fn file_identity(file: &File) -> Result<FileIdentity, String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;

    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        GetFileInformationByHandle(
            file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            &mut information,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if information.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
    {
        return Err("result path is not a regular non-reparse file".to_string());
    }
    Ok(FileIdentity {
        volume: u64::from(information.dwVolumeSerialNumber),
        file: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(file: &File) -> Result<FileIdentity, String> {
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("result path is not a regular file".to_string());
    }
    Ok(FileIdentity {
        volume: 0,
        file: metadata.len(),
    })
}

#[cfg(windows)]
fn windows_verbatim_path(path: &Path) -> Result<Vec<u16>, String> {
    use std::os::windows::ffi::OsStrExt;

    let parent = path
        .parent()
        .ok_or_else(|| "result path has no parent directory".to_string())?;
    let name = path
        .file_name()
        .ok_or_else(|| "result path has no file name".to_string())?;
    let absolute = fs::canonicalize(parent)
        .map_err(|error| error.to_string())?
        .join(name);
    let mut wide = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
    const VERBATIM: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const UNC: &[u16] = &[b'\\' as u16, b'\\' as u16];
    if wide.starts_with(VERBATIM) {
        wide.push(0);
        return Ok(wide);
    }
    let mut prefixed = VERBATIM.to_vec();
    if wide.starts_with(UNC) {
        prefixed.extend("UNC\\".encode_utf16());
        prefixed.extend_from_slice(&wide[2..]);
    } else {
        prefixed.append(&mut wide);
    }
    prefixed.push(0);
    Ok(prefixed)
}

#[cfg(windows)]
fn apply_private_acl(path: &Path, directory: bool) -> Result<(), String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
    use windows_sys::Win32::Security::SetFileSecurityW;

    let path = windows_verbatim_path(path)?;
    let sddl = if directory {
        "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;OW)"
    } else {
        "D:P(A;;FA;;;SY)(A;;FA;;;OW)"
    };
    let sddl = sddl
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let status = unsafe {
        SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
    };
    unsafe {
        LocalFree(descriptor);
    }
    if status == 0 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn atomic_rename_no_replace(source: &Path, destination: &Path) -> Result<(), String> {
    fs::hard_link(source, destination).map_err(|error| error.to_string())?;
    fs::remove_file(source).map_err(|error| error.to_string())
}

#[cfg(unix)]
fn fsync_dir(directory: &ResultDirectory) -> Result<(), String> {
    directory
        .handle
        .sync_all()
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
fn fsync_dir(_directory: &ResultDirectory) -> Result<(), String> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn open_directory_handle(path: &Path) -> Result<File, String> {
    File::open(path).map_err(|error| error.to_string())
}

#[cfg(not(any(unix, windows)))]
fn directory_identity(directory: &File) -> Result<FileIdentity, String> {
    let metadata = directory.metadata().map_err(|error| error.to_string())?;
    if !metadata.file_type().is_dir() {
        return Err("result directory handle is not a directory".to_string());
    }
    Ok(FileIdentity {
        volume: 0,
        file: metadata.len(),
    })
}

#[cfg(not(any(unix, windows)))]
fn exists_at(directory: &ResultDirectory, name: &str) -> Result<bool, String> {
    validate_relative_name(name)?;
    match fs::symlink_metadata(directory.path.join(name)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(not(any(unix, windows)))]
fn open_new_private_file_at(directory: &ResultDirectory, name: &str) -> Result<File, String> {
    validate_relative_name(name)?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.path.join(name))
        .map_err(|error| error.to_string())
}

#[cfg(not(any(unix, windows)))]
fn open_existing_private_file_at(directory: &ResultDirectory, name: &str) -> Result<File, String> {
    validate_relative_name(name)?;
    OpenOptions::new()
        .read(true)
        .open(directory.path.join(name))
        .map_err(|error| error.to_string())
}

#[cfg(not(any(unix, windows)))]
fn atomic_rename_no_replace_at(
    directory: &ResultDirectory,
    source: &str,
    destination: &str,
    _source_file: &File,
) -> Result<(), String> {
    validate_relative_name(source)?;
    validate_relative_name(destination)?;
    atomic_rename_no_replace(
        &directory.path.join(source),
        &directory.path.join(destination),
    )
}

#[cfg(not(any(unix, windows)))]
fn fsync_dir(_directory: &ResultDirectory) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
#[path = "secure_result_tests.rs"]
mod secure_result_tests;
