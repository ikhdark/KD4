use std::fmt;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;

use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;

const ARTIFACT_EXPIRED_MESSAGE: &str = "artifact expired or does not belong to this thread; rerun the command if the output is still needed";
const ARTIFACT_WRITING_MESSAGE: &str =
    "artifact is still being written; retry after the command yields or exits";
const EVIDENCE_PROTECTION_EXTENSION: &str = "evidence-protected";
const EVIDENCE_PROTECTION_MARKER_BYTES: &[u8; 34] = b"KD4_EXTERNAL_EVIDENCE_ARTIFACT_V1\n";
const MAX_RAW_OUTPUT_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD: u64 = 256 * 1024 * 1024;
const MAX_RETAINED_ARTIFACT_BYTES_TOTAL: u64 = 2 * 1024 * 1024 * 1024;

pub(crate) fn max_retained_artifacts_per_thread() -> usize {
    128
}

fn max_retained_artifacts_total() -> usize {
    1_024
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolOutputArtifactId(uuid::Uuid);

impl ToolOutputArtifactId {
    fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }
}

impl fmt::Display for ToolOutputArtifactId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ToolOutputArtifactId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        uuid::Uuid::parse_str(value).map(Self)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum RawOutputArtifact {
    Stored {
        id: ToolOutputArtifactId,
        path: PathBuf,
        bytes: u64,
        truncated: bool,
        handle: Arc<File>,
    },
    Failed {
        id: Option<ToolOutputArtifactId>,
        message: String,
        owned_path: Option<PathBuf>,
        bytes: u64,
    },
}

impl PartialEq for RawOutputArtifact {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Stored {
                    id: left_id,
                    path: left_path,
                    bytes: left_bytes,
                    truncated: left_truncated,
                    ..
                },
                Self::Stored {
                    id: right_id,
                    path: right_path,
                    bytes: right_bytes,
                    truncated: right_truncated,
                    ..
                },
            ) => {
                left_id == right_id
                    && left_path == right_path
                    && left_bytes == right_bytes
                    && left_truncated == right_truncated
            }
            (
                Self::Failed {
                    id: left_id,
                    message: left_message,
                    owned_path: left_path,
                    bytes: left_bytes,
                },
                Self::Failed {
                    id: right_id,
                    message: right_message,
                    owned_path: right_path,
                    bytes: right_bytes,
                },
            ) => {
                left_id == right_id
                    && left_message == right_message
                    && left_path == right_path
                    && left_bytes == right_bytes
            }
            _ => false,
        }
    }
}

impl Eq for RawOutputArtifact {}

pub(crate) struct RawOutputArtifactWriter {
    id: Option<ToolOutputArtifactId>,
    path: Option<PathBuf>,
    file: Option<tokio::fs::File>,
    bytes: u64,
    truncated: bool,
    handle: Option<Arc<File>>,
}

impl RawOutputArtifactWriter {
    pub(crate) async fn open(state: Option<&Arc<Mutex<RawOutputArtifact>>>) -> Option<Self> {
        let state = state?;
        let artifact = state.lock().await.clone();
        let RawOutputArtifact::Stored {
            id,
            path,
            bytes,
            truncated,
            handle,
        } = artifact
        else {
            return Some(Self {
                id: None,
                path: None,
                file: None,
                bytes: 0,
                truncated: false,
                handle: None,
            });
        };
        match lock_artifact_handle(&handle, SeekFrom::End(0)) {
            Ok(file) => {
                let file = tokio::fs::File::from_std(file);
                match lock_output_file(file).await {
                    Ok(file) => Some(Self {
                        id: Some(id),
                        path: Some(path),
                        file: Some(file),
                        bytes,
                        truncated,
                        handle: Some(handle),
                    }),
                    Err(err) => {
                        enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), &path)
                            .await;
                        *state.lock().await = RawOutputArtifact::Failed {
                            id: artifact_id_from_path(&path),
                            message: format!(
                                "failed to lock `{}` for streaming: {err}",
                                path.display()
                            ),
                            owned_path: Some(path.clone()),
                            bytes,
                        };
                        Some(Self {
                            id: Some(id),
                            path: Some(path),
                            file: None,
                            bytes,
                            truncated,
                            handle: Some(handle),
                        })
                    }
                }
            }
            Err(err) => {
                enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), &path).await;
                *state.lock().await = RawOutputArtifact::Failed {
                    id: artifact_id_from_path(&path),
                    message: format!("failed to open `{}` for streaming: {err}", path.display()),
                    owned_path: Some(path.clone()),
                    bytes,
                };
                Some(Self {
                    id: Some(id),
                    path: Some(path),
                    file: None,
                    bytes,
                    truncated,
                    handle: Some(handle),
                })
            }
        }
    }

    pub(crate) async fn write_chunk(
        &mut self,
        state: Option<&Arc<Mutex<RawOutputArtifact>>>,
        output: &[u8],
    ) {
        let (Some(state), Some(id), Some(path)) = (state, self.id, self.path.clone()) else {
            return;
        };
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let remaining = MAX_RAW_OUTPUT_ARTIFACT_BYTES.saturating_sub(self.bytes as usize);
        let retained = &output[..output.len().min(remaining)];
        self.truncated |= retained.len() != output.len();
        if let Err(err) = file.write_all(retained).await {
            if let Some(file) = self.file.take() {
                let _ = unlock_output_file(file).await;
            }
            *state.lock().await = failed_with_owned_path(
                path.clone(),
                self.bytes,
                format!("failed to stream `{}`: {err}", path.display()),
            )
            .await;
            return;
        }
        self.bytes = self.bytes.saturating_add(retained.len() as u64);
        let Some(handle) = self.handle.clone() else {
            return;
        };
        *state.lock().await = RawOutputArtifact::Stored {
            id,
            path,
            bytes: self.bytes,
            truncated: self.truncated,
            handle,
        };
    }

    pub(crate) async fn finish(&mut self, state: Option<&Arc<Mutex<RawOutputArtifact>>>) {
        let (Some(state), Some(path), Some(mut file)) =
            (state, self.path.clone(), self.file.take())
        else {
            return;
        };
        if let Err(err) = file.flush().await {
            let _ = unlock_output_file(file).await;
            *state.lock().await = failed_with_owned_path(
                path.clone(),
                self.bytes,
                format!("failed to flush `{}`: {err}", path.display()),
            )
            .await;
            return;
        }
        if let Err(err) = unlock_output_file(file).await {
            *state.lock().await = failed_with_owned_path(
                path.clone(),
                self.bytes,
                format!("failed to unlock `{}`: {err}", path.display()),
            )
            .await;
        }
    }
}

