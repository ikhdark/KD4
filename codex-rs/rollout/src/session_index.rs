use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionMetaLine;
use serde::Deserialize;
use serde::Serialize;

const SESSION_INDEX_FILE: &str = "session_index.jsonl";
const SESSION_INDEX_LOCK_FILE: &str = "session_index.jsonl.lock";
const READ_CHUNK_SIZE: usize = 8192;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionIndexEntry {
    pub id: ThreadId,
    pub thread_name: String,
    pub updated_at: String,
}

/// Append a thread name update to the session index.
/// Name updates are append-only; the most recent entry wins when resolving names or ids.
pub async fn append_thread_name(
    codex_home: &Path,
    thread_id: ThreadId,
    name: &str,
) -> std::io::Result<()> {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let updated_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());
    let entry = SessionIndexEntry {
        id: thread_id,
        thread_name: name.to_string(),
        updated_at,
    };
    append_session_index_entry(codex_home, &entry).await
}

/// Append a raw session index entry to `session_index.jsonl`.
/// Consumers scan from the end to find the newest match.
pub async fn append_session_index_entry(
    codex_home: &Path,
    entry: &SessionIndexEntry,
) -> std::io::Result<()> {
    let codex_home = codex_home.to_path_buf();
    let entry = entry.clone();
    tokio::task::spawn_blocking(move || append_session_index_entry_blocking(&codex_home, &entry))
        .await
        .map_err(std::io::Error::other)?
}

fn append_session_index_entry_blocking(
    codex_home: &Path,
    entry: &SessionIndexEntry,
) -> std::io::Result<()> {
    with_session_index_lock(codex_home, || {
        let path = session_index_path(codex_home);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)?;
        let mut line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
        line.push('\n');
        if file.metadata()?.len() > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut tail = [0];
            file.read_exact(&mut tail)?;
            if tail[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }
        file.write_all(line.as_bytes())?;
        file.flush()
    })
}

fn with_session_index_lock<T>(
    codex_home: &Path,
    operation: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let lock_path = session_index_lock_path(codex_home);
    let lock_file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    lock_file.lock()?;
    operation()
}

fn remove_thread_name_entries_blocking(
    codex_home: &Path,
    thread_ids: &HashSet<ThreadId>,
) -> std::io::Result<()> {
    with_session_index_lock(codex_home, || {
        let path = session_index_path(codex_home);
        // Bytes, not text: an append torn inside a multi-byte character must be retained like
        // any other malformed line instead of failing this and every later removal.
        let contents = match std::fs::read(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        };
        let mut removed = false;
        let mut remaining = Vec::with_capacity(contents.len());
        for line in contents.split_inclusive(|byte| *byte == b'\n') {
            let should_remove = std::str::from_utf8(line)
                .ok()
                .and_then(|line| serde_json::from_str::<SessionIndexEntry>(line.trim()).ok())
                .is_some_and(|entry| thread_ids.contains(&entry.id));
            if should_remove {
                removed = true;
            } else {
                remaining.extend_from_slice(line);
                if !line.ends_with(b"\n") {
                    remaining.push(b'\n');
                }
            }
        }
        if !removed {
            return Ok(());
        }
        let mut temp_file = tempfile::NamedTempFile::new_in(codex_home)?;
        temp_file.write_all(&remaining)?;
        temp_file.as_file().sync_all()?;
        temp_file.persist(path).map_err(|error| error.error)?;
        Ok(())
    })
}

fn session_index_lock_path(codex_home: &Path) -> PathBuf {
    codex_home.join(SESSION_INDEX_LOCK_FILE)
}

/// Remove all recorded names for a thread from the session index.
pub async fn remove_thread_name_entries(
    codex_home: &Path,
    thread_id: ThreadId,
) -> std::io::Result<()> {
    remove_threads_name_entries(codex_home, &[thread_id]).await
}

