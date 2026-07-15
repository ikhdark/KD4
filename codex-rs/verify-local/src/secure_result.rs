use crate::model::CommandResultV2;
use rand::TryRngCore;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;
use std::fs;
use std::fs::File;
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
    let file_name = result_filename(
        result.command_ordinal,
        &result.command_id,
        &result.invocation_id,
        &result.runner_nonce,
    );
    let destination = result_dir.join(&file_name);
    require_absent(&destination, "result destination")?;
    let temporary = result_dir.join(format!("{file_name}.tmp"));
    require_absent(&temporary, "temporary result destination")?;
    let payload = serde_json::to_vec(result).map_err(|error| error.to_string())?;
    let mut file = open_new_private_file(&temporary)
        .map_err(|error| format!("create temporary result: {error}"))?;
    file.write_all(&payload)
        .map_err(|error| error.to_string())?;
    file.flush().map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    let temporary_identity =
        file_identity(&file).map_err(|error| format!("inspect temporary result: {error}"))?;
    drop(file);
    atomic_rename_no_replace(&temporary, &destination)
        .map_err(|error| format!("publish result: {error}"))?;
    fsync_dir(result_dir).map_err(|error| format!("sync result directory: {error}"))?;
    let mut reopened = open_existing_private_file(&destination)
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
    let mut file = open_existing_private_file(path)?;
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

fn require_absent(path: &Path, label: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(format!("{label} already exists")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    volume: u64,
    file: u64,
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
    Ok(())
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
fn open_new_private_file(path: &Path) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
fn open_new_private_file(path: &Path) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| error.to_string())
}

#[cfg(not(any(unix, windows)))]
fn open_new_private_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| error.to_string())
}

#[cfg(unix)]
fn open_existing_private_file(path: &Path) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
fn open_existing_private_file(path: &Path) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| error.to_string())
}

#[cfg(not(any(unix, windows)))]
fn open_existing_private_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|error| error.to_string())
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

#[cfg(unix)]
fn atomic_rename_no_replace(source: &Path, destination: &Path) -> Result<(), String> {
    fs::hard_link(source, destination).map_err(|error| error.to_string())?;
    if let Err(error) = fs::remove_file(source) {
        let _ = fs::remove_file(destination);
        return Err(error.to_string());
    }
    Ok(())
}

#[cfg(windows)]
fn atomic_rename_no_replace(source: &Path, destination: &Path) -> Result<(), String> {
    fs::hard_link(source, destination).map_err(|error| error.to_string())?;
    if let Err(error) = fs::remove_file(source) {
        let _ = fs::remove_file(destination);
        return Err(error.to_string());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn atomic_rename_no_replace(source: &Path, destination: &Path) -> Result<(), String> {
    fs::hard_link(source, destination).map_err(|error| error.to_string())?;
    fs::remove_file(source).map_err(|error| error.to_string())
}

#[cfg(unix)]
fn fsync_dir(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn fsync_dir(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
#[path = "secure_result_tests.rs"]
mod secure_result_tests;
