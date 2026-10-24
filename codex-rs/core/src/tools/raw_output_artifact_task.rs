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

        if let Some(task) = &mut self.task {
            task.write_chunk(bytes);
        }
    }

    pub(crate) fn mark_output_gap(&mut self) {

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

        attached: false,
    }));
    let output = LEGACY_RAW_CAPTURE.scope(Arc::clone(&capture), future).await;
    // Never hold the reader's synchronous lock while waiting for storage.
    let (task, attached) = {
        let mut capture = capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (capture.task.take(), capture.attached)
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

    (
        output,
        CapturedShellOutput {
            artifact,
            canonical: None,
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
        let opened = (|| {
            let residency = super::command_output_artifact::artifact_residency_file(&codex_home, &thread_id, false)?;
            std::fs::create_dir_all(path.parent().ok_or_else(|| std::io::Error::other("missing log parent"))?)?;
            let file = OpenOptions::new().create_new(true).read(true).write(true).open(&path)?;
            Ok::<_, std::io::Error>((file, residency))
        })();
        let (writer, handle, residency, failure) = match opened.and_then(|(file, residency)| {
            let handle = Arc::new(file.try_clone()?);
            Ok((BufWriter::with_capacity(64 * 1024, file), handle, residency))
        }) {
            Ok((writer, handle, residency)) => (Some(writer), Some(handle), Some(residency), None),
            Err(error) => (None, None, None, Some(error.to_string())),
        };
        let mut capture = Self { state, ready, writer, handle, _residency: residency, id, path, bytes: 0, digest: Sha256::new(),
            incomplete: false, failure, finished: false };
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
        RawOutputArtifact::Failed { id: Some(self.id), message, owned_path: Some(self.path.clone()), bytes: self.bytes }
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
            if let Ok(bytes) = serde_json::to_vec(&status) {
                let _ = codex_file_system::write_bytes_atomically(&self.path.with_extension("status"), &bytes);
            }
            if let Ok(mut state) = self.state.try_lock() {
                *state = self.failed("command output capture ended before finalization".into());
            }
            self.ready.cancel();
        }
    }
}