/// Remove all recorded names for a set of threads in one atomic index rewrite.
pub async fn remove_threads_name_entries(
    codex_home: &Path,
    thread_ids: &[ThreadId],
) -> std::io::Result<()> {
    if thread_ids.is_empty() {
        return Ok(());
    }
    let codex_home = codex_home.to_path_buf();
    let thread_ids = thread_ids.iter().copied().collect::<HashSet<_>>();
    tokio::task::spawn_blocking(move || remove_thread_name_entries_blocking(&codex_home, &thread_ids))
        .await
        .map_err(std::io::Error::other)?
}

/// Find the latest thread name for a thread id, if any.
pub async fn find_thread_name_by_id(
    codex_home: &Path,
    thread_id: &ThreadId,
) -> std::io::Result<Option<String>> {
    let path = session_index_path(codex_home);
    let id = *thread_id;
    let entry = tokio::task::spawn_blocking(move || match scan_index_from_end_by_id(&path, &id) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        result => result,
    })
    .await
    .map_err(std::io::Error::other)??;
    Ok(entry.map(|entry| entry.thread_name))
}

/// Find the latest thread names for a batch of thread ids.
pub async fn find_thread_names_by_ids(
    codex_home: &Path,
    thread_ids: &HashSet<ThreadId>,
) -> std::io::Result<HashMap<ThreadId, String>> {
    if thread_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let path = session_index_path(codex_home);
    let thread_ids = thread_ids.clone();
    tokio::task::spawn_blocking(move || cached_thread_names_by_ids(&path, &thread_ids))
        .await
        .map_err(std::io::Error::other)?
}

/// Locate a recorded thread rollout and read its session metadata by thread name.
/// Returns the newest indexed name that still has a readable rollout header.
pub async fn find_thread_meta_by_name_str(
    codex_home: &Path,
    name: &str,
    state_db_ctx: Option<&codex_state::StateRuntime>,
) -> std::io::Result<Option<(PathBuf, SessionMetaLine)>> {
    if name.trim().is_empty() {
        return Ok(None);
    }
    let path = session_index_path(codex_home);
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let name = name.to_string();
    // Stream matching ids newest-first instead of stopping at the first name hit: the newest entry
    // may point at a thread whose rollout was never materialized.
    let scan = tokio::task::spawn_blocking(move || {
        match stream_thread_ids_from_end_by_name(&path, &name, tx) {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            result => result,
        }
    });

    while let Some(thread_id) = rx.recv().await {
        // Keep walking until a matching id resolves to a loadable rollout so an unsaved or partial
        // rename cannot shadow an older persisted session with the same name.
        if let Some((path, header)) = super::list::find_thread_path_and_meta_by_id_str(
            codex_home,
            &thread_id.to_string(),
            state_db_ctx,
        )
        .await?
        {
            // A successfully verified header is one request-local observation.
            // Keep the legacy retry only for recoverable header-read failures.
            let header = match header {
                Some(header) => Some(header),
                None => super::list::read_session_meta_line(&path).await.ok(),
            };
            if let Some(session_meta) = header {
                drop(rx);
                scan.await.map_err(std::io::Error::other)??;
                return Ok(Some((path, session_meta)));
            }
        }
    }
    scan.await.map_err(std::io::Error::other)??;

    Ok(None)
}

fn session_index_path(codex_home: &Path) -> PathBuf {
    codex_home.join(SESSION_INDEX_FILE)
}

fn scan_index_from_end_by_id(
    path: &Path,
    thread_id: &ThreadId,
) -> std::io::Result<Option<SessionIndexEntry>> {
    scan_index_from_end(path, |entry| entry.id == *thread_id)
}

#[derive(Clone, PartialEq)]
struct IndexVersion {
    identity: (u64, u64),
    length: u64,
    modified: std::time::SystemTime,
}

