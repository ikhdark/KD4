//! Stable logical writer ownership, independent of active/archive file paths.
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;

/// Nonblocking OS lease. Lock files are never removed: unlinking an open lock
/// would permit a second inode to represent the same logical conversation.
pub async fn acquire_thread_writer(home: &Path, id: codex_protocol::ThreadId) -> io::Result<Arc<File>> {
    let path = home.join("thread-writer-locks").join(format!("{id}.lock"));
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(path.parent().ok_or_else(|| io::Error::other("missing lease parent"))?)?;
        let file = File::options().create(true).truncate(false).read(true).write(true).open(path)?;
        file.try_lock().map_err(io::Error::from)?;
        Ok(Arc::new(file))
    }).await.map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn independent_owners_conflict_until_last_guard_drops() {
        let home = tempfile::tempdir().unwrap();
        let id = codex_protocol::ThreadId::new();
        let first = acquire_thread_writer(home.path(), id).await.unwrap();
        let retained = Arc::clone(&first);
        drop(first);
        assert_eq!(acquire_thread_writer(home.path(), id).await.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        let other = acquire_thread_writer(home.path(), codex_protocol::ThreadId::new()).await.unwrap();
        drop(retained);
        assert!(acquire_thread_writer(home.path(), id).await.is_ok());
        drop(other);
    }
}
