use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

pub async fn clear_memory_roots_contents(codex_home: &Path) -> std::io::Result<()> {
    stage_memory_roots_reset(codex_home).await?.commit().await
}

pub struct StagedMemoryRootsReset {
    roots: Vec<StagedMemoryRoot>,
}

struct StagedMemoryRoot {
    live: PathBuf,
    staged: Option<PathBuf>,
}

pub async fn stage_memory_roots_reset(
    codex_home: &Path,
) -> std::io::Result<StagedMemoryRootsReset> {
    let mut roots = Vec::new();
    for name in ["memories", "memories_extensions"] {
        let live = codex_home.join(name);
        let metadata = match tokio::fs::symlink_metadata(&live).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let err = std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("refusing to clear symlinked memory root {}", live.display()),
                );
                rollback_staged_roots(&mut roots).await?;
                return Err(err);
            }
            Ok(metadata) if !metadata.is_dir() => {
                let err = std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "refusing to clear non-directory memory root {}",
                        live.display()
                    ),
                );
                rollback_staged_roots(&mut roots).await?;
                return Err(err);
            }
            Ok(metadata) => Some(metadata),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                rollback_staged_roots(&mut roots).await?;
                return Err(err);
            }
        };

        let staged = if metadata.is_some() {
            let staged = codex_home.join(format!(".{name}.reset-{}", Uuid::new_v4()));
            if let Err(err) = tokio::fs::rename(&live, &staged).await {
                rollback_staged_roots(&mut roots).await?;
                return Err(err);
            }
            Some(staged)
        } else {
            None
        };

        if let Err(err) = tokio::fs::create_dir_all(&live).await {
            roots.push(StagedMemoryRoot { live, staged });
            return match rollback_staged_roots(&mut roots).await {
                Ok(()) => Err(err),
                Err(rollback_err) => Err(std::io::Error::other(format!(
                    "failed to stage memory reset: {err}; also failed to restore memory roots: {rollback_err}"
                ))),
            };
        }
        roots.push(StagedMemoryRoot { live, staged });
    }

    Ok(StagedMemoryRootsReset { roots })
}

impl StagedMemoryRootsReset {
    pub async fn rollback(mut self) -> std::io::Result<()> {
        rollback_staged_roots(&mut self.roots).await
    }

    pub async fn commit(self) -> std::io::Result<()> {
        let mut first_error = None;
        for root in self.roots {
            if let Some(staged) = root.staged
                && let Err(err) = tokio::fs::remove_dir_all(staged).await
                && first_error.is_none()
            {
                first_error = Some(err);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

async fn rollback_staged_roots(roots: &mut Vec<StagedMemoryRoot>) -> std::io::Result<()> {
    let mut first_error = None;
    while let Some(mut root) = roots.pop() {
        if let Err(err) = rollback_staged_root(&mut root).await
            && first_error.is_none()
        {
            first_error = Some(err);
        }
    }
    first_error.map_or(Ok(()), Err)
}

async fn rollback_staged_root(root: &mut StagedMemoryRoot) -> std::io::Result<()> {
    match tokio::fs::remove_dir(&root.live).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    if let Some(staged) = root.staged.take() {
        tokio::fs::rename(staged, &root.live).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn clear_memory_roots_contents_preserves_root_directory() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("memories");
        let nested_dir = root.join("rollout_summaries");
        tokio::fs::create_dir_all(&nested_dir)
            .await
            .expect("create rollout summaries dir");
        tokio::fs::write(root.join("MEMORY.md"), "stale memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(nested_dir.join("rollout.md"), "stale rollout\n")
            .await
            .expect("write rollout summary");

        clear_memory_roots_contents(dir.path())
            .await
            .expect("clear memory roots contents");

        assert!(
            tokio::fs::try_exists(&root)
                .await
                .expect("check memory root existence"),
            "memory root should still exist after clearing contents"
        );
        let mut entries = tokio::fs::read_dir(&root)
            .await
            .expect("read memory root after clear");
        assert!(
            entries
                .next_entry()
                .await
                .expect("read next entry")
                .is_none(),
            "memory root should be empty after clearing contents"
        );
    }

    #[tokio::test]
    async fn staged_memory_roots_reset_can_restore_both_roots() {
        let dir = tempdir().expect("tempdir");
        let memories = dir.path().join("memories");
        let extensions = dir.path().join("memories_extensions");
        tokio::fs::create_dir_all(&memories)
            .await
            .expect("create memories root");
        tokio::fs::create_dir_all(&extensions)
            .await
            .expect("create extensions root");
        tokio::fs::write(memories.join("MEMORY.md"), "memory\n")
            .await
            .expect("write memory");
        tokio::fs::write(extensions.join("extension.md"), "extension\n")
            .await
            .expect("write extension memory");

        stage_memory_roots_reset(dir.path())
            .await
            .expect("stage reset")
            .rollback()
            .await
            .expect("rollback reset");

        assert_eq!(
            tokio::fs::read_to_string(memories.join("MEMORY.md"))
                .await
                .expect("read restored memory"),
            "memory\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(extensions.join("extension.md"))
                .await
                .expect("read restored extension memory"),
            "extension\n"
        );
    }
}