impl Drop for RawOutputArtifactWriter {
    fn drop(&mut self) {
        let Some(file) = self.file.take() else {
            return;
        };
        if let Ok(file) = file.try_into_std() {
            let _ = file.unlock();
        }
    }
}

async fn lock_output_file(file: tokio::fs::File) -> std::io::Result<tokio::fs::File> {
    let file = file.into_std().await;
    file.try_lock()?;
    Ok(tokio::fs::File::from_std(file))
}

async fn unlock_output_file(file: tokio::fs::File) -> std::io::Result<()> {
    let file = file.into_std().await;
    file.unlock()
}

fn lock_artifact_handle(handle: &Arc<File>, position: SeekFrom) -> std::io::Result<File> {
    let mut file = handle.try_clone()?;
    file.seek(position)?;
    Ok(file)
}

impl RawOutputArtifact {
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::Failed {
            id: None,
            message: message.into(),
            owned_path: None,
            bytes: 0,
        }
    }

    pub(crate) fn render_for_model(&self) -> String {
        match self {
            Self::Stored {
                id,
                bytes,
                truncated,
                ..
            } => {
                let suffix = if *truncated {
                    ", truncated at safety limit"
                } else {
                    ""
                };
                format!("Raw output artifact: {id} ({bytes} bytes retained{suffix})")
            }
            Self::Failed {
                id: Some(id),
                bytes,
                ..
            } => format!("Raw output artifact {id} unavailable ({bytes} bytes retained)"),
            Self::Failed { .. } => "Raw output artifact unavailable".to_string(),
        }
    }

    pub(crate) fn model_projection(
        &self,
    ) -> (Option<ToolOutputArtifactId>, Option<u64>, Option<String>) {
        match self {
            Self::Stored { id, bytes, .. } => (Some(*id), Some(*bytes), None),
            Self::Failed { .. } => (
                None,
                None,
                Some("raw output artifact storage failed".to_string()),
            ),
        }
    }

    pub(crate) fn reduction_notice(&self) -> Option<String> {
        let Self::Stored {
            id,
            path,
            truncated,
            ..
        } = self
        else {
            return None;
        };
        if open_regular_artifact(path).is_err() {
            return None;
        }
        let scope = if *truncated {
            "the retained output prefix (the artifact reached its safety limit)"
        } else {
            "full retained output"
        };
        Some(format!(
            "[command output reduced; {scope} is available as artifact {id}.\nUse read_tool_output with that id and a narrow line range.]"
        ))
    }

    pub(crate) fn retained_bytes(&self) -> Option<u64> {
        match self {
            Self::Stored { bytes, .. } => Some(*bytes),
            Self::Failed { .. } => None,
        }
    }

    pub(crate) fn retention_limit_hit(&self) -> bool {
        matches!(
            self,
            Self::Stored {
                truncated: true,
                ..
            }
        )
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

    let id = ToolOutputArtifactId::new();
    let path = directory.join(format!("{id}.log"));
    let retained = &output[..output.len().min(MAX_RAW_OUTPUT_ARTIFACT_BYTES)];
    let truncated = retained.len() != output.len();
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
            if let Err(err) = file.write_all(retained).await {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    0,
                    format!("failed to write `{}`: {err}", path.display()),
                )
                .await;
            }
            if let Err(err) = file.flush().await {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    retained.len() as u64,
                    format!("failed to flush `{}`: {err}", path.display()),
                )
                .await;
            }
            if let Err(err) = file.sync_all().await {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    retained.len() as u64,
                    format!("failed to sync `{}`: {err}", path.display()),
                )
                .await;
            }
            let file = file.into_std().await;
            let _ = file.unlock();
            let handle = Arc::new(file);
            if let Err(err) = sync_parent_directory(&path) {
                return failed_with_owned_path(
                    path.clone(),
                    retained.len() as u64,
                    format!("failed to sync artifact directory: {err}"),
                )
                .await;
            }
            enforce_retention(&directory, &path).await;
            RawOutputArtifact::Stored {
                id,
                path,
                bytes: retained.len() as u64,
                truncated,
                handle,
            }
        }
        Err(err) => {
            enforce_retention(&directory, &path).await;
            RawOutputArtifact::unavailable(format!("failed to create `{}`: {err}", path.display()))
        }
    }
}

