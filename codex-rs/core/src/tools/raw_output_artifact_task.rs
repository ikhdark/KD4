use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use sha2::{Digest, Sha256};
use base64::Engine as _;
use super::{RawOutputArtifact, ToolOutputArtifactId, ToolOutputSelector};

/// Plain command logs are separate from indexed source snapshots. Writes apply
/// disk backpressure instead of retaining an unbounded queue or dropping a prefix.
/// Disk exhaustion fails capture explicitly; it never turns a partial log complete.
#[derive(Debug)]
pub(crate) struct RawOutputArtifactTask {
    state: Arc<Mutex<RawOutputArtifact>>,
    ready: CancellationToken,
    writer: Option<BufWriter<File>>,
    handle: Option<Arc<File>>,
    _residency: Option<File>,
    id: ToolOutputArtifactId,
    path: PathBuf,
    bytes: u64,
    digest: Sha256,
    incomplete: bool,
    failure: Option<String>,
    finished: bool,
}

/// Only original producer chunks enter this sink. Display decoding, cap markers
/// and cancellation/status notices never become canonical command bytes.
#[derive(Debug)]
pub(crate) struct LegacyRawCapture {
    task: Option<RawOutputArtifactTask>,
    inline: Vec<u8>,
    gap: bool,
    attached: bool,
}

pub(crate) type SharedLegacyRawCapture = Arc<std::sync::Mutex<LegacyRawCapture>>;

tokio::task_local! {
    static LEGACY_RAW_CAPTURE: SharedLegacyRawCapture;
}

pub(crate) fn current_legacy_capture() -> Option<SharedLegacyRawCapture> {
    LEGACY_RAW_CAPTURE
        .try_with(|capture| {
            capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .attached = true;
            Arc::clone(capture)
        })
        .ok()
}

impl LegacyRawCapture {
    pub(crate) fn write_chunk(&mut self, bytes: &[u8]) {
        let remaining = super::MAX_RAW_OUTPUT_ARTIFACT_BYTES.saturating_sub(self.inline.len());
        self.inline.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        self.gap |= bytes.len() > remaining;
        if let Some(task) = &mut self.task {
            task.write_chunk(bytes);
        }
    }

    pub(crate) fn mark_output_gap(&mut self) {
        self.gap = true;
        if let Some(task) = &mut self.task {
            task.mark_output_gap();
        }
    }
}

#[derive(Clone)]
pub(crate) struct CapturedShellOutput {
    pub(crate) artifact: RawOutputArtifact,
    pub(crate) canonical: Option<codex_tools::CanonicalToolResult>,
}

pub(crate) async fn capture_legacy_output<F: std::future::Future>(
    home: &std::path::Path,
    thread_id: &str,
    future: F,
) -> (F::Output, CapturedShellOutput) {
    let state = Arc::new(Mutex::new(RawOutputArtifact::pending(home, thread_id)));
    let capture = Arc::new(std::sync::Mutex::new(LegacyRawCapture {
        task: RawOutputArtifactTask::spawn(Some(Arc::clone(&state)), CancellationToken::new()),
        inline: Vec::new(),
        gap: false,
        attached: false,
    }));
    let output = LEGACY_RAW_CAPTURE.scope(Arc::clone(&capture), future).await;
    // Never hold the reader's synchronous lock while waiting for storage.
    let (task, inline, gap, attached) = {
        let mut capture = capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (capture.task.take(), std::mem::take(&mut capture.inline), capture.gap, capture.attached)
    };
    if !attached {
        drop(task);
        return (output, CapturedShellOutput {
            artifact: RawOutputArtifact::pending(home, thread_id),
            canonical: None,
        });
    }
    if let Some(task) = task {
        task.finish().await;
    }
    let artifact = state.lock().await.clone();
    let mut canonical = codex_tools::CanonicalToolResult::bytes(inline);
    canonical.complete = !gap;
    (
        output,
        CapturedShellOutput {
            artifact,
            canonical: Some(canonical),
        },
    )
}



impl RawOutputArtifactTask {
    pub(crate) fn spawn(state: Option<Arc<Mutex<RawOutputArtifact>>>, ready: CancellationToken) -> Option<Self> {
        let state = state?;
        let target = match state.try_lock() {
            Ok(state) => state.clone(),
            Err(_) => {
                ready.cancel();
                return None;
            }
        };
        Self::spawn_with_target(Some(state), ready, Some(target))
    }

