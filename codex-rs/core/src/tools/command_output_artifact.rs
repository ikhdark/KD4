use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::Weak;

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::sync::Notify;

const RETENTION_RETRY_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_millis(25);
const RETENTION_RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

fn max_retained_artifacts_per_thread() -> usize {
    128
}

fn max_retained_artifacts_total() -> usize {
    1_024
}

#[derive(Clone, Default)]
struct PendingRetention {
    directories: HashMap<PathBuf, PathBuf>,
    roots: HashMap<PathBuf, PathBuf>,
}

struct RetentionJanitor {
    pending: StdMutex<PendingRetention>,
    running: AtomicBool,
}

impl RetentionJanitor {
    fn new() -> Self {
        Self {
            pending: StdMutex::new(PendingRetention::default()),
            running: AtomicBool::new(false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RawOutputArtifact {
    Stored {
        path: PathBuf,
        bytes: u64,
    },
    Failed {
        message: String,
        owned_path: Option<PathBuf>,
        bytes: u64,
    },
}

pub(crate) struct RawOutputArtifactWriter {
    path: Option<PathBuf>,
    file: Option<tokio::fs::File>,
    initial_bytes: u64,
    bytes: u64,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RawOutputArtifactWritePositions {
    pub(crate) observed: u64,
    pub(crate) queued: u64,
    pub(crate) committed: u64,
    pub(crate) finalized: u64,
}

#[derive(Default)]
struct RawOutputArtifactPositionTracker {
    observed: AtomicU64,
    queued: AtomicU64,
    committed: AtomicU64,
    finalized: AtomicU64,
    worker_done: AtomicBool,
    worker_done_notify: Notify,
}

impl RawOutputArtifactPositionTracker {
    #[cfg(test)]
    fn snapshot(&self) -> RawOutputArtifactWritePositions {
        RawOutputArtifactWritePositions {
            observed: self.observed.load(Ordering::Acquire),
            queued: self.queued.load(Ordering::Acquire),
            committed: self.committed.load(Ordering::Acquire),
            finalized: self.finalized.load(Ordering::Acquire),
        }
    }

    async fn wait_until_done(&self) {
        loop {
            let notified = self.worker_done_notify.notified();
            if self.worker_done.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Default)]
struct PendingRawOutputArtifactBytes {
    bytes: Vec<u8>,
    finalize_requested: bool,
}

/// Non-blocking producer for one serialized raw-output artifact writer.
///
/// Exact bytes live in one ordered staging buffer while the capacity-one channel
/// only coalesces wakeups. This keeps process draining independent of artifact
/// latency without creating a task or channel entry per output chunk.
pub(crate) struct RawOutputArtifactWriteQueue {
    pending: Arc<StdMutex<PendingRawOutputArtifactBytes>>,
    positions: Arc<RawOutputArtifactPositionTracker>,
    wake_tx: mpsc::Sender<()>,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct RawOutputArtifactWriteObserver {
    positions: Arc<RawOutputArtifactPositionTracker>,
}

#[cfg(test)]
impl RawOutputArtifactWriteObserver {
    pub(crate) fn positions(&self) -> RawOutputArtifactWritePositions {
        self.positions.snapshot()
    }

    pub(crate) async fn wait_until_done(&self) {
        self.positions.wait_until_done().await;
    }
}

impl RawOutputArtifactWriteQueue {
    pub(crate) fn spawn(state: Option<Arc<Mutex<RawOutputArtifact>>>) -> Option<Self> {
        Self::spawn_inner(state, None)
    }

    fn spawn_inner(
        state: Option<Arc<Mutex<RawOutputArtifact>>>,
        #[cfg_attr(not(test), allow(unused_variables))]
        io_gate: Option<Arc<tokio::sync::Semaphore>>,
    ) -> Option<Self> {
        let state = state?;
        let pending = Arc::new(StdMutex::new(PendingRawOutputArtifactBytes::default()));
        let positions = Arc::new(RawOutputArtifactPositionTracker::default());
        let (wake_tx, wake_rx) = mpsc::channel(1);
        tokio::spawn(run_raw_output_artifact_writer(
            state,
            Arc::clone(&pending),
            Arc::clone(&positions),
            wake_rx,
            io_gate,
        ));
        Some(Self {
            pending,
            positions,
            wake_tx,
        })
    }

    pub(crate) fn enqueue(&self, output: &[u8]) {
        let output_len = u64::try_from(output.len()).unwrap_or(u64::MAX);
        self.positions
            .observed
            .fetch_add(output_len, Ordering::AcqRel);
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.bytes.extend_from_slice(output);
            self.positions.queued.fetch_add(output_len, Ordering::AcqRel);
        }
        let _ = self.wake_tx.try_send(());
    }

    pub(crate) async fn finish(self) {
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.finalize_requested = true;
        }
        let _ = self.wake_tx.try_send(());
        self.positions.wait_until_done().await;
    }

    #[cfg(test)]
    pub(crate) fn spawn_with_io_gate(
        state: Option<Arc<Mutex<RawOutputArtifact>>>,
        io_gate: Arc<tokio::sync::Semaphore>,
    ) -> Option<Self> {
        Self::spawn_inner(state, Some(io_gate))
    }

    #[cfg(test)]
    pub(crate) fn observer(&self) -> RawOutputArtifactWriteObserver {
        RawOutputArtifactWriteObserver {
            positions: Arc::clone(&self.positions),
        }
    }
}

impl Drop for RawOutputArtifactWriteQueue {
    fn drop(&mut self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.finalize_requested = true;
    }
}

async fn run_raw_output_artifact_writer(
    state: Arc<Mutex<RawOutputArtifact>>,
    pending: Arc<StdMutex<PendingRawOutputArtifactBytes>>,
    positions: Arc<RawOutputArtifactPositionTracker>,
    mut wake_rx: mpsc::Receiver<()>,
    #[cfg_attr(not(test), allow(unused_variables))] io_gate: Option<Arc<tokio::sync::Semaphore>>,
) {
    #[cfg(test)]
    if let Some(io_gate) = io_gate {
        let _permit = io_gate.acquire().await;
    }

    let mut writer = RawOutputArtifactWriter::open(Some(&state)).await;
    loop {
        let channel_closed = wake_rx.recv().await.is_none();
        loop {
            let (bytes, finalize_requested) = {
                let mut pending = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (std::mem::take(&mut pending.bytes), pending.finalize_requested)
            };
            if !bytes.is_empty()
                && let Some(writer) = writer.as_mut()
            {
                writer.write_chunk(Some(&state), &bytes).await;
                positions
                    .committed
                    .store(writer.committed_position(), Ordering::Release);
            }

            let pending_is_empty = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .bytes
                .is_empty();
            if pending_is_empty && (finalize_requested || channel_closed) {
                if let Some(writer) = writer.as_mut() {
                    writer.finish(Some(&state)).await;
                    positions
                        .committed
                        .store(writer.committed_position(), Ordering::Release);
                }
                positions.finalized.store(
                    positions.committed.load(Ordering::Acquire),
                    Ordering::Release,
                );
                positions.worker_done.store(true, Ordering::Release);
                positions.worker_done_notify.notify_waiters();
                return;
            }
            if pending_is_empty {
                break;
            }
        }
    }
}

impl RawOutputArtifactWriter {
    pub(crate) async fn open(state: Option<&Arc<Mutex<RawOutputArtifact>>>) -> Option<Self> {
        let state = state?;
        let artifact = state.lock().await.clone();
        let RawOutputArtifact::Stored { path, bytes } = artifact else {
            return Some(Self {
                path: None,
                file: None,
                initial_bytes: 0,
                bytes: 0,
            });
        };
        match tokio::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .await
        {
            Ok(file) => match lock_output_file(file).await {
                Ok(file) => Some(Self {
                    path: Some(path),
                    file: Some(file),
                    initial_bytes: bytes,
                    bytes,
                }),
                Err(err) => {
                    enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), &path);
                    *state.lock().await = RawOutputArtifact::Failed {
                        message: format!(
                            "failed to lock `{}` for streaming: {err}",
                            path.display()
                        ),
                        owned_path: Some(path.clone()),
                        bytes,
                    };
                    Some(Self {
                        path: Some(path),
                        file: None,
                        initial_bytes: bytes,
                        bytes,
                    })
                }
            },
            Err(err) => {
                enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), &path);
                *state.lock().await = RawOutputArtifact::Failed {
                    message: format!("failed to open `{}` for streaming: {err}", path.display()),
                    owned_path: Some(path.clone()),
                    bytes,
                };
                Some(Self {
                    path: Some(path),
                    file: None,
                    initial_bytes: bytes,
                    bytes,
                })
            }
        }
    }

    pub(crate) async fn write_chunk(
        &mut self,
        state: Option<&Arc<Mutex<RawOutputArtifact>>>,
        output: &[u8],
    ) {
        let (Some(state), Some(path)) = (state, self.path.clone()) else {
            return;
        };
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(err) = file.write_all(output).await {
            self.file = None;
            *state.lock().await = failed_with_owned_path(
                path.clone(),
                self.bytes,
                format!("failed to stream `{}`: {err}", path.display()),
            )
            .await;
            return;
        }
        self.bytes = self.bytes.saturating_add(output.len() as u64);
        *state.lock().await = RawOutputArtifact::Stored {
            path,
            bytes: self.bytes,
        };
    }

    pub(crate) async fn finish(&mut self, state: Option<&Arc<Mutex<RawOutputArtifact>>>) {
        let (Some(state), Some(path), Some(mut file)) =
            (state, self.path.clone(), self.file.take())
        else {
            return;
        };
        if let Err(err) = file.flush().await {
            drop(file);
            *state.lock().await = failed_with_owned_path(
                path.clone(),
                self.bytes,
                format!("failed to flush `{}`: {err}", path.display()),
            )
            .await;
        }
    }

    fn committed_position(&self) -> u64 {
        self.bytes.saturating_sub(self.initial_bytes)
    }
}

async fn lock_output_file(file: tokio::fs::File) -> std::io::Result<tokio::fs::File> {
    let file = file.into_std().await;
    file.try_lock()?;
    Ok(tokio::fs::File::from_std(file))
}

impl RawOutputArtifact {
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
            owned_path: None,
            bytes: 0,
        }
    }

    pub(crate) fn render_for_model(&self) -> String {
        match self {
            Self::Stored { path, bytes } => format!(
                "Raw output artifact: {} ({bytes} bytes retained before model summarization)",
                path.display()
            ),
            Self::Failed {
                message,
                owned_path: Some(path),
                bytes,
            } => format!(
                "Raw output artifact incomplete: {} ({bytes} bytes retained; {message})",
                path.display()
            ),
            Self::Failed { message, .. } => format!("Raw output artifact unavailable: {message}"),
        }
    }
}