impl IndexVersion {
    fn read(file: &File) -> Option<Self> {
        let metadata = file.metadata().ok()?;
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            (metadata.dev(), metadata.ino())
        };
        #[cfg(windows)]
        let identity = {
            use std::os::windows::io::AsRawHandle;
            let mut info = std::mem::MaybeUninit::uninit();
            // The opened handle remains live across the identity query/cache.
            if unsafe { windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(
                file.as_raw_handle().cast(), info.as_mut_ptr(),
            ) } == 0 { return None; }
            let info = unsafe { info.assume_init() };
            (u64::from(info.dwVolumeSerialNumber),
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow))
        };
        #[cfg(not(any(unix, windows)))]
        let identity = return None;
        Some(Self { identity, length: metadata.len(), modified: metadata.modified().ok()? })
    }
}

struct CachedNames {
    version: IndexVersion,
    // Keep the old file identity allocated even after atomic replacement.
    _file: File,
    names: HashMap<ThreadId, Option<String>>,
}

fn cached_thread_names_by_ids(
    path: &Path,
    thread_ids: &HashSet<ThreadId>,
) -> std::io::Result<HashMap<ThreadId, String>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, CachedNames>>> = std::sync::OnceLock::new();
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(error),
    };
    let Some(version) = IndexVersion::read(&file) else {
        return scan_index_from_end_by_ids(path, thread_ids);
    };
    let cache = CACHE.get_or_init(Default::default);
    let known = {
        let cached = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cached.get(path).filter(|entry| entry.version == version).map(|entry| {
            thread_ids.iter().filter_map(|id| entry.names.get(id).map(|name| (*id, name.clone())))
                .collect::<HashMap<_, _>>()
        }).unwrap_or_default()
    };
    let pending = thread_ids.iter().filter(|id| !known.contains_key(*id))
        .copied().collect::<HashSet<_>>();
    let mut names = known.into_iter().filter_map(|(id, name)| name.map(|name| (id, name)))
        .collect::<HashMap<_, _>>();
    if pending.is_empty() { return Ok(names); }
    // Known negatives must not force every new page to scan to the beginning.
    // Scan only newly requested IDs, retaining the reverse reader's blank-name
    // and malformed-line behavior.
    names.extend(scan_index_from_end_by_ids_on_file(&mut file, &pending)?);
    let named_version = File::open(path).ok().and_then(|file| IndexVersion::read(&file));
    if IndexVersion::read(&file).as_ref() == Some(&version)
        && named_version.as_ref() == Some(&version)
        && thread_ids.len() <= 2048
        && names.values().map(String::len).sum::<usize>() <= 4 * 1024 * 1024
    {
        let mut cached = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if cached.len() >= 4 && !cached.contains_key(path)
            && let Some(oldest) = cached.keys().next().cloned()
        { cached.remove(&oldest); }
        // The query result cache is deliberately bounded. Appends, truncation,
        // replacement or same-length rewrites invalidate positive AND negative
        // results together; no cross-process append/remove is hidden.
        let mut observed = cached.remove(path).filter(|entry| entry.version == version)
            .map(|entry| entry.names).unwrap_or_default();
        observed.extend(thread_ids.iter().map(|id| (*id, names.get(id).cloned())));
        if observed.len() > 2048
            || observed.values().filter_map(Option::as_ref).map(String::len).sum::<usize>() > 4 * 1024 * 1024
        {
            observed = thread_ids.iter().map(|id| (*id, names.get(id).cloned())).collect();
        }
        cached.insert(path.to_path_buf(), CachedNames { version, _file: file, names: observed });
    }
    Ok(names)
}

fn scan_index_from_end_by_ids(
    path: &Path,
    thread_ids: &HashSet<ThreadId>,
) -> std::io::Result<HashMap<ThreadId, String>> {
    match File::open(path) {
        Ok(mut file) => scan_index_from_end_by_ids_on_file(&mut file, thread_ids),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(HashMap::new()),
        Err(error) => Err(error),
    }
}