    pub(crate) fn spawn_with_target(
        state: Option<Arc<Mutex<RawOutputArtifact>>>,
        ready: CancellationToken,
        target: Option<RawOutputArtifact>,
    ) -> Option<Self> {
        let Some(state) = state else { ready.cancel(); return None; };
        let (codex_home, thread_id, previous) = match target {
            Some(RawOutputArtifact::Pending { codex_home, thread_id }) => (codex_home, thread_id, None),
            Some(RawOutputArtifact::Stored { path, truncated, .. }) => {
                let thread = path.parent()?;
                let store = thread.parent()?;
                if !matches!(store.file_name().and_then(|s| s.to_str()), Some("tool-output" | "command-output")) {
                    ready.cancel();
                    return None;
                }
                (store.parent()?.to_path_buf(), thread.file_name()?.to_string_lossy().into_owned(), Some((path, truncated)))
            }
            _ => { ready.cancel(); return None; }
        };
        let id = ToolOutputArtifactId::new();
        let path = codex_home.join("command-output").join(&thread_id).join(format!("{id}.log"));
        let mut capture = Self { state, ready, writer: None, handle: None, _residency: None, id, path, bytes: 0, digest: Sha256::new(),
            incomplete: false, failure: None, finished: false };
        if let Some((previous, incomplete)) = previous {
            capture.incomplete = incomplete;
            match File::open(previous) {
                Ok(mut file) => {
                    let mut buffer = [0u8; 64 * 1024];
                    loop {
                        match file.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(length) => capture.write_chunk(&buffer[..length]),
                            Err(error) => {
                                capture.failure = Some(error.to_string());
                                capture.writer.take();
                                capture.incomplete = true;
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    capture.failure = Some(error.to_string());
                    capture.writer.take();
                    capture.incomplete = true;
                }
            }
        }
        capture.publish_live();
        Some(capture)
    }

    pub(crate) fn write_chunk(&mut self, output: &[u8]) {
        if output.is_empty() { return; }
        if self.writer.is_none() && self.failure.is_none() {
            if let Err(error) = self.initialize_writer() {
                self.failure = Some(error.to_string());
                self.incomplete = true;
                return;
            }
        }
        let Some(writer) = self.writer.as_mut() else { return; };
        if let Err(error) = writer.write_all(output) {
            self.failure = Some(error.to_string());
            self.writer.take();
            self.incomplete = true;
            return;
        }
        self.bytes = self.bytes.saturating_add(output.len() as u64);
        self.digest.update(output);
        self.publish_live();
    }

    fn initialize_writer(&mut self) -> std::io::Result<()> {
        let thread = self.path.parent().ok_or_else(|| std::io::Error::other("missing log parent"))?;
        let home = thread.parent().and_then(Path::parent).ok_or_else(|| std::io::Error::other("missing log home"))?;
        let thread_id = thread.file_name().and_then(|name| name.to_str()).ok_or_else(|| std::io::Error::other("invalid log thread"))?;
        let residency = super::artifact_residency_file(home, thread_id, false)?;
        std::fs::create_dir_all(thread)?;
        let file = OpenOptions::new().create_new(true).read(true).write(true).open(&self.path)?;
        // Retained receipts only read this plain log. Do not keep write access
        // alive after EOF: it would prevent recovery's immutable read pin.
        self.handle = Some(Arc::new(command_log_read_handle(&file)?));
        self.writer = Some(BufWriter::with_capacity(64 * 1024, file));
        self._residency = Some(residency);
        Ok(())
    }

    fn publish_live(&self) {
        let Some(handle) = &self.handle else { return; };
        if let Ok(mut state) = self.state.try_lock() {
            *state = RawOutputArtifact::Stored {
                id: self.id, path: self.path.clone(), bytes: self.bytes,
                truncated: self.incomplete, handle: Arc::clone(handle),
                digest: self.digest.clone(), finalized: false,
            };
        }
    }

    pub(crate) fn mark_output_gap(&mut self) {
        self.incomplete = true;
        self.publish_live();
    }

    pub(crate) async fn finish(mut self) {
        // Empty, complete producers need no durable recovery object. Leave the
        // pending (or previous empty) state for consumers' inline fallback.
        if self.handle.is_none() && self.failure.is_none() && !self.incomplete {
            self.finished = true;
            self.ready.cancel();
            return;
        }
        let artifact = if let Some(mut writer) = self.writer.take() {
            let result = (|| {
                writer.flush()?;
                writer.get_ref().sync_all()?;
                let handle = Arc::clone(self.handle.as_ref().ok_or_else(|| std::io::Error::other("command log handle unavailable"))?);
                let status = serde_json::json!({
                    "complete": !self.incomplete,
                    "bytes": self.bytes,
                    "sha256": hex::encode(self.digest.clone().finalize()),
                });
                codex_file_system::write_bytes_atomically(
                    &self.path.with_extension("status"), &serde_json::to_vec(&status)?,
                )?;
                Ok::<_, std::io::Error>(handle)
            })();
            match result {
                Ok(handle) => RawOutputArtifact::Stored {
                    id: self.id, path: self.path.clone(), bytes: self.bytes,
                    truncated: self.incomplete, handle, digest: self.digest.clone(), finalized: true,
                },
                Err(error) => self.failed(error.to_string()),
            }
        } else {
            self.failed(self.failure.clone().unwrap_or_else(|| "command log unavailable".into()))
        };
        *self.state.lock().await = artifact;
        self.finished = true;
        self.ready.cancel();
    }

    fn failed(&self, message: String) -> RawOutputArtifact {
        RawOutputArtifact::Failed { id: self.handle.as_ref().map(|_| self.id), message, owned_path: self.handle.as_ref().map(|_| self.path.clone()), bytes: self.bytes }
    }
}

impl Drop for RawOutputArtifactTask {
    fn drop(&mut self) {
        if !self.finished {
            // Preserve an explicitly incomplete prefix even when the owner is
            // cancelled. No cancellation path publishes a complete transcript.
            if let Some(writer) = self.writer.as_mut() { let _ = writer.flush(); }
            let status = serde_json::json!({
                "complete": false, "bytes": self.bytes,
                "sha256": hex::encode(self.digest.clone().finalize()),
            });
            if self.handle.is_some() {
                if let Ok(bytes) = serde_json::to_vec(&status) {
                    let _ = codex_file_system::write_bytes_atomically(&self.path.with_extension("status"), &bytes);
                }
            }
            if let Ok(mut state) = self.state.try_lock() {
                *state = self.failed("command output capture ended before finalization".into());
            }
            self.ready.cancel();
        }
    }
}

fn command_log_read_handle(file: &File) -> std::io::Result<File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, ReOpenFile,
    };
    // Reopen the same object, never the mutable pathname. The producer keeps its
    // original write handle until finish; all published snapshots get read access.
    // SAFETY: `file` owns a live synchronous disk handle. A successful reopen
    // returns a new owned handle, transferred exactly once into File below.
    let handle = unsafe { ReOpenFile(file.as_raw_handle(), FILE_GENERIC_READ,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, 0) };
    if handle == INVALID_HANDLE_VALUE {
        // Unsupported filesystems retain the old capture behavior and simply
        // miss the optional recovery cache while their write handle is alive.
        file.try_clone()
    } else {
        Ok(unsafe { File::from_raw_handle(handle) })
    }
}