pub(crate) async fn create_raw_output_artifact(
    codex_home: &Path,
    thread_id: &str,
    output: &[u8],
) -> RawOutputArtifact {
    let directory = codex_home.join("tool-output").join(thread_id);
    if let Err(err) = tokio::fs::create_dir_all(&directory).await {
        return RawOutputArtifact::unavailable(format!(
            "failed to create `{}`: {err}",
            directory.display()
        ));
    }

    let path = directory.join(format!("{}.log", uuid::Uuid::now_v7()));
    match tokio::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .await
    {
        Ok(file) => {
            let mut file = match lock_output_file(file).await {
                Ok(file) => file,
                Err(err) => {
                    return failed_with_owned_path(
                        path.clone(),
                        0,
                        format!("failed to lock `{}` for creation: {err}", path.display()),
                    )
                    .await;
                }
            };
            if let Err(err) = file.write_all(output).await {
                drop(file);
                return failed_with_owned_path(
                    path.clone(),
                    0,
                    format!("failed to write `{}`: {err}", path.display()),
                )
                .await;
            }
            if let Err(err) = file.flush().await {
                drop(file);
                return failed_with_owned_path(
                    path.clone(),
                    output.len() as u64,
                    format!("failed to flush `{}`: {err}", path.display()),
                )
                .await;
            }
            drop(file);
            enforce_retention(&directory, &path);
            RawOutputArtifact::Stored {
                path,
                bytes: output.len() as u64,
            }
        }
        Err(err) => {
            enforce_retention(&directory, &path);
            RawOutputArtifact::unavailable(format!("failed to create `{}`: {err}", path.display()))
        }
    }
}

