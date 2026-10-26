//! Stable logical writer ownership, independent of active/archive file paths.
//!
//! Writable-version boundary: every executable mutating a shared storage home
//! must participate in this lifetime lease and representation ownership. Older
//! executables without those owners may still read/export legacy history, but
//! must be fully stopped before an upgraded executable writes the same home.
//! A new marker, an idle-turn scan, or a momentarily free physical lock cannot
//! prevent an unmodified old process from publishing later. Such mixed-version
//! mutation is unsupported; payload collection remains deferred for this reason.
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;

/// Nonblocking OS lease. Lock files are never removed: unlinking an open lock
/// would permit a second inode to represent the same logical conversation.
pub async fn acquire_thread_writer(
    home: &Path,
    id: codex_protocol::ThreadId,
) -> io::Result<Arc<File>> {
    let path = home.join("thread-writer-locks").join(format!("{id}.lock"));
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| io::Error::other("missing lease parent"))?,
        )?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock().map_err(io::Error::from)?;
        Ok(Arc::new(file))
    })
    .await
    .map_err(io::Error::other)?
}