#[derive(Clone, Eq, PartialEq)]
struct CommandLogIdentity {
    path: PathBuf,
    volume: u64,
    file_id: [u8; 16],
    created: i64,
    changed: i64,
    bytes: u64,
}

#[derive(Clone, Default)]
struct CommandLogLineIndex {
    // Exact line starts only. No UTF-8 decoding or fixed-width assumption.
    points: Vec<(usize, u64)>,
}

const COMMAND_LOG_INDEX_POINTS: usize = 1024;
const COMMAND_LOG_INDEX_CACHE_BYTES: usize = 2 * 1024 * 1024;
const COMMAND_LOG_INDEX_CACHE_ENTRIES: usize = 32;

impl CommandLogLineIndex {
    fn before(&self, line: usize) -> (usize, u64) {
        let end = self.points.partition_point(|(known, _)| *known <= line);
        end.checked_sub(1).map_or((1, 0), |index| self.points[index])
    }

    fn record(&mut self, line: usize, offset: u64) {
        if self.points.binary_search_by_key(&line, |(line, _)| *line).is_ok() { return; }
        if self.points.len() == COMMAND_LOG_INDEX_POINTS { self.points.remove(0); }
        let index = self.points.partition_point(|(known, _)| *known < line);
        self.points.insert(index, (line, offset));
    }
}

struct CachedCommandLogIndex {
    identity: CommandLogIdentity,
    index: CommandLogLineIndex,
    bytes: usize,
}