pub(crate) async fn append_raw_output_artifact(
    artifact: &RawOutputArtifact,
    output: &[u8],
) -> RawOutputArtifact {
    let RawOutputArtifact::Stored { path, bytes } = artifact else {
        return artifact.clone();
    };

    match tokio::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .await
    {
        Ok(file) => {
            let mut file = match lock_output_file(file).await {
                Ok(file) => file,
                Err(err) => {
                    return failed_with_owned_path(
                        path.clone(),
                        *bytes,
                        format!("failed to lock `{}` for append: {err}", path.display()),
                    )
                    .await;
                }
            };
            if let Err(err) = file.write_all(output).await {
                drop(file);
                return failed_with_owned_path(
                    path.clone(),
                    *bytes,
                    format!("failed to append `{}`: {err}", path.display()),
                )
                .await;
            }
            if let Err(err) = file.flush().await {
                drop(file);
                return failed_with_owned_path(
                    path.clone(),
                    (*bytes).saturating_add(output.len() as u64),
                    format!("failed to flush `{}`: {err}", path.display()),
                )
                .await;
            }
            match file.metadata().await {
                Ok(metadata) => {
                    enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), path);
                    RawOutputArtifact::Stored {
                        path: path.clone(),
                        bytes: metadata.len(),
                    }
                }
                Err(err) => {
                    drop(file);
                    failed_with_owned_path(
                        path.clone(),
                        (*bytes).saturating_add(output.len() as u64),
                        format!("failed to stat `{}` after append: {err}", path.display()),
                    )
                    .await
                }
            }
        }
        Err(err) => {
            failed_with_owned_path(
                path.clone(),
                *bytes,
                format!("failed to open `{}` for append: {err}", path.display()),
            )
            .await
        }
    }
}

