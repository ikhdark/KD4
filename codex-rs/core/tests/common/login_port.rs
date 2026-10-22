//! Cross-process ownership of the registered OAuth callback ports used by tests.

use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;

/// Keep this guard alive until callback listeners and workers have drained.
/// The OS releases the lease on panic/process death; never unlink the lock file.
pub struct LoginPortLease {
    _file: File,
}

impl LoginPortLease {
    /// All worktrees share this namespace, unlike Cargo/Nextest invocation locks.
    pub async fn acquire() -> anyhow::Result<Self> {
        tokio::task::spawn_blocking(|| {
            Self::acquire_at(
                &std::env::temp_dir().join("kd4-oauth-callback-1455-1457.lock"),
                Duration::from_secs(10),
            )
        })
        .await?
    }

    fn acquire_at(path: &Path, timeout: Duration) -> anyhow::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .context("OAuth callback setup unverified: cannot open host fixture lease")?;
        let deadline = Instant::now() + timeout;
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Self { _file: file }),
                Err(error)
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    if Instant::now() >= deadline {
                        anyhow::bail!(
                            "OAuth callback behavior unverified: timed out acquiring host fixture lease"
                        );
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error).context("OAuth callback fixture lease failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contention_is_bounded_and_drop_releases_lease() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("ports.lock");
        let lease = LoginPortLease::acquire_at(&path, Duration::ZERO)?;
        let error = LoginPortLease::acquire_at(&path, Duration::ZERO)
            .err()
            .expect("a second owner must not enter the fixture");
        assert!(error.to_string().contains("behavior unverified"));
        drop(lease);
        let _next = LoginPortLease::acquire_at(&path, Duration::ZERO)?;
        Ok(())
    }
}