pub(crate) async fn create_evidence_output_artifact(
    codex_home: &Path,
    thread_id: &str,
    output: &[u8],
) -> Result<PendingEvidenceArtifact, String> {
    if output.len() > MAX_RAW_OUTPUT_ARTIFACT_BYTES {
        return Err(format!(
            "evidence output exceeds the {MAX_RAW_OUTPUT_ARTIFACT_BYTES} byte artifact safety limit"
        ));
    }
    create_evidence_output_artifact_inner(codex_home, thread_id, output, None).await
}

async fn create_evidence_output_artifact_inner(
    codex_home: &Path,
    thread_id: &str,
    output: &[u8],
    pre_marker_barrier: Option<&tokio::sync::Barrier>,
) -> Result<PendingEvidenceArtifact, String> {
    if output.len() > MAX_RAW_OUTPUT_ARTIFACT_BYTES {
        return Err(format!(
            "evidence output exceeds the {MAX_RAW_OUTPUT_ARTIFACT_BYTES} byte artifact safety limit"
        ));
    }
    let directory = codex_home.join("tool-output").join(thread_id);
    let retention_permit = retention_sweep_permit().await;
    std::fs::create_dir_all(&directory)
        .map_err(|err| format!("failed to create `{}`: {err}", directory.display()))?;
    // Make room before rejecting the reservation. Expired and inactive ordinary command output
    // should not cause durable evidence creation to fail spuriously.
    enforce_retention_locked(&directory, Path::new(""), output.len() as u64, 1).await;
    let thread_bytes = log_bytes_in_directory(&directory).await;
    let global_bytes =
        log_bytes_in_tool_output_root(directory.parent().unwrap_or_else(|| Path::new("."))).await;
    if thread_bytes.saturating_add(output.len() as u64) > MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
        || global_bytes.saturating_add(output.len() as u64) > MAX_RETAINED_ARTIFACT_BYTES_TOTAL
    {
        return Err("evidence artifact retention budget is exhausted".to_string());
    }

    let id = ToolOutputArtifactId::new();
    let path = directory.join(format!("{id}.log"));
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|err| format!("failed to create `{}`: {err}", path.display()))?;
    let cleanup = PendingEvidenceArtifactCleanup::new(path.clone());
    file.try_lock()
        .map_err(|err| format!("failed to lock `{}` for creation: {err}", path.display()))?;
    let mut file = tokio::fs::File::from_std(file);
    file.write_all(output)
        .await
        .map_err(|err| format!("failed to write `{}`: {err}", path.display()))?;
    file.flush()
        .await
        .map_err(|err| format!("failed to flush `{}`: {err}", path.display()))?;
    file.sync_all()
        .await
        .map_err(|err| format!("failed to sync `{}`: {err}", path.display()))?;

    #[cfg(test)]
    if let Some(barrier) = pre_marker_barrier {
        barrier.wait().await;
        barrier.wait().await;
    }
    #[cfg(not(test))]
    let _ = pre_marker_barrier;

    let marker = evidence_protection_path(&path);
    create_new_evidence_protection_marker(&marker)
        .map_err(|err| format!("failed to protect `{}` as evidence: {err}", path.display()))?;
    sync_parent_directory(&path)
        .map_err(|err| format!("failed to sync evidence directory: {err}"))?;

    let file = file.into_std().await;
    let _ = file.unlock();
    let handle = Arc::new(file);
    Ok(PendingEvidenceArtifact {
        id,
        path,
        bytes: output.len() as u64,
        handle,
        cleanup,
        retention_permit: Some(retention_permit),
    })
}

pub(crate) struct PendingEvidenceArtifact {
    id: ToolOutputArtifactId,
    path: PathBuf,
    bytes: u64,
    handle: Arc<File>,
    cleanup: PendingEvidenceArtifactCleanup,
    retention_permit: Option<SemaphorePermit<'static>>,
}

impl PendingEvidenceArtifact {
    pub(crate) fn id(&self) -> ToolOutputArtifactId {
        self.id
    }

    pub(crate) fn mark_durable(mut self) -> RawOutputArtifact {
        self.cleanup.disarm();
        drop(self.retention_permit.take());
        RawOutputArtifact::Stored {
            id: self.id,
            path: self.path.clone(),
            bytes: self.bytes,
            truncated: false,
            handle: self.handle.clone(),
        }
    }
}

struct PendingEvidenceArtifactCleanup {
    path: PathBuf,
    armed: bool,
}

impl PendingEvidenceArtifactCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingEvidenceArtifactCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(evidence_protection_path(&self.path));
    }
}

fn create_new_evidence_protection_marker(marker: &Path) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(marker)?;
    if let Err(err) = file
        .write_all(EVIDENCE_PROTECTION_MARKER_BYTES)
        .and_then(|()| file.sync_all())
    {
        drop(file);
        let _ = std::fs::remove_file(marker);
        return Err(err);
    }
    Ok(())
}

pub(crate) async fn delete_evidence_artifact(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
) -> std::io::Result<()> {
    let id = artifact_id.parse::<ToolOutputArtifactId>().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid artifact id")
    })?;
    if id.to_string() != artifact_id {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "non-canonical artifact id",
        ));
    }
    let path = codex_home
        .join("tool-output")
        .join(thread_id)
        .join(format!("{id}.log"));
    let marker = evidence_protection_path(&path);
    let _retention_permit = retention_sweep_permit().await;
    if !remove_inactive_output_path(path).await {
        return Err(std::io::Error::other(
            "evidence artifact is still active and could not be deleted",
        ));
    }
    sync_parent_directory(&marker)?;
    match tokio::fs::remove_file(&marker).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    sync_parent_directory(&marker)?;
    Ok(())
}

