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
    let path = root.join(format!("{invocation_id}-{nonce}"));
    create_private_dir(&path)?;
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
    if destination.exists() {
        return Err("result destination already exists".to_string());
    }
    let temporary = result_dir.join(format!("{file_name}.tmp"));
    if temporary.exists() {
        return Err("temporary result destination already exists".to_string());
    }
    let payload = serde_json::to_vec(result).map_err(|error| error.to_string())?;
    let mut file = open_new_private_file(&temporary)?;
    file.write_all(&payload)
        .map_err(|error| error.to_string())?;
    file.flush().map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    drop(file);
    ensure_regular_file(&temporary)?;
    fs::rename(&temporary, &destination).map_err(|error| error.to_string())?;
    fsync_dir(result_dir)?;
    let parsed = read_result_file(&destination)?;
    verify_result_identity(result, &parsed)?;
    Ok(parsed)
}

pub fn read_result_file(path: &Path) -> Result<CommandResultV2, String> {
    ensure_regular_file(path)?;
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|error| error.to_string())?
        .read_to_end(&mut bytes)
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
fn open_new_private_file(path: &Path) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn open_new_private_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| error.to_string())
}

fn ensure_regular_file(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_file() {
        Ok(())
    } else {
        Err("result path is not a regular file".to_string())
    }
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
