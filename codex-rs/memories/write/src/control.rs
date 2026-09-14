use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::Weak;
use tokio::sync::OwnedRwLockWriteGuard;
use tokio::sync::RwLock;
use uuid::Uuid;

// Reset excludes the entire startup pipeline, including seeding and worker shutdown.
pub(crate) fn memory_pipeline_lock(codex_home: &Path) -> Arc<RwLock<()>> {
    static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<RwLock<()>>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut locks = LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(codex_home).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(RwLock::new(()));
    locks.insert(codex_home.to_path_buf(), Arc::downgrade(&lock));
    lock
}

pub async fn clear_memory_roots_contents(codex_home: &Path) -> std::io::Result<()> {
    stage_memory_roots_reset(codex_home).await?.commit().await
}

/// Must be committed or rolled back. Dropping an uncommitted reset restores its roots.
#[must_use = "complete the reset with commit or rollback"]
pub struct StagedMemoryRootsReset {
    roots: Vec<StagedMemoryRoot>,
    _guard: OwnedRwLockWriteGuard<()>,
}

struct StagedMemoryRoot {
    live: PathBuf,
    staged: Option<PathBuf>,
}

pub async fn stage_memory_roots_reset(
    codex_home: &Path,
) -> std::io::Result<StagedMemoryRootsReset> {
    let guard = memory_pipeline_lock(codex_home).write_owned().await;
    let codex_home = codex_home.to_path_buf();
    tokio::task::spawn_blocking(move || stage_memory_roots_reset_sync(&codex_home, guard))
        .await
        .map_err(|err| std::io::Error::other(format!("memory reset staging task failed: {err}")))?
}

fn stage_memory_roots_reset_sync(
    codex_home: &Path,
    guard: OwnedRwLockWriteGuard<()>,
) -> std::io::Result<StagedMemoryRootsReset> {
    let mut roots = Vec::new();
    for name in ["memories", "memories_extensions"] {
        let live = codex_home.join(name);
        let metadata = match std::fs::symlink_metadata(&live) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let err = std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("refusing to clear symlinked memory root {}", live.display()),
                );
                return Err(restore_after_staging_error(&mut roots, err));
            }
            Ok(metadata) if !metadata.is_dir() => {
                let err = std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "refusing to clear non-directory memory root {}",
                        live.display()
                    ),
                );
                return Err(restore_after_staging_error(&mut roots, err));
            }
            Ok(metadata) => Some(metadata),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(restore_after_staging_error(&mut roots, err));
            }
        };

        let staged = if metadata.is_some() {
            let staged = codex_home.join(format!(".{name}.reset-{}", Uuid::new_v4()));
            if let Err(err) = std::fs::rename(&live, &staged) {
                return Err(restore_after_staging_error(&mut roots, err));
            }
            Some(staged)
        } else {
            None
        };

        if let Err(err) = std::fs::create_dir_all(&live) {
            roots.push(StagedMemoryRoot { live, staged });
            return match rollback_staged_roots(&mut roots) {
                Ok(()) => Err(err),
                Err(rollback_err) => Err(std::io::Error::other(format!(
                    "failed to stage memory reset: {err}; also failed to restore memory roots: {rollback_err}"
                ))),
            };
        }
        roots.push(StagedMemoryRoot { live, staged });
    }

    Ok(StagedMemoryRootsReset {
        roots,
        _guard: guard,
    })
}

impl StagedMemoryRootsReset {
    pub async fn rollback(mut self) -> std::io::Result<()> {
        tokio::task::spawn_blocking(move || rollback_staged_roots(&mut self.roots))
            .await
            .map_err(|err| std::io::Error::other(format!("memory rollback task failed: {err}")))?
    }

    pub async fn commit(mut self) -> std::io::Result<()> {
        tokio::task::spawn_blocking(move || {
            let mut errors = Vec::new();
            for root in self.roots.drain(..) {
                if let Some(staged) = root.staged
                    && let Err(err) = std::fs::remove_dir_all(&staged)
                {
                    errors.push(format!("remove backup {}: {err}", staged.display()));
                }
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(std::io::Error::other(errors.join("; ")))
            }
        })
        .await
        .map_err(|err| std::io::Error::other(format!("memory reset commit task failed: {err}")))?
    }
}

impl Drop for StagedMemoryRootsReset {
    fn drop(&mut self) {
        if let Err(err) = rollback_staged_roots(&mut self.roots) {
            tracing::error!("failed restoring abandoned memory reset: {err}");
        }
    }
}

fn rollback_staged_roots(roots: &mut Vec<StagedMemoryRoot>) -> std::io::Result<()> {
    let mut first_error = None;
    while let Some(mut root) = roots.pop() {
        if let Err(err) = rollback_staged_root(&mut root)
            && first_error.is_none()
        {
            first_error = Some(std::io::Error::new(
                err.kind(),
                format!(
                    "restore {} from {:?}: {err}",
                    root.live.display(),
                    root.staged
                ),
            ));
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn rollback_staged_root(root: &mut StagedMemoryRoot) -> std::io::Result<()> {
    match std::fs::remove_dir(&root.live) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    if let Some(staged) = root.staged.as_ref() {
        std::fs::rename(staged, &root.live)?;
        root.staged = None;
    }
    Ok(())
}

fn restore_after_staging_error(
    roots: &mut Vec<StagedMemoryRoot>,
    err: std::io::Error,
) -> std::io::Error {
    match rollback_staged_roots(roots) {
        Ok(()) => err,
        Err(rollback_err) => std::io::Error::new(
            err.kind(),
            format!("stage memory reset: {err}; rollback: {rollback_err}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn abandoned_reset_restores_data_and_releases_exclusion() {
        let home = tempdir().unwrap();
        let root = home.path().join("memories");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("MEMORY.md"), "saved").unwrap();
        let staged = stage_memory_roots_reset(home.path()).await.unwrap();
        let lock = memory_pipeline_lock(home.path());
        assert!(
            lock.try_read().is_err(),
            "reset must exclude startup writers"
        );
        assert!(!root.join("MEMORY.md").exists());
        drop(staged);
        assert_eq!(
            std::fs::read_to_string(root.join("MEMORY.md")).unwrap(),
            "saved"
        );
        assert!(lock.try_read().is_ok());
    }

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