pub(crate) async fn reconcile_evidence_artifact_protection(
    codex_home: &Path,
    thread_id: &str,
    referenced_artifact_ids: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeSet<String> {
    let directory = codex_home.join("tool-output").join(thread_id);
    let _retention_permit = retention_sweep_permit().await;
    let mut live_artifact_ids = std::collections::BTreeSet::new();
    for artifact_id in referenced_artifact_ids {
        let Ok(id) = artifact_id.parse::<ToolOutputArtifactId>() else {
            continue;
        };
        if id.to_string() != *artifact_id {
            continue;
        }
        let path = directory.join(format!("{id}.log"));
        let marker = evidence_protection_path(&path);
        let marker_is_valid = tokio::task::spawn_blocking(move || {
            let Ok((_log_file, _)) = open_regular_artifact(&path) else {
                return false;
            };
            match std::fs::symlink_metadata(&marker) {
                Ok(_) => evidence_protection_marker_is_valid(&marker),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    create_new_evidence_protection_marker(&marker).is_ok()
                }
                Err(_) => false,
            }
        })
        .await
        .unwrap_or(false);
        if marker_is_valid {
            live_artifact_ids.insert(artifact_id.clone());
        }
    }

    let Ok(mut entries) = tokio::fs::read_dir(&directory).await else {
        return live_artifact_ids;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let marker = entry.path();
        if marker.extension().and_then(|extension| extension.to_str())
            != Some(EVIDENCE_PROTECTION_EXTENSION)
        {
            continue;
        }
        let Some(artifact_id) = marker.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if live_artifact_ids.contains(artifact_id) {
            continue;
        }
        let path = directory.join(format!("{artifact_id}.log"));
        let _ = tokio::fs::remove_file(marker).await;
        let _ = remove_inactive_output_path(path).await;
    }
    live_artifact_ids
}

fn evidence_protection_marker_is_valid(marker: &Path) -> bool {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    configure_no_follow_open(&mut options);
    let Ok(mut file) = options.open(marker) else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    if !metadata.is_file() || metadata_is_reparse_point(&metadata) {
        return false;
    }
    if metadata.len() != EVIDENCE_PROTECTION_MARKER_BYTES.len() as u64 {
        return false;
    }
    let mut bytes = [0_u8; EVIDENCE_PROTECTION_MARKER_BYTES.len()];
    if file.read_exact(&mut bytes).is_err() || bytes != *EVIDENCE_PROTECTION_MARKER_BYTES {
        return false;
    }
    let mut trailing = [0_u8; 1];
    matches!(file.read(&mut trailing), Ok(0))
}

pub(crate) async fn append_raw_output_artifact(
    artifact: &RawOutputArtifact,
    output: &[u8],
) -> RawOutputArtifact {
    let RawOutputArtifact::Stored {
        id,
        path,
        bytes,
        truncated,
        handle,
    } = artifact
    else {
        return artifact.clone();
    };

    match lock_artifact_handle(handle, SeekFrom::End(0)) {
        Ok(file) => {
            let file = tokio::fs::File::from_std(file);
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
            let remaining = MAX_RAW_OUTPUT_ARTIFACT_BYTES.saturating_sub(*bytes as usize);
            let retained = &output[..output.len().min(remaining)];
            let truncated = *truncated || retained.len() != output.len();
            if let Err(err) = file.write_all(retained).await {
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
                    (*bytes).saturating_add(retained.len() as u64),
                    format!("failed to flush `{}`: {err}", path.display()),
                )
                .await;
            }
            let metadata = file.metadata().await;
            if let Err(err) = unlock_output_file(file).await {
                return failed_with_owned_path(
                    path.clone(),
                    (*bytes).saturating_add(retained.len() as u64),
                    format!("failed to unlock `{}` after append: {err}", path.display()),
                )
                .await;
            }
            match metadata {
                Ok(metadata) => {
                    enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), path).await;
                    RawOutputArtifact::Stored {
                        id: *id,
                        path: path.clone(),
                        bytes: metadata.len(),
                        truncated,
                        handle: handle.clone(),
                    }
                }
                Err(err) => {
                    failed_with_owned_path(
                        path.clone(),
                        (*bytes).saturating_add(retained.len() as u64),
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
    let RawOutputArtifact::Stored {
        id,
        path,
        bytes,
        handle,
        ..
    } = artifact
    else {
        return artifact.clone();
    };

    match lock_artifact_handle(handle, SeekFrom::Start(0)) {
        Ok(file) => {
            let file = tokio::fs::File::from_std(file);
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
            let retained = &output[..output.len().min(MAX_RAW_OUTPUT_ARTIFACT_BYTES)];
            let truncated = retained.len() != output.len();
            if let Err(err) = file.set_len(0).await {
                let _ = unlock_output_file(file).await;
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
            if let Err(err) = file.write_all(retained).await {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    0,
                    format!("failed to replace `{}`: {err}", path.display()),
                )
                .await;
            }
            if let Err(err) = file.flush().await {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    retained.len() as u64,
                    format!("failed to flush `{}`: {err}", path.display()),
                )
                .await;
            }
            if let Err(err) = unlock_output_file(file).await {
                return failed_with_owned_path(
                    path.clone(),
                    retained.len() as u64,
                    format!(
                        "failed to unlock `{}` after replacement: {err}",
                        path.display()
                    ),
                )
                .await;
            }
            enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), path).await;
            RawOutputArtifact::Stored {
                id: *id,
                path: path.clone(),
                bytes: retained.len() as u64,
                truncated,
                handle: handle.clone(),
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
    enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), &path).await;
    RawOutputArtifact::Failed {
        id: artifact_id_from_path(&path),
        message,
        owned_path: Some(path),
        bytes,
    }
}