pub(crate) async fn replace_raw_output_artifact(
    artifact: &RawOutputArtifact,
    output: &[u8],
) -> RawOutputArtifact {
    let RawOutputArtifact::Stored { path, bytes } = artifact else {
        return artifact.clone();
    };

    match tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .await
    {
        Ok(file) => {
            let mut file = match lock_output_file(file).await {
                Ok(file) => file,
                Err(err) => {
                    return failed_with_owned_path(
                        path.clone(),
                        *bytes,
                        format!("failed to lock `{}` for replacement: {err}", path.display()),
                    )
                    .await;
                }
            };
            if let Err(err) = file.set_len(0).await {
                drop(file);
                return failed_with_owned_path(
                    path.clone(),
                    *bytes,
                    format!(
                        "failed to truncate `{}` for replacement: {err}",
                        path.display()
                    ),
                )
                .await;
            }
            if let Err(err) = file.write_all(output).await {
                drop(file);
                return failed_with_owned_path(
                    path.clone(),
                    0,
                    format!("failed to replace `{}`: {err}", path.display()),
                )
                .await;
            }
            if let Err(err) = file.flush().await {
                drop(file);
                return failed_with_owned_path(
                    path.clone(),
                    output.len() as u64,
                    format!("failed to flush `{}`: {err}", path.display()),
                )
                .await;
            }
            enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), path);
            RawOutputArtifact::Stored {
                path: path.clone(),
                bytes: output.len() as u64,
            }
        }
        Err(err) => {
            failed_with_owned_path(
                path.clone(),
                *bytes,
                format!("failed to open `{}` for replacement: {err}", path.display()),
            )
            .await
        }
    }
}

async fn failed_with_owned_path(
    path: PathBuf,
    fallback_bytes: u64,
    message: String,
) -> RawOutputArtifact {
    let bytes = tokio::fs::metadata(&path)
        .await
        .map_or(fallback_bytes, |metadata| metadata.len());
    enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), &path);
    RawOutputArtifact::Failed {
        message,
        owned_path: Some(path),
        bytes,
    }
}

fn enforce_retention(directory: &Path, keep_path: &Path) {
    schedule_retention(directory, keep_path);
}

async fn enforce_local_retention_locked(directory: &Path, keep_path: &Path) -> bool {
    let mut entries = match tokio::fs::read_dir(directory).await {
        Ok(entries) => entries,
        Err(err) => return err.kind() == std::io::ErrorKind::NotFound,
    };
    let mut paths = Vec::new();
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(_) => return false,
        };
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("log") {
            paths.push(path);
        }
    }
    paths.sort_unstable();

    let mut retained_count = paths.len();
    for path in paths {
        if retained_count <= max_retained_artifacts_per_thread() {
            break;
        }
        if path == keep_path {
            continue;
        }
        if remove_inactive_output_path(path).await {
            retained_count = retained_count.saturating_sub(1);
        }
    }
    retained_count <= max_retained_artifacts_per_thread()
}

#[cfg(test)]
async fn enforce_global_retention(tool_output_root: &Path, keep_path: &Path) {
    schedule_global_retention(tool_output_root, keep_path);
    wait_for_retention_idle().await;
}

