//! Persistence layer for the global, append-only *message history* file.
//!
//! The history is stored at `~/.codex/history.jsonl` with **one JSON object per
//! line** so that it can be efficiently appended to and parsed with standard
//! JSON-Lines tooling. Each record has the following schema:
//!
//! ````text
//! {"session_id":"<uuid>","ts":<unix_seconds>,"text":"<message>"}
//! ````
//!
//! Complete records are serialized before taking an exclusive file lock. Writers
//! recover incomplete trailing records and append while holding that lock.
//! Retention publishes a complete replacement; locked handles are checked against
//! the current path before use. Appends do not promise a disk sync per message.

use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Result;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use memchr::memchr_iter;
use serde::Deserialize;
use serde::Serialize;

use std::time::Duration;

use codex_config::types::History;
use codex_config::types::HistoryPersistence;

/// Filename that stores the message history inside `~/.codex`.
const HISTORY_FILENAME: &str = "history.jsonl";
const HISTORY_READ_BUFFER_SIZE: usize = 8192;

/// When history exceeds the hard cap, trim it down to this fraction of `max_bytes`.
const HISTORY_SOFT_CAP_RATIO: f64 = 0.8;

const MAX_RETRIES: usize = 10;
const RETRY_SLEEP: Duration = Duration::from_millis(100);

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub session_id: String,
    pub ts: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryConfig {
    pub codex_home: PathBuf,
    pub persistence: HistoryPersistence,
    pub max_bytes: Option<usize>,
}

impl HistoryConfig {
    pub fn new(codex_home: impl Into<PathBuf>, history: &History) -> Self {
        Self {
            codex_home: codex_home.into(),
            persistence: history.persistence,
            max_bytes: history.max_bytes,
        }
    }
}

fn history_filepath(config: &HistoryConfig) -> PathBuf {
    config.codex_home.join(HISTORY_FILENAME)
}

/// Append a `text` entry associated with `conversation_id` to the history file.
///
/// Uses advisory file locking (`File::try_lock`) with a retry loop to ensure
/// concurrent writes from multiple TUI processes do not interleave. The lock
/// acquisition and write are performed inside `spawn_blocking` so the caller's
/// async runtime is not blocked.
///
/// The entry is silently skipped when `config.history.persistence` is
/// [`HistoryPersistence::None`].
///
/// # Errors
///
/// Returns an I/O error if the history file cannot be opened/created, the
/// system clock is before the Unix epoch, or the exclusive lock cannot be
/// acquired after [`MAX_RETRIES`] attempts.
pub async fn append_entry(
    text: &str,
    conversation_id: impl std::fmt::Display,
    config: &HistoryConfig,
) -> Result<()> {
    match config.persistence {
        HistoryPersistence::SaveAll => {
            // Save everything: proceed.
        }
        HistoryPersistence::None => {
            // No history persistence requested.
            return Ok(());
        }
    }

    // TODO: check `text` for sensitive patterns

    // Resolve `~/.codex/history.jsonl` and ensure the parent directory exists.
    let path = history_filepath(config);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Compute timestamp (seconds since the Unix epoch).
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| std::io::Error::other(format!("system clock before Unix epoch: {e}")))?
        .as_secs();

    // Serialize before taking the exclusive lock.
    let entry = HistoryEntry {
        session_id: conversation_id.to_string(),
        ts,
        text: text.to_string(),
    };
    let mut line = serde_json::to_string(&entry)
        .map_err(|e| std::io::Error::other(format!("failed to serialise history entry: {e}")))?;
    line.push('\n');

    let history_max_bytes = config.max_bytes;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut file = open_locked_history(&path, true)?;
        recover_incomplete_suffix(&mut file)?;
        file.seek(SeekFrom::End(0))?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        enforce_history_limit(&mut file, &path, history_max_bytes, line.len() as u64)
    })
    .await??;
    Ok(())
}

// A writer may have opened the previous file before another writer replaced it.
// Check identity only after locking, then reopen stale handles. Readers use the
// same protocol so their identity and contents describe one coherent snapshot.
fn open_locked_history(path: &Path, exclusive: bool) -> Result<File> {
    for _ in 0..MAX_RETRIES {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(exclusive)
            .create(exclusive)
            .truncate(false);
        // Prompt history must not become readable by other users under a permissive umask.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        let lock = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match lock {
            Ok(()) => {
                if log_identity(&file)? == log_identity(&File::open(path)?)? {
                    return Ok(file);
                }
            }
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(RETRY_SLEEP),
            Err(error) => return Err(error.into()),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        "could not lock the current history file after multiple attempts",
    ))
}

fn recover_incomplete_suffix(file: &mut File) -> Result<()> {
    let mut end = file.metadata()?.len();
    if end == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }
    let mut buffer = [0; HISTORY_READ_BUFFER_SIZE];
    while end > 0 {
        let start = end.saturating_sub(buffer.len() as u64);
        let len = (end - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..len])?;
        if let Some(index) = memchr::memrchr(b'\n', &buffer[..len]) {
            return file.set_len(start + index as u64 + 1);
        }
        end = start;
    }
    file.set_len(0)
}