fn artifact_id_from_path(path: &Path) -> Option<ToolOutputArtifactId> {
    path.file_stem()?.to_str()?.parse().ok()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadToolOutputError {
    InvalidArtifactId,
    InvalidRange(String),
    Expired,
    StillWriting,
    Io(String),
}

impl ReadToolOutputError {
    pub(crate) fn for_model(&self) -> String {
        match self {
            Self::InvalidArtifactId => "artifact_id must be a UUID".to_string(),
            Self::InvalidRange(message) | Self::Io(message) => message.clone(),
            Self::Expired => ARTIFACT_EXPIRED_MESSAGE.to_string(),
            Self::StillWriting => ARTIFACT_WRITING_MESSAGE.to_string(),
        }
    }
}

pub(crate) async fn read_tool_output_artifact(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    start_line: usize,
    end_line: usize,
    max_bytes: usize,
) -> Result<String, ReadToolOutputError> {
    let id = artifact_id
        .parse::<ToolOutputArtifactId>()
        .map_err(|_| ReadToolOutputError::InvalidArtifactId)?;
    if id.to_string() != artifact_id {
        return Err(ReadToolOutputError::InvalidArtifactId);
    }
    validate_read_range(start_line, end_line, max_bytes)?;
    let path = codex_home
        .join("tool-output")
        .join(thread_id)
        .join(format!("{id}.log"));
    tokio::task::spawn_blocking(move || {
        read_tool_output_artifact_blocking(&path, id, start_line, end_line, max_bytes)
    })
    .await
    .map_err(|err| ReadToolOutputError::Io(format!("failed to read artifact: {err}")))?
}

fn validate_read_range(
    start_line: usize,
    end_line: usize,
    max_bytes: usize,
) -> Result<(), ReadToolOutputError> {
    if start_line == 0 {
        return Err(ReadToolOutputError::InvalidRange(
            "start_line must be at least 1".to_string(),
        ));
    }
    if end_line < start_line {
        return Err(ReadToolOutputError::InvalidRange(
            "end_line must be greater than or equal to start_line".to_string(),
        ));
    }
    if end_line - start_line >= 2_000 {
        return Err(ReadToolOutputError::InvalidRange(
            "requested line span must not exceed 2000 lines".to_string(),
        ));
    }
    if max_bytes == 0 || max_bytes > 16_384 {
        return Err(ReadToolOutputError::InvalidRange(
            "max_bytes must be between 1 and 16384".to_string(),
        ));
    }
    Ok(())
}

fn read_tool_output_artifact_blocking(
    path: &Path,
    id: ToolOutputArtifactId,
    start_line: usize,
    end_line: usize,
    max_bytes: usize,
) -> Result<String, ReadToolOutputError> {
    let (mut file, total_bytes) = open_regular_artifact(path)?;
    file.try_lock_shared()
        .map_err(|_| ReadToolOutputError::StillWriting)?;

    let mut retained = Vec::with_capacity(max_bytes.min(16 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut current_line = 1_usize;
    let mut last_line = None;
    let mut clamped = false;
    let mut pending_cr = false;
    let mut done = false;

    while !done {
        let count = file
            .read(&mut buffer)
            .map_err(|err| ReadToolOutputError::Io(format!("failed to read artifact: {err}")))?;
        if count == 0 {
            if pending_cr && current_line >= start_line && current_line <= end_line {
                push_bounded(&mut retained, b'\r', max_bytes, &mut clamped);
                last_line = Some(current_line);
            }
            break;
        }

        for &byte in &buffer[..count] {
            if pending_cr {
                if byte == b'\n' {
                    if current_line >= start_line && current_line <= end_line {
                        push_bounded(&mut retained, b'\n', max_bytes, &mut clamped);
                        last_line = Some(current_line);
                    }
                    pending_cr = false;
                    if current_line == end_line {
                        done = true;
                        break;
                    }
                    current_line += 1;
                    continue;
                }
                if current_line >= start_line && current_line <= end_line {
                    push_bounded(&mut retained, b'\r', max_bytes, &mut clamped);
                    last_line = Some(current_line);
                }
                pending_cr = false;
            }

            if byte == b'\r' {
                pending_cr = true;
            } else if byte == b'\n' {
                if current_line >= start_line && current_line <= end_line {
                    push_bounded(&mut retained, b'\n', max_bytes, &mut clamped);
                    last_line = Some(current_line);
                }
                if current_line == end_line {
                    done = true;
                    break;
                }
                current_line += 1;
            } else if current_line >= start_line && current_line <= end_line {
                push_bounded(&mut retained, byte, max_bytes, &mut clamped);
                last_line = Some(current_line);
            }
        }
    }

    let (text, invalid_utf8) = match String::from_utf8(retained) {
        Ok(text) => (text, false),
        Err(err) => (String::from_utf8_lossy(err.as_bytes()).into_owned(), true),
    };
    let lines = match last_line {
        Some(last) => format!("{start_line}–{last}"),
        None => "none".to_string(),
    };
    let mut rendered =
        format!("artifact {id}, lines {lines}, {total_bytes} retained bytes\n{text}");
    if invalid_utf8 {
        rendered.push_str("\n[invalid UTF-8 replaced]");
    }
    if clamped {
        rendered.push_str("\n[range clamped at byte limit]");
    }
    Ok(rendered)
}

fn open_regular_artifact(path: &Path) -> Result<(File, u64), ReadToolOutputError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    configure_no_follow_open(&mut options);
    let file = options.open(path).map_err(map_artifact_open_error)?;
    let metadata = file
        .metadata()
        .map_err(|err| ReadToolOutputError::Io(format!("failed to inspect artifact: {err}")))?;
    if !metadata.is_file() || metadata_is_reparse_point(&metadata) {
        return Err(ReadToolOutputError::Expired);
    }
    Ok((file, metadata.len()))
}

fn map_artifact_open_error(error: std::io::Error) -> ReadToolOutputError {
    if error.kind() == std::io::ErrorKind::NotFound
        || error.raw_os_error().is_some_and(is_no_follow_error)
    {
        ReadToolOutputError::Expired
    } else {
        ReadToolOutputError::Io(format!("failed to open artifact: {error}"))
    }
}

#[cfg(unix)]
fn configure_no_follow_open(options: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
}

#[cfg(windows)]
fn configure_no_follow_open(options: &mut std::fs::OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(unix, windows)))]
fn configure_no_follow_open(_options: &mut std::fs::OpenOptions) {}

