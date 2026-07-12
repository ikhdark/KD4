use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RawOutputArtifact {
    Stored { path: PathBuf, bytes: u64 },
    Failed { message: String },
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
        match tokio::fs::OpenOptions::new().append(true).open(&path).await {
            Ok(file) => Some(Self {
                path: Some(path),
                file: Some(file),
                bytes,
            }),
            Err(err) => {
                *state.lock().await = RawOutputArtifact::Failed {
                    message: format!("failed to open `{}` for streaming: {err}", path.display()),
                };
                Some(Self {
                    path: None,
                    file: None,
                    bytes: 0,
                })
            }
        }
    }

    pub(crate) async fn write_chunk(
        &mut self,
        state: Option<&Arc<Mutex<RawOutputArtifact>>>,
        output: &[u8],
    ) {
        let (Some(state), Some(path), Some(file)) = (state, self.path.as_ref(), self.file.as_mut())
        else {
            return;
        };
        if let Err(err) = file.write_all(output).await {
            *state.lock().await = RawOutputArtifact::Failed {
                message: format!("failed to stream `{}`: {err}", path.display()),
            };
            self.file = None;
            return;
        }
        self.bytes = self.bytes.saturating_add(output.len() as u64);
        *state.lock().await = RawOutputArtifact::Stored {
            path: path.clone(),
            bytes: self.bytes,
        };
    }

    pub(crate) async fn finish(&mut self, state: Option<&Arc<Mutex<RawOutputArtifact>>>) {
        let (Some(state), Some(path), Some(file)) = (state, self.path.as_ref(), self.file.as_mut())
        else {
            return;
        };
        if let Err(err) = file.flush().await {
            *state.lock().await = RawOutputArtifact::Failed {
                message: format!("failed to flush `{}`: {err}", path.display()),
            };
        }
    }
}

impl RawOutputArtifact {
    pub(crate) fn render_for_model(&self) -> String {
        match self {
            Self::Stored { path, bytes } => format!(
                "Raw output artifact: {} ({bytes} bytes retained before model summarization)",
                path.display()
            ),
            Self::Failed { message } => {
                format!("Raw output artifact unavailable: {message}")
            }
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
        return RawOutputArtifact::Failed {
            message: format!("failed to create `{}`: {err}", directory.display()),
        };
    }

    let path = directory.join(format!("{}.log", uuid::Uuid::now_v7()));
    match tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .await
    {
        Ok(mut file) => {
            if let Err(err) = file.write_all(output).await {
                return RawOutputArtifact::Failed {
                    message: format!("failed to write `{}`: {err}", path.display()),
                };
            }
            if let Err(err) = file.flush().await {
                return RawOutputArtifact::Failed {
                    message: format!("failed to flush `{}`: {err}", path.display()),
                };
            }
            RawOutputArtifact::Stored {
                path,
                bytes: output.len() as u64,
            }
        }
        Err(err) => RawOutputArtifact::Failed {
            message: format!("failed to create `{}`: {err}", path.display()),
        },
    }
}

pub(crate) async fn append_raw_output_artifact(
    artifact: &RawOutputArtifact,
    output: &[u8],
) -> RawOutputArtifact {
    let RawOutputArtifact::Stored { path, .. } = artifact else {
        return artifact.clone();
    };

    match tokio::fs::OpenOptions::new().append(true).open(path).await {
        Ok(mut file) => {
            if let Err(err) = file.write_all(output).await {
                return RawOutputArtifact::Failed {
                    message: format!("failed to append `{}`: {err}", path.display()),
                };
            }
            if let Err(err) = file.flush().await {
                return RawOutputArtifact::Failed {
                    message: format!("failed to flush `{}`: {err}", path.display()),
                };
            }
            match file.metadata().await {
                Ok(metadata) => RawOutputArtifact::Stored {
                    path: path.clone(),
                    bytes: metadata.len(),
                },
                Err(err) => RawOutputArtifact::Failed {
                    message: format!("failed to stat `{}` after append: {err}", path.display()),
                },
            }
        }
        Err(err) => RawOutputArtifact::Failed {
            message: format!("failed to open `{}` for append: {err}", path.display()),
        },
    }
}

pub(crate) async fn replace_raw_output_artifact(
    artifact: &RawOutputArtifact,
    output: &[u8],
) -> RawOutputArtifact {
    let RawOutputArtifact::Stored { path, .. } = artifact else {
        return artifact.clone();
    };

    match tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .await
    {
        Ok(mut file) => {
            if let Err(err) = file.write_all(output).await {
                return RawOutputArtifact::Failed {
                    message: format!("failed to replace `{}`: {err}", path.display()),
                };
            }
            if let Err(err) = file.flush().await {
                return RawOutputArtifact::Failed {
                    message: format!("failed to flush `{}`: {err}", path.display()),
                };
            }
            RawOutputArtifact::Stored {
                path: path.clone(),
                bytes: output.len() as u64,
            }
        }
        Err(err) => RawOutputArtifact::Failed {
            message: format!("failed to open `{}` for replacement: {err}", path.display()),
        },
    }
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
}