static COMMAND_LOG_INDEXES: std::sync::LazyLock<std::sync::Mutex<std::collections::VecDeque<CachedCommandLogIndex>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::VecDeque::new()));

fn retain_command_log_index(identity: CommandLogIdentity, mut index: CommandLogLineIndex) {
    let mut cache = COMMAND_LOG_INDEXES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(position) = cache.iter().position(|entry| entry.identity == identity)
        && let Some(previous) = cache.remove(position)
    {
        for (line, offset) in previous.index.points { index.record(line, offset); }
    }
    let bytes = std::mem::size_of::<CachedCommandLogIndex>()
        .saturating_add(identity.path.capacity().saturating_mul(2))
        .saturating_add(index.points.capacity().saturating_mul(std::mem::size_of::<(usize, u64)>()));
    if bytes > COMMAND_LOG_INDEX_CACHE_BYTES { return; }
    while cache.len() >= COMMAND_LOG_INDEX_CACHE_ENTRIES
        || cache.iter().map(|entry| entry.bytes).sum::<usize>().saturating_add(bytes) > COMMAND_LOG_INDEX_CACHE_BYTES
    { cache.pop_front(); }
    cache.push_back(CachedCommandLogIndex { identity, index, bytes });
}

// A cache hit requires an actual file identity and its change generation, not
// path/mtime or the unauthenticated status digest. Deny writes only during this
// read; retain no file handles in the cache and do not block rename/cleanup.
fn open_command_log(path: &Path) -> std::io::Result<(File, Option<CommandLogIdentity>)> {
    use std::mem::MaybeUninit;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx,
    };

    let file = match OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE).open(path) {
        Ok(file) => file,
        // An active writer or unsupported pin uses the original uncached read.
        Err(_) => return File::open(path).map(|file| (file, None)),
    };
    let mut id = MaybeUninit::<FILE_ID_INFO>::zeroed();
    let mut basic = MaybeUninit::<FILE_BASIC_INFO>::zeroed();
    // SAFETY: the live pinned file owns this handle; both aligned buffers and
    // exact sizes match their query classes. Failed queries are never consumed.
    let identified = unsafe {
        GetFileInformationByHandleEx(file.as_raw_handle(), FileIdInfo, id.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32) != 0
            && GetFileInformationByHandleEx(file.as_raw_handle(), FileBasicInfo, basic.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32) != 0
    };
    let identity = if identified {
        // SAFETY: both successful queries initialized these complete buffers.
        let (id, basic) = unsafe { (id.assume_init(), basic.assume_init()) };
        file.metadata().ok().filter(|metadata| metadata.is_file()).map(|metadata| CommandLogIdentity {
            path: path.to_owned(), volume: id.VolumeSerialNumber, file_id: id.FileId.Identifier,
            created: basic.CreationTime, changed: basic.ChangeTime, bytes: metadata.len(),
        }).filter(|identity| identity.created > 0 && identity.changed > 0
            && (identity.volume != 0 || identity.file_id.iter().any(|byte| *byte != 0)))
    } else { None };
    Ok((file, identity))
}