#[cfg(unix)]
fn is_no_follow_error(raw_os_error: i32) -> bool {
    raw_os_error == libc::ELOOP
}

#[cfg(not(unix))]
fn is_no_follow_error(_raw_os_error: i32) -> bool {
    false
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn push_bounded(output: &mut Vec<u8>, byte: u8, max_bytes: usize, clamped: &mut bool) {
    if output.len() < max_bytes {
        output.push(byte);
    } else {
        *clamped = true;
    }
}

fn evidence_protection_path(artifact_path: &Path) -> PathBuf {
    artifact_path.with_extension(EVIDENCE_PROTECTION_EXTENSION)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> std::io::Result<()> {
    // Portable Rust APIs cannot open directory handles on every supported platform. Individual
    // files are still synced; directory durability is requested where that operation is exposed.
    Ok(())
}

async fn evidence_artifact_is_protected(artifact_path: &Path) -> bool {
    let marker = evidence_protection_path(artifact_path);
    tokio::task::spawn_blocking(move || evidence_protection_marker_is_valid(&marker))
        .await
        .unwrap_or(false)
}

async fn enforce_retention(directory: &Path, keep_path: &Path) {
    let _retention_permit = retention_sweep_permit().await;
    enforce_retention_locked(directory, keep_path, 0, 0).await;
}

async fn enforce_retention_locked(
    directory: &Path,
    keep_path: &Path,
    reserved_bytes: u64,
    reserved_artifacts: usize,
) {
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return;
    };
    let mut paths = Vec::new();
    let mut total_bytes = 0_u64;
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) | Err(_) => break,
        };
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("log") {
            let bytes = entry.metadata().await.map_or(0, |metadata| metadata.len());
            total_bytes = total_bytes.saturating_add(bytes);
            paths.push((
                path.clone(),
                bytes,
                evidence_artifact_is_protected(&path).await,
            ));
        }
    }
    paths.sort_unstable_by(|(left, ..), (right, ..)| left.cmp(right));

    let mut remove_count = paths
        .iter()
        .filter(|(_, _, protected)| !protected)
        .count()
        .saturating_add(reserved_artifacts)
        .saturating_sub(max_retained_artifacts_per_thread());
    for (path, bytes, protected) in paths {
        if remove_count == 0
            && total_bytes.saturating_add(reserved_bytes) <= MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
        {
            break;
        }
        if path == keep_path || protected {
            continue;
        }
        if remove_inactive_output_path(path).await {
            remove_count = remove_count.saturating_sub(1);
            total_bytes = total_bytes.saturating_sub(bytes);
        }
    }

    if let Some(tool_output_root) = directory.parent() {
        enforce_global_retention_locked(
            tool_output_root,
            keep_path,
            reserved_bytes,
            reserved_artifacts,
        )
        .await;
    }
}

#[cfg(test)]
async fn enforce_global_retention(tool_output_root: &Path, keep_path: &Path) {
    let _retention_permit = retention_sweep_permit().await;
    enforce_global_retention_locked(tool_output_root, keep_path, 0, 0).await;
}

