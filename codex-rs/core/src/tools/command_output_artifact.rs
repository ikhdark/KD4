use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;

fn max_retained_artifacts_per_thread() -> usize {
    128
}

fn max_retained_artifacts_total() -> usize {
    1_024
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
    bytes: u64,
}

impl RawOutputArtifactWriter {
    pub(crate) async fn open(state: Option<&Arc<Mutex<RawOutputArtifact>>>) -> Option<Self> {
        let state = state?;
        let artifact = state.lock().await.clone();
        let RawOutputArtifact::Stored { path, bytes } = artifact else {
            return Some(Self {
                path: None,
                file: None,
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
                    bytes,
                }),
                Err(err) => {
                    enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), &path).await;
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
                        bytes,
                    })
                }
            },
            Err(err) => {
                enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), &path).await;
                *state.lock().await = RawOutputArtifact::Failed {
                    message: format!("failed to open `{}` for streaming: {err}", path.display()),
                    owned_path: Some(path.clone()),
                    bytes,
                };
                Some(Self {
                    path: Some(path),
                    file: None,
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
            enforce_retention(&directory, &path).await;
            RawOutputArtifact::Stored {
                path,
                bytes: output.len() as u64,
            }
        }
        Err(err) => {
            enforce_retention(&directory, &path).await;
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
                    enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), path).await;
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
            enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), path).await;
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
    enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), &path).await;
    RawOutputArtifact::Failed {
        message,
        owned_path: Some(path),
        bytes,
    }
}

async fn enforce_retention(directory: &Path, keep_path: &Path) {
    let _retention_permit = retention_sweep_permit().await;
    enforce_retention_locked(directory, keep_path).await;
}

async fn enforce_retention_locked(directory: &Path, keep_path: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return;
    };
    let mut paths = Vec::new();
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) | Err(_) => break,
        };
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("log") {
            paths.push(path);
        }
    }
    paths.sort_unstable();

    let mut remove_count = paths
        .len()
        .saturating_sub(max_retained_artifacts_per_thread());
    for path in paths {
        if remove_count == 0 {
            break;
        }
        if path == keep_path {
            continue;
        }
        if remove_inactive_output_path(path).await {
            remove_count -= 1;
        }
    }

    if let Some(tool_output_root) = directory.parent() {
        enforce_global_retention_locked(tool_output_root, keep_path).await;
    }
}

#[cfg(test)]
async fn enforce_global_retention(tool_output_root: &Path, keep_path: &Path) {
    let _retention_permit = retention_sweep_permit().await;
    enforce_global_retention_locked(tool_output_root, keep_path).await;
}

async fn enforce_global_retention_locked(tool_output_root: &Path, keep_path: &Path) {
    let Ok(mut thread_directories) = tokio::fs::read_dir(tool_output_root).await else {
        return;
    };
    let mut paths = Vec::new();
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
    let mut remove_count = paths.len().saturating_sub(max_retained_artifacts_total());
    for (_, path) in paths {
        if remove_count == 0 {
            break;
        }
        if path != keep_path && remove_inactive_output_path(path).await {
            remove_count -= 1;
        }
    }
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