/// Return None for an old/indexed artifact so its existing recovery owner handles it.
/// Reads are thread-confined, bounded-memory and never mutate saved logs.
pub(crate) async fn read_command_log(
    home: &Path, thread_id: &str, artifact_id: &str,
    selectors: &[ToolOutputSelector], max_bytes: usize,
) -> Result<Option<serde_json::Value>, super::ReadToolOutputError> {
    let id = artifact_id.parse::<ToolOutputArtifactId>()
        .map_err(|_| super::ReadToolOutputError::InvalidArtifactId)?;
    if id.to_string() != artifact_id { return Err(super::ReadToolOutputError::InvalidArtifactId); }
    let path = home.join("command-output").join(thread_id).join(format!("{id}.log"));
    let selectors = selectors.to_vec();
    let artifact_id = artifact_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(super::ReadToolOutputError::Io(error.to_string())),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(super::ReadToolOutputError::Io("command log is not a regular file".into()));
        }
        let status = std::fs::read(path.with_extension("status"))
            .map_err(|error| if error.kind() == std::io::ErrorKind::NotFound {
                super::ReadToolOutputError::StillWriting
            } else { super::ReadToolOutputError::Io(error.to_string()) })?;
        let status: serde_json::Value = serde_json::from_slice(&status)
            .map_err(|error| super::ReadToolOutputError::Io(error.to_string()))?;
        let mut remaining = max_bytes;
        let mut results = Vec::new();
        let mut offsets = std::collections::BTreeMap::new();
        let mut ranges = Vec::with_capacity(selectors.len());
        let mut byte_ranges = Vec::new();
        for selector in &selectors {
            if let ToolOutputSelector::Bytes { start, end } = selector {
                if start > end || *end > metadata.len() {
                    return Err(super::ReadToolOutputError::InvalidRange(format!(
                        "byte range must be within retained bytes 0..{}", metadata.len()
                    )));
                }
                byte_ranges.push((*start, *end));
                continue;
            }
            let ToolOutputSelector::Lines { start, end } = selector else {
                return Err(super::ReadToolOutputError::InvalidRange(
                    "Command logs support bytes and lines selectors; source snapshots retain advanced selectors.".into()));
            };
            if *start == 0 || end < start {
                return Err(super::ReadToolOutputError::InvalidRange("expected 1-based start <= end".into()));
            }
            ranges.push((*start, *end));
        }
        // Use the snapshot owner's canonical union, not independent overlapping
        // deliveries charged repeatedly against the same response budget.
        let selectors = super::normalize_line_ranges(ranges).into_iter()
            .map(|(start, end)| ToolOutputSelector::Lines { start, end })
            .collect::<Vec<_>>();
        for selector in &selectors {
            if let ToolOutputSelector::Lines { start, .. } = selector {
                offsets.insert(*start, None);
            }
        }
        let io_error = |error: std::io::Error| super::ReadToolOutputError::Io(error.to_string());
        let (file, identity) = if offsets.is_empty() {
            (File::open(&path).map_err(io_error)?, None)
        } else {
            open_command_log(&path).map_err(io_error)?
        };
        let mut line_index = identity.as_ref().and_then(|identity|
            COMMAND_LOG_INDEXES.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter().find(|entry| &entry.identity == identity).map(|entry| entry.index.clone())
        ).unwrap_or_default();
        let mut reader = BufReader::new(file);
        let mut scanned_line = 1usize;
        let mut offset = 0u64;
        // Resume at an authenticated exact checkpoint. Cold reads retain the
        // original prefix scan, recording only requested starts and page cursors.
        for (start, position) in &mut offsets {
            let (known_line, known_offset) = line_index.before(*start);
            if known_line > scanned_line {
                reader.seek(SeekFrom::Start(known_offset)).map_err(io_error)?;
                (scanned_line, offset) = (known_line, known_offset);
            }
            while scanned_line < *start {
                let available = reader.fill_buf().map_err(io_error)?;
                if available.is_empty() { break; }
                let newline = available.iter().position(|byte| *byte == b'\n');
                let count = newline.map_or(available.len(), |index| index + 1);
                reader.consume(count);
                offset += count as u64;
                if newline.is_some() { scanned_line += 1; }
            }
            if scanned_line == *start {
                *position = Some(offset);
                line_index.record(scanned_line, offset);
            }
        }
        for selector in selectors {
            let ToolOutputSelector::Lines { start, end } = selector else {
                return Err(super::ReadToolOutputError::InvalidRange(
                    "Command logs support bytes and lines selectors; source snapshots retain advanced selectors.".into()));
            };
            if start == 0 || end < start { return Err(super::ReadToolOutputError::InvalidRange("expected 1-based start <= end".into())); }
            let position = offsets.get(&start).copied().flatten();
            if let Some(position) = position {
                reader.seek(SeekFrom::Start(position)).map_err(io_error)?;
            }
            let mut line = start;
            let mut selection = Vec::new();
            let mut complete = true;
            let mut next_line = None;
            let mut next_bytes = None;
            let mut error = None;
            let mut indexed_end = None;
            while position.is_some() && line <= end {
                let mut bytes = Vec::new();
                let count = Read::by_ref(&mut reader).take(remaining.saturating_add(1) as u64)
                    .read_until(b'\n', &mut bytes)
                    .map_err(|error| super::ReadToolOutputError::Io(error.to_string()))?;
                if count == 0 { break; }
                if count > remaining {
                    line_index.record(line, position.unwrap_or(0) + selection.len() as u64);
                    if selection.is_empty() && remaining == max_bytes {
                        // Find the whole unconsumed selection's exact byte extent
                        // without buffering it, including later requested lines.
                        // read_file cannot recover oversized or non-UTF-8 logs.
                        let start = position.unwrap_or(0);
                        let mut byte_end = start + count as u64;
                        let mut lines_left = end - line + 1;
                        if bytes.last() == Some(&b'\n') { lines_left -= 1; }
                        while lines_left > 0 {
                            let available = reader.fill_buf().map_err(io_error)?;
                            if available.is_empty() { break; }
                            let newline = available.iter().position(|byte| *byte == b'\n');
                            let count = newline.map_or(available.len(), |index| index + 1);
                            reader.consume(count);
                            byte_end += count as u64;
                            if newline.is_some() { lines_left -= 1; }
                        }
                        next_bytes = Some((start, byte_end));
                        error = Some(format!(
                            "Line {line} cannot fit this whole-line response ({max_bytes} source bytes). Recover the remaining requested lines with read_tool_output using the returned byte continuation."
                        ));
                    } else {
                        next_line = Some(line);
                    }
                    complete = false;
                    break;
                }
                remaining -= count;
                let newline = bytes.last() == Some(&b'\n');
                selection.extend_from_slice(&bytes);
                line += 1;
                if newline { indexed_end = Some((line, position.unwrap_or(0) + selection.len() as u64)); }
            }
            if let Some((line, offset)) = indexed_end { line_index.record(line, offset); }
            let mut item = serde_json::json!({
                "selector":{"kind":"lines","start":start,"end":end},
                "status":if error.is_some() { "selector_too_large" } else { "ok" }, "complete":complete,
            });
            if let Some(start) = position {
                item["canonical_range"] = serde_json::json!({"start":start,"end":start + selection.len() as u64});
            }
            if let Some(error) = error { item["message"] = error.into(); }
            match String::from_utf8(selection) {
                Ok(text) => item["text"] = text.into(),
                Err(error) => item["data_base64"] =
                    base64::engine::general_purpose::STANDARD.encode(error.into_bytes()).into(),
            }
            if let Some(start) = next_line {
                item["continuation"] = serde_json::json!({"kind":"lines","start":start,"end":end});
            }
            if let Some((start, end)) = next_bytes {
                item["continuation"] = serde_json::json!({"kind":"bytes","start":start,"end":end});
            }
            results.push(item);
        }
        byte_ranges.sort_unstable();
        let mut normalized_bytes: Vec<(u64, u64)> = Vec::new();
        for (start, end) in byte_ranges {
            if let Some((_, previous_end)) = normalized_bytes.last_mut()
                && start <= *previous_end
            {
                *previous_end = (*previous_end).max(end);
            } else {
                normalized_bytes.push((start, end));
            }
        }
        for (start, end) in normalized_bytes {
            let count = (end - start).min(remaining as u64) as usize;
            reader.seek(SeekFrom::Start(start)).map_err(io_error)?;
            let mut bytes = vec![0; count];
            reader.read_exact(&mut bytes).map_err(io_error)?;
            remaining -= count;
            let delivered_end = start + count as u64;
            let mut item = serde_json::json!({
                "selector":{"kind":"bytes","start":start,"end":end},
                "status":"ok", "complete":delivered_end == end,
                "canonical_range":{"start":start,"end":delivered_end},
            });
            match String::from_utf8(bytes) {
                Ok(text) => item["text"] = text.into(),
                Err(error) => item["data_base64"] =
                    base64::engine::general_purpose::STANDARD.encode(error.into_bytes()).into(),
            }
            if delivered_end < end {
                item["continuation"] = serde_json::json!({"kind":"bytes","start":delivered_end,"end":end});
            }
            results.push(item);
        }
        results.sort_by_key(|result| result["canonical_range"]["start"].as_u64().unwrap_or(u64::MAX));
        if let Some(identity) = identity { retain_command_log_index(identity, line_index); }
        Ok(Some(serde_json::json!({
            "artifact_id":artifact_id,
            "canonical_sha256":status["sha256"],
            "canonical_bytes":metadata.len(),
            "historical_authenticity":"unverified",
            "complete":results.iter().all(|r| r["complete"] == true),
            "delivered_selection_complete":results.iter().all(|r| r["complete"] == true),
            "retained_bytes":metadata.len(),
            "retained_artifact_complete":status["complete"] == true && status["bytes"].as_u64() == Some(metadata.len()),
            "results":results,
        })))
    }).await.map_err(|error| super::ReadToolOutputError::Io(error.to_string()))?
}