async fn enforce_global_retention_locked(
    tool_output_root: &Path,
    keep_path: &Path,
    reserved_bytes: u64,
    reserved_artifacts: usize,
) {
    let Ok(mut thread_directories) = tokio::fs::read_dir(tool_output_root).await else {
        return;
    };
    let mut paths = Vec::new();
    let mut total_bytes = 0_u64;
    loop {
        let thread_directory = match thread_directories.next_entry().await {
            Ok(Some(entry)) => entry.path(),
            Ok(None) | Err(_) => break,
        };
        let Ok(mut entries) = tokio::fs::read_dir(&thread_directory).await else {
            continue;
        };
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) | Err(_) => break,
            };
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("log") {
                let metadata = entry.metadata().await.ok();
                let bytes = metadata.as_ref().map_or(0, std::fs::Metadata::len);
                total_bytes = total_bytes.saturating_add(bytes);
                let modified = metadata
                    .and_then(|metadata| metadata.modified().ok())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                paths.push((
                    modified,
                    path.clone(),
                    bytes,
                    evidence_artifact_is_protected(&path).await,
                ));
            }
        }
    }
    paths.sort_unstable_by(|(left_time, left_path, ..), (right_time, right_path, ..)| {
        left_time
            .cmp(right_time)
            .then_with(|| left_path.cmp(right_path))
    });
    let mut remove_count = paths
        .iter()
        .filter(|(_, _, _, protected)| !protected)
        .count()
        .saturating_add(reserved_artifacts)
        .saturating_sub(max_retained_artifacts_total());
    for (_, path, bytes, protected) in paths {
        if remove_count == 0
            && total_bytes.saturating_add(reserved_bytes) <= MAX_RETAINED_ARTIFACT_BYTES_TOTAL
        {
            break;
        }
        if !protected && path != keep_path && remove_inactive_output_path(path).await {
            remove_count = remove_count.saturating_sub(1);
            total_bytes = total_bytes.saturating_sub(bytes);
        }
    }
}

async fn log_bytes_in_directory(directory: &Path) -> u64 {
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return 0;
    };
    let mut bytes = 0_u64;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.path().extension().and_then(|value| value.to_str()) == Some("log") {
            bytes = bytes.saturating_add(entry.metadata().await.map_or(0, |meta| meta.len()));
        }
    }
    bytes
}

async fn log_bytes_in_tool_output_root(root: &Path) -> u64 {
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return 0;
    };
    let mut bytes = 0_u64;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry
            .metadata()
            .await
            .is_ok_and(|metadata| metadata.is_dir())
        {
            bytes = bytes.saturating_add(log_bytes_in_directory(&entry.path()).await);
        }
    }
    bytes
}

async fn retention_sweep_permit() -> SemaphorePermit<'static> {
    match retention_sweep_semaphore().acquire().await {
        Ok(permit) => permit,
        Err(_) => unreachable!("the process-wide retention sweep semaphore is never closed"),
    }
}

