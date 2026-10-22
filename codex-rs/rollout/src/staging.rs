//! Conservative, bounded presence checks shared by deletion and optional GC.
use std::io;
use std::path::Path;

pub const RECOVERABLE_DELETE_PREFIX: &str = "thread-delete-v2-";

/// Call while owning rollout moves. This does not recover or reacquire locks.
/// Unknown layouts defer optional reclamation; resolved committed-only stages
/// do not. Errors likewise must never be interpreted as proven deletion.
pub async fn has_recoverable_delete_staging(home: &Path) -> io::Result<bool> {
    let mut entries = match tokio::fs::read_dir(home).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut visited = 0;
    while let Some(entry) = entries.next_entry().await? {
        visited += 1;
        if visited > 4096 {
            return Ok(true);
        }
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("thread-delete-")
        {
            continue;
        }
        if !entry.file_type().await?.is_dir()
            || !entry
                .file_name()
                .to_string_lossy()
                .starts_with(RECOVERABLE_DELETE_PREFIX)
        {
            return Ok(true);
        }
        let mut children = tokio::fs::read_dir(entry.path()).await?;
        while let Some(child) = children.next_entry().await? {
            visited += 1;
            if visited > 4096 {
                return Ok(true);
            }
            let name = child.file_name();
            let kind = child.file_type().await?;
            if name == ".owner" && kind.is_file() {
                continue;
            }
            if name == "committed" && kind.is_dir() {
                continue;
            }
            // Indexed stages require a DB witness, which GC deliberately does
            // not own. Unindexed and unknown stages are recoverable/uncertain.
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn abandoned_and_unknown_stages_defer_gc_but_resolved_stages_do_not() {
        for kind in ["indexed", "unindexed", "unknown", "committed"] {
            let home = tempfile::tempdir().unwrap();
            let stage = home
                .path()
                .join(format!("{RECOVERABLE_DELETE_PREFIX}fixture"));
            tokio::fs::create_dir_all(stage.join(kind)).await.unwrap();
            tokio::fs::write(stage.join(".owner"), b"").await.unwrap();
            assert_eq!(
                has_recoverable_delete_staging(home.path()).await.unwrap(),
                kind != "committed"
            );
        }
        let home = tempfile::tempdir().unwrap();
        assert!(!has_recoverable_delete_staging(home.path()).await.unwrap());
    }
}