fn scan_index_from_end_by_ids_on_file(
    file: &mut File,
    thread_ids: &HashSet<ThreadId>,
) -> std::io::Result<HashMap<ThreadId, String>> {
    let mut pending = thread_ids.clone();
    let mut names = HashMap::with_capacity(thread_ids.len());
    match scan_index_from_end_for_each_file(file, |entry| {
        let name = entry.thread_name.trim();
        if pending.remove(&entry.id) {
            if !name.is_empty() {
                names.insert(entry.id, name.to_string());
            }
            if pending.is_empty() {
                return Ok(Some(entry.clone()));
            }
        }
        Ok(None)
    }) {
        Ok(_) => Ok(names),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(HashMap::new()),
        Err(error) => Err(error),
    }
}

fn stream_thread_ids_from_end_by_name(
    path: &Path,
    name: &str,
    tx: tokio::sync::mpsc::Sender<ThreadId>,
) -> std::io::Result<()> {
    let mut seen = HashSet::new();
    scan_index_from_end_for_each(path, |entry| {
        if tx.is_closed() {
            return Ok(Some(entry.clone()));
        }
        // The first row seen for an id is its latest name. Ignore older rows for that id so a
        // historical name cannot be treated as the current one after the thread is renamed.
        if seen.insert(entry.id) && entry.thread_name == name && tx.blocking_send(entry.id).is_err()
        {
            return Ok(Some(entry.clone()));
        }
        Ok(None)
    })?;
    Ok(())
}

fn scan_index_from_end<F>(
    path: &Path,
    mut predicate: F,
) -> std::io::Result<Option<SessionIndexEntry>>
where
    F: FnMut(&SessionIndexEntry) -> bool,
{
    scan_index_from_end_for_each(path, |entry| {
        if predicate(entry) {
            return Ok(Some(entry.clone()));
        }
        Ok(None)
    })
}

fn scan_index_from_end_for_each<F>(
    path: &Path,
    visit_entry: F,
) -> std::io::Result<Option<SessionIndexEntry>>
where
    F: FnMut(&SessionIndexEntry) -> std::io::Result<Option<SessionIndexEntry>>,
{
    let mut file = File::open(path)?;
    scan_index_from_end_for_each_file(&mut file, visit_entry)
}

fn scan_index_from_end_for_each_file<F>(
    file: &mut File,
    mut visit_entry: F,
) -> std::io::Result<Option<SessionIndexEntry>>
where
    F: FnMut(&SessionIndexEntry) -> std::io::Result<Option<SessionIndexEntry>>,
{
    let mut remaining = file.metadata()?.len();
    let mut line_rev: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; READ_CHUNK_SIZE];

    while remaining > 0 {
        let read_size = usize::try_from(remaining.min(READ_CHUNK_SIZE as u64))
            .map_err(std::io::Error::other)?;
        remaining -= read_size as u64;
        file.seek(SeekFrom::Start(remaining))?;
        file.read_exact(&mut buf[..read_size])?;

        for &byte in buf[..read_size].iter().rev() {
            if byte == b'\n' {
                if let Some(entry) = parse_line_from_rev(&mut line_rev, &mut visit_entry)? {
                    return Ok(Some(entry));
                }
                continue;
            }
            line_rev.push(byte);
        }
    }

    if let Some(entry) = parse_line_from_rev(&mut line_rev, &mut visit_entry)? {
        return Ok(Some(entry));
    }

    Ok(None)
}




fn parse_line_from_rev<F>(
    line_rev: &mut Vec<u8>,
    visit_entry: &mut F,
) -> std::io::Result<Option<SessionIndexEntry>>
where
    F: FnMut(&SessionIndexEntry) -> std::io::Result<Option<SessionIndexEntry>>,
{
    if line_rev.is_empty() {
        return Ok(None);
    }
    line_rev.reverse();
    let line = std::mem::take(line_rev);
    let Ok(mut line) = String::from_utf8(line) else {
        return Ok(None);
    };
    if line.ends_with('\r') {
        line.pop();
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(trimmed) else {
        return Ok(None);
    };
    visit_entry(&entry)
}

