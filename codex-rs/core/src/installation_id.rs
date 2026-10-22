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

#[cfg(test)]
mod tests {
    use super::INSTALLATION_ID_FILENAME;
    use super::resolve_installation_id;
    use crate::test_harness::PathExt;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use uuid::Uuid;

    #[test]
    fn cancelled_identity_lock_wait_does_not_retain_runtime_or_rotate_uuid() {
        let home = TempDir::new().unwrap();
        let path = home.path().join(INSTALLATION_ID_FILENAME);
        let expected = Uuid::new_v4().to_string();
        std::fs::write(&path, &expected).unwrap();
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        holder.lock().unwrap();
        let absolute = home.path().abs();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let cancelled = runtime.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    resolve_installation_id(&absolute),
                )
                .await
                .is_err()
            });
            drop(runtime);
            let _ = finished_tx.send(cancelled);
        });
        let result = finished_rx.recv_timeout(std::time::Duration::from_secs(2));
        drop(holder);
        waiter.join().unwrap();
        assert!(result.expect("runtime must exit while the independent holder is still locked"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    }

    #[tokio::test]
    async fn resolve_installation_id_generates_and_persists_uuid() {
        let codex_home = TempDir::new().expect("create temp dir");
        let home = codex_home.path().join("nested");
        let codex_home_abs = home.abs();
        let persisted_path = home.join(INSTALLATION_ID_FILENAME);

        let installation_id = resolve_installation_id(&codex_home_abs)
            .await
            .expect("resolve installation id");

        assert_eq!(
            std::fs::read_to_string(&persisted_path).expect("read persisted installation id"),
            installation_id
        );
        assert!(Uuid::parse_str(&installation_id).is_ok());
        assert_eq!(
            resolve_installation_id(&codex_home_abs).await.unwrap(),
            installation_id,
            "resolving a persisted identity must not rotate it"
        );

        // Existing IDs are normalized for callers but are not rewritten on disk.
        let existing = format!("  {}\n", installation_id.to_uppercase());
        std::fs::write(&persisted_path, &existing).expect("write installation id");

        let resolved = resolve_installation_id(&codex_home_abs)
            .await
            .expect("resolve installation id");

        assert_eq!(resolved, installation_id);
        assert_eq!(std::fs::read_to_string(&persisted_path).unwrap(), existing);
    }

    #[tokio::test]
    async fn resolve_installation_id_rewrites_invalid_file_contents() {
        let codex_home = TempDir::new().expect("create temp dir");
        let existing = Uuid::new_v4().to_string();
        let path = codex_home.path().join(INSTALLATION_ID_FILENAME);
        for contents in [
            String::new(),
            "not-a-uuid".to_string(),
            format!("{existing}{}", " ".repeat(512 * 1024)),
        ] {
            std::fs::write(&path, &contents).expect("write invalid record");
            let resolved = resolve_installation_id(&codex_home.path().abs())
                .await
                .expect("replace invalid record");
            assert_ne!(resolved, existing);
            assert!(Uuid::parse_str(&resolved).is_ok());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), resolved);
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 36);
            assert_eq!(
                resolve_installation_id(&codex_home.path().abs())
                    .await
                    .unwrap(),
                resolved,
                "repaired identity must survive the next resolution"
            );
        }
    }
}