/// Return None for an old/indexed artifact so its existing recovery owner handles it.
/// Reads are thread-confined, line-only, bounded-memory and never mutate saved logs.
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
        for selector in &selectors {
            let ToolOutputSelector::Lines { start, end } = selector else {
                return Err(super::ReadToolOutputError::InvalidRange(
                    "Command logs support line ranges only; use selectors:[{kind:lines,start:1,end:200}]. Source snapshots retain advanced selectors.".into()));
            };
            if *start == 0 || end < start {
                return Err(super::ReadToolOutputError::InvalidRange("expected 1-based start <= end".into()));
            }
            offsets.insert(*start, None);
        }
        let io_error = |error: std::io::Error| super::ReadToolOutputError::Io(error.to_string());
        let mut reader = BufReader::new(File::open(&path).map_err(io_error)?);
        let mut scanned_line = 1usize;
        let mut offset = 0u64;
        // Index only requested starts, in one bounded-memory prefix scan.
        // Delivery remains in caller order, including its shared byte budget.
        for (start, position) in &mut offsets {
            while scanned_line < *start {
                let available = reader.fill_buf().map_err(io_error)?;
                if available.is_empty() { break; }
                let newline = available.iter().position(|byte| *byte == b'\n');
                let count = newline.map_or(available.len(), |index| index + 1);
                reader.consume(count);
                offset += count as u64;
                if newline.is_some() { scanned_line += 1; }
            }
            if scanned_line == *start { *position = Some(offset); }
        }
        for selector in selectors {
            let ToolOutputSelector::Lines { start, end } = selector else {
                return Err(super::ReadToolOutputError::InvalidRange(
                    "Command logs support line ranges only; use selectors:[{kind:lines,start:1,end:200}]. Source snapshots retain advanced selectors.".into()));
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
            let mut error = None;
            while position.is_some() && line <= end {
                let mut bytes = Vec::new();
                let count = Read::by_ref(&mut reader).take(remaining.saturating_add(1) as u64)
                    .read_until(b'\n', &mut bytes)
                    .map_err(|error| super::ReadToolOutputError::Io(error.to_string()))?;
                if count == 0 { break; }
                if count > remaining {
                    if selection.is_empty() && remaining == max_bytes {
                        error = Some(format!(
                            "Line {line} exceeds this response budget. Request a larger max_bytes in Code Mode, or read the saved log directly: {}", path.display()
                        ));
                    } else {
                        next_line = Some(line);
                    }
                    complete = false;
                    break;
                }
                remaining -= count;
                selection.extend_from_slice(&bytes);
                line += 1;
            }
            let mut item = serde_json::json!({
                "selector":{"kind":"lines","start":start,"end":end},
                "status":if error.is_some() { "invalid" } else { "ok" }, "complete":complete,
            });
            if let Some(error) = error { item["message"] = error.into(); }
            match String::from_utf8(selection) {
                Ok(text) => item["text"] = text.into(),
                Err(error) => item["data_base64"] =
                    base64::engine::general_purpose::STANDARD.encode(error.into_bytes()).into(),
            }
            if let Some(start) = next_line {
                item["continuation"] = serde_json::json!({"kind":"lines","start":start,"end":end});
            }
            results.push(item);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn command_lines_preserve_batch_progress_and_skip_large_preceding_lines() {
        let home = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(RawOutputArtifact::pending(home.path(), "lines")));
        let mut task = RawOutputArtifactTask::spawn(Some(state.clone()), CancellationToken::new()).unwrap();
        task.write_chunk(&vec![b'x'; 32 * 1024]);
        task.write_chunk(b"\nfirst\nsecond\n");
        task.finish().await;
        let id = state.lock().await.artifact_id().unwrap().to_string();
        let selected = read_command_log(home.path(), "lines", &id,
            &[ToolOutputSelector::Lines { start:2, end:3 }], 6).await.unwrap().unwrap();
        assert_eq!(selected["results"][0]["text"], "first\n");
        assert_eq!(selected["results"][0]["continuation"]["start"], 3);
        assert_eq!(selected["complete"], false);
        let batch = read_command_log(home.path(), "lines", &id,
            &[ToolOutputSelector::Lines { start:1, end:1 }, ToolOutputSelector::Lines { start:2, end:2 }],
            8).await.unwrap().unwrap();
        assert_eq!(batch["results"][0]["status"], "invalid");
        assert_eq!(batch["results"][1]["text"], "first\n");
        assert_eq!(batch["retained_artifact_complete"], true);
        assert_eq!(batch["canonical_bytes"], 32 * 1024 + 14);
    }
    #[tokio::test]
    async fn full_log_exceeds_old_cap_and_remains_line_readable() {
        let home = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(RawOutputArtifact::pending(home.path(), "full")));
        let ready = CancellationToken::new();
        let mut task = RawOutputArtifactTask::spawn(Some(state.clone()), ready.clone()).unwrap();
        let line = vec![b'x'; 1023].into_iter().chain([b'\n']).collect::<Vec<_>>();
        for _ in 0..17_000 { task.write_chunk(&line); }
        task.write_chunk("final é\r\n".as_bytes());
        task.finish().await;
        assert!(ready.is_cancelled());
        let artifact = state.lock().await.clone();
        assert!(artifact.capture_finalized_complete());
        assert_eq!(artifact.retained_bytes(), Some(17_000 * 1024 + "final é\r\n".len() as u64));
        let value = read_command_log(home.path(), "full", &artifact.artifact_id().unwrap().to_string(),
            &[ToolOutputSelector::Lines { start:17_001, end:17_001 }], 4096).await.unwrap().unwrap();
        assert_eq!(value["results"][0]["text"], "final é\r\n");
        assert_eq!(value["retained_artifact_complete"], true);
    }
    #[tokio::test]
    async fn gaps_and_abandonment_never_claim_complete_capture() {
        let home = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(RawOutputArtifact::pending(home.path(), "gap")));
        let mut task = RawOutputArtifactTask::spawn(Some(state.clone()), CancellationToken::new()).unwrap();
        task.write_chunk(b"before\n");
        task.mark_output_gap();
        task.write_chunk(b"after\n");
        task.finish().await;
        let artifact = state.lock().await.clone();
        assert!(!artifact.capture_finalized_complete());
        let value = read_command_log(home.path(), "gap", &artifact.artifact_id().unwrap().to_string(),
            &[ToolOutputSelector::Lines { start:1, end:2 }], 4096).await.unwrap().unwrap();
        assert_eq!(value["results"][0]["text"], "before\nafter\n");
        assert_eq!(value["retained_artifact_complete"], false);
        let second = Arc::new(Mutex::new(RawOutputArtifact::pending(home.path(), "drop")));
        let ready = CancellationToken::new();
        let mut abandoned = RawOutputArtifactTask::spawn(Some(second.clone()), ready.clone()).unwrap();
        abandoned.write_chunk(b"retained prefix\n");
        let live_id = second.lock().await.artifact_id().unwrap().to_string();
        drop(abandoned);
        assert!(ready.is_cancelled());
        assert!(matches!(*second.lock().await, RawOutputArtifact::Failed { .. }));
        let prefix = read_command_log(home.path(), "drop", &live_id,
            &[ToolOutputSelector::Lines { start:1, end:1 }], 4096).await.unwrap().unwrap();
        assert_eq!(prefix["results"][0]["text"], "retained prefix\n");
        assert_eq!(prefix["retained_artifact_complete"], false);
    }
}