fn retention_sweep_semaphore() -> &'static Semaphore {
    static RETENTION_SWEEP_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();
    RETENTION_SWEEP_SEMAPHORE.get_or_init(|| Semaphore::new(1))
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

        let RawOutputArtifact::Stored { path, bytes, .. } = second else {
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

        let RawOutputArtifact::Stored { path, bytes, .. } = replaced else {
            panic!("expected stored artifact");
        };
        assert_eq!(bytes, final_output.len() as u64);
        assert_eq!(
            tokio::fs::read(path).await.expect("read artifact"),
            final_output
        );
    }

    #[tokio::test]
    async fn artifact_creation_truncates_at_the_per_artifact_byte_limit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let oversized = vec![b'x'; MAX_RAW_OUTPUT_ARTIFACT_BYTES + 1];

        let artifact = create_raw_output_artifact(temp.path(), "thread", &oversized).await;

        let RawOutputArtifact::Stored {
            path,
            bytes,
            truncated,
            ..
        } = artifact
        else {
            panic!("expected stored artifact");
        };
        assert_eq!(bytes, MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64);
        assert!(truncated);
        assert_eq!(
            tokio::fs::metadata(path)
                .await
                .expect("artifact metadata")
                .len(),
            MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64
        );
    }

    #[tokio::test]
    async fn append_uses_the_original_handle_after_path_substitution() {
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact = create_raw_output_artifact(temp.path(), "thread", b"original").await;
        let RawOutputArtifact::Stored { path, .. } = &artifact else {
            panic!("expected stored artifact");
        };
        let displaced = path.with_extension("displaced");
        std::fs::rename(path, &displaced).expect("displace artifact path");
        std::fs::write(path, b"substitute").expect("write substitute path");

        let appended = append_raw_output_artifact(&artifact, b"-tail").await;

        assert!(matches!(appended, RawOutputArtifact::Stored { .. }));
        assert_eq!(std::fs::read(path).expect("read substitute"), b"substitute");
        assert_eq!(
            std::fs::read(displaced).expect("read original handle target"),
            b"original-tail"
        );
    }

    #[test]
    fn failed_artifact_preserves_owned_partial_metadata() {
        let artifact = RawOutputArtifact::Failed {
            id: None,
            message: "flush failed".to_string(),
            owned_path: Some(PathBuf::from("C:/codex/tool-output/partial.log")),
            bytes: 17,
        };

        assert!(!artifact.render_for_model().contains("partial.log"));
        assert!(matches!(
            artifact,
            RawOutputArtifact::Failed { bytes: 17, .. }
        ));
    }

    #[test]
    fn evidence_protection_marker_rejects_oversized_contents() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marker = temp.path().join("artifact.evidence-protected");
        create_new_evidence_protection_marker(&marker).expect("create evidence marker");
        assert!(evidence_protection_marker_is_valid(&marker));

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&marker)
            .expect("open evidence marker for append");
        file.write_all(b"x").expect("append trailing marker byte");

        assert!(!evidence_protection_marker_is_valid(&marker));
    }

    fn stored_id(artifact: &RawOutputArtifact) -> ToolOutputArtifactId {
        let RawOutputArtifact::Stored { id, .. } = artifact else {
            panic!("expected stored artifact");
        };
        *id
    }

    #[tokio::test]
    async fn read_returns_exact_range_and_treats_crlf_as_one_break() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bytes = b"one\r\ntwo\r\nthree\r\nfour\r\n";
        let artifact = create_raw_output_artifact(temp.path(), "thread", bytes).await;
        let id = stored_id(&artifact);

        let output =
            read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 2, 3, 16_384)
                .await
                .expect("read artifact");

        assert_eq!(
            output,
            format!(
                "artifact {id}, lines 2–3, {} retained bytes\ntwo\nthree\n",
                bytes.len()
            )
        );

        let through_eof =
            read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 1, 200, 16_384)
                .await
                .expect("read through eof");
        assert!(through_eof.starts_with(&format!(
            "artifact {id}, lines 1–4, {} retained bytes\n",
            bytes.len()
        )));
    }

    #[tokio::test]
    async fn read_clamps_at_byte_limit_with_trailer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact = create_raw_output_artifact(temp.path(), "thread", b"abcdef\n").await;
        let id = stored_id(&artifact);

        let output = read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 1, 1, 3)
            .await
            .expect("read artifact");

        assert!(output.contains("\nabc\n[range clamped at byte limit]"));
    }

    #[tokio::test]
    async fn read_rejects_invalid_uuid_and_traversal() {
        let temp = tempfile::tempdir().expect("tempdir");
        for invalid in ["not-a-uuid", "../019fa782-f8e1-7533-a3f7-60d3f9a42997"] {
            let error = read_tool_output_artifact(temp.path(), "thread", invalid, 1, 1, 16_384)
                .await
                .expect_err("invalid id should fail");
            assert_eq!(error, ReadToolOutputError::InvalidArtifactId);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_rejects_uuid_named_symlink_outside_thread_directory() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let thread_directory = temp.path().join("tool-output").join("thread");
        std::fs::create_dir_all(&thread_directory).expect("create thread directory");
        let outside = temp.path().join("outside.log");
        std::fs::write(&outside, b"outside secret\n").expect("write outside artifact");
        let id = ToolOutputArtifactId::new();
        symlink(&outside, thread_directory.join(format!("{id}.log"))).expect("create symlink");

        let error = read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 1, 1, 16_384)
            .await
            .expect_err("symlink artifact should fail");

        assert_eq!(error, ReadToolOutputError::Expired);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn read_rejects_uuid_named_reparse_point_outside_thread_directory() {
        use std::os::windows::fs::symlink_file;

        let temp = tempfile::tempdir().expect("tempdir");
        let thread_directory = temp.path().join("tool-output").join("thread");
        std::fs::create_dir_all(&thread_directory).expect("create thread directory");
        let outside = temp.path().join("outside.log");
        std::fs::write(&outside, b"outside secret\n").expect("write outside artifact");
        let id = ToolOutputArtifactId::new();
        if let Err(error) = symlink_file(&outside, thread_directory.join(format!("{id}.log"))) {
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(1314)
            {
                return;
            }
            panic!("create file reparse point: {error}");
        }

        let error = read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 1, 1, 16_384)
            .await
            .expect_err("reparse artifact should fail");

        assert_eq!(error, ReadToolOutputError::Expired);
    }

    #[tokio::test]
    async fn read_rejects_artifact_from_other_thread() {
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact = create_raw_output_artifact(temp.path(), "thread-a", b"secret\n").await;
        let id = stored_id(&artifact);

        let error =
            read_tool_output_artifact(temp.path(), "thread-b", &id.to_string(), 1, 1, 16_384)
                .await
                .expect_err("cross-thread read should fail");

        assert_eq!(error, ReadToolOutputError::Expired);
        assert_eq!(error.for_model(), ARTIFACT_EXPIRED_MESSAGE);
    }

    #[tokio::test]
    async fn read_reports_evicted_artifact() {
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact = create_raw_output_artifact(temp.path(), "thread", b"old\n").await;
        let id = stored_id(&artifact);
        let RawOutputArtifact::Stored { path, .. } = artifact else {
            unreachable!();
        };
        tokio::fs::remove_file(path).await.expect("evict artifact");

        let error = read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 1, 1, 16_384)
            .await
            .expect_err("evicted artifact should fail");

        assert_eq!(error, ReadToolOutputError::Expired);
    }

    #[tokio::test]
    async fn read_replaces_invalid_utf8_and_adds_notice() {
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact = create_raw_output_artifact(temp.path(), "thread", b"bad:\xff\n").await;
        let id = stored_id(&artifact);

        let output =
            read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 1, 1, 16_384)
                .await
                .expect("read artifact");

        assert!(output.contains("bad:\u{fffd}"));
        assert!(output.ends_with("[invalid UTF-8 replaced]"));
    }

    #[tokio::test]
    async fn read_reports_active_writer_lock_without_blocking() {
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact = create_raw_output_artifact(temp.path(), "thread", b"partial\n").await;
        let id = stored_id(&artifact);
        let state = Arc::new(Mutex::new(artifact));
        let _writer = RawOutputArtifactWriter::open(Some(&state))
            .await
            .expect("writer state");

        let error = read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 1, 1, 16_384)
            .await
            .expect_err("locked artifact should not be read");

        assert_eq!(error, ReadToolOutputError::StillWriting);
        assert_eq!(error.for_model(), ARTIFACT_WRITING_MESSAGE);
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