/// Retain complete records within the soft cap, always keeping the newest entry.
/// Copy only the retained suffix and publish it without truncating the original.
fn enforce_history_limit(
    file: &mut File,
    path: &Path,
    max_bytes: Option<usize>,
    newest_entry_len: u64,
) -> Result<()> {
    let Some(max_bytes) = max_bytes.filter(|limit| *limit > 0) else {
        return Ok(());
    };
    let current_len = file.metadata()?.len();
    if current_len <= max_bytes as u64 {
        return Ok(());
    }
    let target = trim_target_bytes(max_bytes as u64, newest_entry_len);
    let start = current_len.saturating_sub(target);
    if start == 0 {
        return Ok(());
    }
    // Start one byte earlier so a suffix already aligned to a line is retained.
    file.seek(SeekFrom::Start(start - 1))?;
    let mut reader = BufReader::new(file);
    reader.skip_until(b'\n')?;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("history has no parent"))?;
    let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(&mut reader, &mut replacement)?;
    replacement.as_file().sync_all()?;
    // Keep both generations locked across publication. Waiting users of the old
    // generation must reopen; users of the new one wait for publication to finish.
    replacement.as_file().lock()?;
    // Preserve open readers of the old generation on Windows as well as Unix.
    std::fs::rename(replacement.path(), path)?;
    Ok(())
}

fn trim_target_bytes(max_bytes: u64, newest_entry_len: u64) -> u64 {
    let soft_cap_bytes = ((max_bytes as f64) * HISTORY_SOFT_CAP_RATIO)
        .floor()
        .clamp(1.0, max_bytes as f64) as u64;

    soft_cap_bytes.max(newest_entry_len)
}

/// Asynchronously fetch the history file's *identifier* and current entry count.
///
/// The identifier is the Windows file index, stable across ordinary appends.
/// The entry count is derived by counting newline bytes in the file. Returns
/// `(0, 0)` when the file does not exist or its metadata cannot be read. If
/// opening and locking succeeds but scanning fails, returns
/// `(log_id, 0)` so callers can still detect that a history file exists.
pub async fn history_metadata(config: &HistoryConfig) -> (u64, usize) {
    let path = history_filepath(config);
    history_metadata_for_file(&path).await
}

/// Look up a single history entry by file identity and zero-based offset.
///
/// Returns `Some(entry)` when the current history file's identifier (file index on
/// the Windows filesystem) matches `log_id` **and** a valid JSON
/// record exists at `offset`. Returns `None` on any mismatch, I/O error, or
/// parse failure. I/O and parse failures are logged at `warn` level.
///
/// This function is synchronous because it acquires a shared advisory file lock
/// via `File::try_lock_shared`. Callers on an async runtime should wrap it in
/// `spawn_blocking`.
pub fn lookup(log_id: u64, offset: usize, config: &HistoryConfig) -> Option<HistoryEntry> {
    let path = history_filepath(config);
    lookup_history_entry(&path, log_id, offset)
}

async fn history_metadata_for_file(path: &Path) -> (u64, usize) {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(u64, usize)> {
        let mut file = open_locked_history(&path, false)?;
        let log_id = log_identity(&file)?;
        let mut buffer = [0; HISTORY_READ_BUFFER_SIZE];
        let mut count = 0;
        loop {
            match file.read(&mut buffer) {
                Ok(0) => return Ok((log_id, count)),
                Ok(len) => count += memchr_iter(b'\n', &buffer[..len]).count(),
                Err(_) => return Ok((log_id, 0)),
            }
        }
    })
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or((0, 0))
}

fn lookup_history_entry(path: &Path, log_id: u64, offset: usize) -> Option<HistoryEntry> {
    let result = (|| -> Result<Option<HistoryEntry>> {
        let file = open_locked_history(path, false)?;
        if log_id != 0 && log_identity(&file)? != log_id {
            return Ok(None);
        }
        let mut reader = BufReader::new(file);
        for _ in 0..offset {
            if reader.skip_until(b'\n')? == 0 {
                return Ok(None);
            }
        }
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            return Ok(None);
        }
        serde_json::from_slice(&line)
            .map(Some)
            .map_err(std::io::Error::other)
    })();
    match result {
        Ok(entry) => entry,
        Err(error) => {
            tracing::warn!(%error, "failed to look up history entry");
            None
        }
    }
}

// A file index survives appends but changes on replacement, unlike Windows
// creation timestamps, which can be preserved by filesystem name tunneling.
fn log_identity(file: &File) -> Result<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
    // SAFETY: this structure contains only integer and FILETIME fields, for which
    // zero is valid, and is initialized here as writable API output storage.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle remains owned by `file`; `info` is valid writable storage.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow))
}

#[cfg(test)]
mod tests;
