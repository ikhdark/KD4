use std::fs::OpenOptions;
use std::io::Read;
use std::io::Result;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;

use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::fs;
use uuid::Uuid;

pub(crate) const INSTALLATION_ID_FILENAME: &str = "installation_id";
const MAX_INSTALLATION_ID_BYTES: u64 = 128;

pub async fn resolve_installation_id(codex_home: &AbsolutePathBuf) -> Result<String> {
    let path = codex_home.join(INSTALLATION_ID_FILENAME);
    fs::create_dir_all(codex_home).await?;
    let file = tokio::task::spawn_blocking(move || {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        options.open(&path)
    })
    .await??;
    // Never put a blocking native lock in Tokio's blocking pool: cancelling its
    // caller cannot stop it and runtime teardown would still wait for the lock.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "installation ID lock contention exceeded 10 seconds",
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(std::fs::TryLockError::Error(err)) => return Err(err),
        }
    }
    tokio::task::spawn_blocking(move || {
        let mut file = file;
        let mut contents = Vec::new();
        (&mut file)
            .take(MAX_INSTALLATION_ID_BYTES + 1)
            .read_to_end(&mut contents)?;
        if contents.len() <= MAX_INSTALLATION_ID_BYTES as usize {
            let contents = String::from_utf8(contents)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            let trimmed = contents.trim();
            if !trimmed.is_empty()
                && let Ok(existing) = Uuid::parse_str(trimmed)
            {
                return Ok(existing.to_string());
            }
        }

        let installation_id = Uuid::new_v4().to_string();
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(installation_id.as_bytes())?;
        file.flush()?;
        file.sync_all()?;

        Ok(installation_id)
    })
    .await?
}