async fn enforce_global_retention_locked(tool_output_root: &Path, keep_path: &Path) -> bool {
    let mut thread_directories = match tokio::fs::read_dir(tool_output_root).await {
        Ok(entries) => entries,
        Err(err) => return err.kind() == std::io::ErrorKind::NotFound,
    };
    let mut paths = Vec::new();
    loop {
        let thread_directory = match thread_directories.next_entry().await {
            Ok(Some(entry)) => entry.path(),
            Ok(None) => break,
            Err(_) => return false,
        };
        let mut entries = match tokio::fs::read_dir(&thread_directory).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return false,
        };
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_) => return false,
            };
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("log") {
                let modified = entry
                    .metadata()
                    .await
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                paths.push((modified, path));
            }
        }
    }
    paths.sort_unstable_by(|(left_time, left_path), (right_time, right_path)| {
        left_time
            .cmp(right_time)
            .then_with(|| left_path.cmp(right_path))
    });
    let mut retained_count = paths.len();
    for (_, path) in paths {
        if retained_count <= max_retained_artifacts_total() {
            break;
        }
        if path != keep_path && remove_inactive_output_path(path).await {
            retained_count = retained_count.saturating_sub(1);
        }
    }
    retained_count <= max_retained_artifacts_total()
}

fn schedule_retention(directory: &Path, keep_path: &Path) {
    let Some(tool_output_root) = directory.parent() else {
        return;
    };
    let janitor = retention_janitor();
    {
        let mut pending = janitor
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending
            .directories
            .insert(directory.to_path_buf(), keep_path.to_path_buf());
        pending
            .roots
            .insert(tool_output_root.to_path_buf(), keep_path.to_path_buf());
    }
    start_retention_janitor(janitor);
}

#[cfg(test)]
fn schedule_global_retention(tool_output_root: &Path, keep_path: &Path) {
    let janitor = retention_janitor();
    janitor
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .roots
        .insert(tool_output_root.to_path_buf(), keep_path.to_path_buf());
    start_retention_janitor(janitor);
}

fn start_retention_janitor(janitor: &'static RetentionJanitor) {
    if !janitor.running.swap(true, Ordering::AcqRel) {
        tokio::spawn(run_retention_janitor(janitor));
    }
}

async fn run_retention_janitor(janitor: &'static RetentionJanitor) {
    let mut worker_guard = RetentionWorkerGuard {
        janitor,
        armed: true,
    };
    let mut retry_delay = RETENTION_RETRY_INITIAL_DELAY;
    loop {
        let pending = {
            let pending = janitor
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.directories.is_empty() && pending.roots.is_empty() {
                janitor.running.store(false, Ordering::Release);
                worker_guard.armed = false;
                return;
            }
            pending.clone()
        };

        let mut completed_directories = Vec::new();
        let mut should_delay = false;
        for (directory, keep_path) in pending.directories {
            let directory_lock = retention_directory_lock(&directory);
            let Ok(_directory_guard) = directory_lock.try_lock() else {
                should_delay = true;
                continue;
            };
            let cap_satisfied = enforce_local_retention_locked(&directory, &keep_path).await;
            if cap_satisfied {
                completed_directories.push((directory, keep_path));
            } else {
                should_delay = true;
            }
        }

        let mut completed_roots = Vec::new();
        for (tool_output_root, keep_path) in pending.roots {
            if enforce_global_retention_locked(&tool_output_root, &keep_path).await {
                completed_roots.push((tool_output_root, keep_path));
            } else {
                should_delay = true;
            }
        }

        {
            let mut pending = janitor
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (directory, keep_path) in completed_directories {
                if pending.directories.get(&directory) == Some(&keep_path) {
                    pending.directories.remove(&directory);
                }
            }
            for (tool_output_root, keep_path) in completed_roots {
                if pending.roots.get(&tool_output_root) == Some(&keep_path) {
                    pending.roots.remove(&tool_output_root);
                }
            }
        }

        if should_delay {
            tokio::time::sleep(retry_delay).await;
            retry_delay = retry_delay
                .saturating_mul(2)
                .min(RETENTION_RETRY_MAX_DELAY);
        } else {
            retry_delay = RETENTION_RETRY_INITIAL_DELAY;
        }
    }
}

