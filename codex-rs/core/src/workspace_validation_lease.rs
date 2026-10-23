//! Cross-process validation leases in the shared host temporary namespace.
//! A dirty record is deliberately not reclaimed after host death: its child
//! may still be running. Normal release clears the record before unlocking.
use fs2::FileExt;
use sha2::Digest;
use std::fs::File;
use std::io;
use std::io::Write;
use std::path::Path;

#[derive(Debug)]
pub(super) struct ValidationLease {
    file: File,
}

impl ValidationLease {
    pub(super) fn try_acquire(root: &Path, command: &str) -> io::Result<Option<Self>> {
        let directory = std::env::temp_dir().join("codex-workspace-validation");
        std::fs::create_dir_all(&directory)?;
        let name = hex::encode(sha2::Sha256::digest(root.as_os_str().as_encoded_bytes()));
        Self::at(&directory.join(format!("{name}.lock")), command)
    }

    fn at(path: &Path, command: &str) -> io::Result<Option<Self>> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        }
        if file.metadata()?.len() != 0 {
            return Err(io::Error::other(format!(
                "Validation ownership is uncertain after a previous host exited. Its child may still be running. Verify that validation has stopped before clearing the retained lease at {}. This validation did not run.",
                path.display()
            )));
        }
        let record = serde_json::to_vec(&serde_json::json!({
            "version": 1, "host_pid": std::process::id(), "command": command,
        }))
        .map_err(io::Error::other)?;
        // Write first, then publish a lease. A partial or uncertain write stays
        // dirty and fails closed rather than authorizing another validation.
        file.write_all(&record)?;
        file.sync_all()?;
        Ok(Some(Self { file }))
    }
}

impl Drop for ValidationLease {
    fn drop(&mut self) {
        if let Err(error) = self.file.set_len(0) {
            tracing::warn!(%error, "validation lease could not be cleared; ownership remains uncertain");
        }
        // File close releases the OS lock; never unlink its shared inode.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit151_validation_lease_excludes_other_handles_and_releases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease");
        let first = ValidationLease::at(&path, "first").unwrap().unwrap();
        assert!(ValidationLease::at(&path, "second").unwrap().is_none());
        let other = ValidationLease::at(&dir.path().join("other"), "independent").unwrap();
        assert!(other.is_some());
        drop(first);
        assert!(ValidationLease::at(&path, "successor").unwrap().is_some());
    }

    #[test]
    fn audit151_uncertain_owner_is_not_silently_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease");
        std::fs::write(&path, br#"{"host_pid":0,"command":"possibly orphaned"}"#).unwrap();
        let error = ValidationLease::at(&path, "successor").unwrap_err();
        assert!(error.to_string().contains("child may still be running"));
        assert!(!std::fs::read(&path).unwrap().is_empty());
    }

    #[test]
    #[ignore = "subprocess fixture invoked by audit151_cross_process_validation_lease"]
    fn audit151_lease_child() {
        let Some(path) = std::env::var_os("KD4_AUDIT151_LEASE") else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        let _lease = ValidationLease::at(&path, "child validation")
            .unwrap()
            .unwrap();
        std::fs::write(path.with_extension("ready"), b"ready").unwrap();
        if std::env::var_os("KD4_AUDIT151_ABRUPT").is_some() {
            std::process::exit(0); // OS unlocks; the dirty record must remain.
        }
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !path.with_extension("release").exists() {
            assert!(
                std::time::Instant::now() < until,
                "parent did not release child"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn audit151_cross_process_validation_lease() {
        for abrupt in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("lease");
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "workspace_operation_gate::validation_lease::tests::audit151_lease_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("KD4_AUDIT151_LEASE", &path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if abrupt {
                command.env("KD4_AUDIT151_ABRUPT", "1");
            }
            let mut child = command.spawn().unwrap();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !path.with_extension("ready").exists() {
                if std::time::Instant::now() >= until {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("child lease was not observed");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if !abrupt {
                assert!(ValidationLease::at(&path, "contender").unwrap().is_none());
                std::fs::write(path.with_extension("release"), b"release").unwrap();
            }
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if std::time::Instant::now() >= until {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("child did not settle");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            assert!(status.success());
            if abrupt {
                assert!(ValidationLease::at(&path, "contender").is_err());
            } else {
                assert!(ValidationLease::at(&path, "successor").unwrap().is_some());
            }
        }
    }
}