struct RetentionWorkerGuard {
    janitor: &'static RetentionJanitor,
    armed: bool,
}

impl Drop for RetentionWorkerGuard {
    fn drop(&mut self) {
        if self.armed {
            self.janitor.running.store(false, Ordering::Release);
        }
    }
}

fn retention_janitor() -> &'static RetentionJanitor {
    static RETENTION_JANITOR: OnceLock<RetentionJanitor> = OnceLock::new();
    RETENTION_JANITOR.get_or_init(RetentionJanitor::new)
}

fn retention_directory_lock(directory: &Path) -> Arc<Mutex<()>> {
    static DIRECTORY_LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    let locks = DIRECTORY_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(directory).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(directory.to_path_buf(), Arc::downgrade(&lock));
    lock
}

#[cfg(test)]
async fn wait_for_retention_idle() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let janitor = retention_janitor();
            let pending_empty = {
                let pending = janitor
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending.directories.is_empty() && pending.roots.is_empty()
            };
            if pending_empty && !janitor.running.load(Ordering::Acquire) {
                return;
            }
            if !pending_empty && !janitor.running.load(Ordering::Acquire) {
                start_retention_janitor(janitor);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("retention janitor should become idle");
}

#[cfg(test)]
#[path = "command_output_artifact_tests.rs"]
mod hardening_tests;

async fn remove_inactive_output_path(path: PathBuf) -> bool {
    tokio::task::spawn_blocking(move || {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) => return err.kind() == std::io::ErrorKind::NotFound,
        };
        if file.try_lock().is_err() {
            return false;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(err) => err.kind() == std::io::ErrorKind::NotFound,
        }
    })
    .await
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn artifact_retains_exact_bytes_across_chunks() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = create_raw_output_artifact(temp.path(), "thread", b"alpha\0beta\n").await;
        let second = append_raw_output_artifact(&first, b"unicode: \xce\xbb\n").await;

        let RawOutputArtifact::Stored { path, bytes } = second else {
            panic!("expected stored artifact");
        };
        assert_eq!(bytes, 23);
        assert_eq!(
            tokio::fs::read(path).await.expect("read artifact"),
            b"alpha\0beta\nunicode: \xce\xbb\n"
        );
    }

    #[tokio::test]
    async fn replacement_finalizes_background_output_without_duplicates() {
        let temp = tempfile::tempdir().expect("tempdir");
        let initial = create_raw_output_artifact(temp.path(), "thread", b"partial\n").await;
        let appended = append_raw_output_artifact(&initial, b"tail\n").await;
        let final_output = b"partial\ntail\ncomplete\n";
        let replaced = replace_raw_output_artifact(&appended, final_output).await;

        let RawOutputArtifact::Stored { path, bytes } = replaced else {
            panic!("expected stored artifact");
        };
        assert_eq!(bytes, final_output.len() as u64);
        assert_eq!(
            tokio::fs::read(path).await.expect("read artifact"),
            final_output
        );
    }

    #[test]
    fn failed_artifact_preserves_owned_partial_metadata() {
        let artifact = RawOutputArtifact::Failed {
            message: "flush failed".to_string(),
            owned_path: Some(PathBuf::from("C:/codex/tool-output/partial.log")),
            bytes: 17,
        };

        assert!(artifact.render_for_model().contains("partial.log"));
        assert!(artifact.render_for_model().contains("17 bytes"));
    }

    #[tokio::test]
    async fn retention_removes_oldest_inactive_artifacts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut paths = Vec::new();
        for index in 0..(max_retained_artifacts_per_thread() + 3) {
            let artifact = create_raw_output_artifact(
                temp.path(),
                "thread",
                format!("artifact-{index}").as_bytes(),
            )
            .await;
            let RawOutputArtifact::Stored { path, .. } = artifact else {
                panic!("expected stored artifact");
            };
            paths.push(path);
        }
        wait_for_retention_idle().await;

        let mut retained = tokio::fs::read_dir(temp.path().join("tool-output").join("thread"))
            .await
            .expect("read artifact directory");
        let mut retained_count = 0;
        while retained
            .next_entry()
            .await
            .expect("read retained artifact")
            .is_some()
        {
            retained_count += 1;
        }
        assert_eq!(retained_count, max_retained_artifacts_per_thread());
        assert!(!paths[0].exists());
        assert!(paths.last().expect("newest path").exists());
    }
}
