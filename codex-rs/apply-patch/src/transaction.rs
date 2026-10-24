//! Advisory endpoint ownership for cooperating local patch processes. This does
//! not exclude editors/commands that do not participate, or provide remote CAS.
use std::collections::BTreeSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;

use anyhow::Context;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::FileSystemSandboxContext;
use codex_utils_path_uri::PathUri;
use sha2::Digest;
use sha2::Sha256;

use crate::Hunk;
use crate::invocation::mutation_endpoint_identity;

pub(crate) struct PatchTransaction {
    // Never unlink lock files: a waiter must not lock a different inode from an
    // existing owner. Closing handles releases ownership, including on crash.
    _locks: Vec<File>,
}

async fn identities(
    hunks: &[Hunk],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> anyhow::Result<BTreeSet<String>> {
    let mut endpoints = BTreeSet::new();
    for hunk in hunks {
        let source = hunk.resolve_path(cwd)?;
        endpoints.insert(lease_key(
            mutation_endpoint_identity(fs, &source, sandbox).await?,
        ));
        if let Hunk::UpdateFile {
            move_path: Some(destination),
            ..
        } = hunk
        {
            let destination = cwd.join(&destination.to_string_lossy())?;
            endpoints.insert(lease_key(
                mutation_endpoint_identity(fs, &destination, sandbox).await?,
            ));
        }
    }
    Ok(endpoints)
}

fn lease_key(identity: String) -> String {
    // Keep prospective and existing Windows endpoints in the same namespace,
    // including after creation changes their canonical spelling. Conservatively
    // serialize case-distinct files even in case-sensitive Windows directories.
    if cfg!(windows) {
        identity.to_lowercase()
    } else {
        identity
    }
}

pub(crate) async fn acquire(
    hunks: &[Hunk],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&FileSystemSandboxContext>,
) -> anyhow::Result<Option<PatchTransaction>> {
    if !fs.uses_host_native_paths() {
        // Host paths cannot coordinate a remote backend. Its existing executor
        // ownership remains in force; no cross-process guarantee is inferred.
        return Ok(None);
    }
    let endpoints = identities(hunks, cwd, fs, sandbox).await?;
    let keys = endpoints.clone();
    let transaction = tokio::task::spawn_blocking(move || -> io::Result<PatchTransaction> {
        // Stable across CODEX_HOME/checkouts. The account-private directory
        // is the lease namespace; participants must share it. No
        // target or missing target parent is created during admission.
        let directory = lease_directory()?;
        let mut locks = Vec::with_capacity(keys.len());
        for key in keys {
            let digest = hex::encode(Sha256::digest(key.as_bytes()));
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            }
            let file = options.open(directory.join(digest))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let metadata = file.metadata()?;
                // SAFETY: geteuid takes no pointers and has no failure mode.
                let uid = unsafe { libc::geteuid() };
                if !metadata.is_file() || metadata.uid() != uid || metadata.permissions().mode() & 0o077 != 0 {
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unsafe patch lease file"));
                }
            }
            file.try_lock().map_err(|error| match error {
                std::fs::TryLockError::WouldBlock => io::Error::new(io::ErrorKind::WouldBlock,
                    "apply_patch: an endpoint is busy in another patch process. No files were changed by this attempt; retry after its owner finishes."),
                std::fs::TryLockError::Error(error) => error,
            })?;
            locks.push(file);
        }
        Ok(PatchTransaction { _locks: locks })
    }).await.context("patch endpoint admission worker failed")??;
    anyhow::ensure!(
        identities(hunks, cwd, fs, sandbox).await? == endpoints,
        "patch endpoint aliases changed during admission; no files were changed"
    );
    Ok(Some(transaction))
}

fn lease_directory() -> io::Result<std::path::PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        // SAFETY: geteuid takes no pointers and has no failure mode.
        let uid = unsafe { libc::geteuid() };
        let home = std::env::var_os("HOME").ok_or_else(||
            io::Error::new(io::ErrorKind::NotFound, "HOME is required for private patch leases"))?;
        let home = std::fs::canonicalize(std::path::PathBuf::from(home))?;
        let home_metadata = std::fs::metadata(&home)?;
        if !home_metadata.is_dir() || home_metadata.uid() != uid || home_metadata.permissions().mode() & 0o022 != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "patch lease home is not private to this account"));
        }
        // Unlike a predictable name under shared /tmp, another account cannot
        // pre-create this directory and deny all future endpoint admissions.
        let directory = home.join(".codex-patch-transactions-v2");
        match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = std::fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != uid
            || metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unsafe patch lease directory"));
        }
        return Ok(directory);
    }
    #[cfg(not(unix))]
    {
        let directory = std::env::temp_dir().join("codex-patch-transactions-v1");
        std::fs::create_dir_all(&directory)?;
        Ok(directory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_patch;
    use crate::apply_patch_with_cancellation;
    use std::path::Path;
    use std::process::Child;
    use std::process::Command;
    use std::process::Stdio;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready() {
            assert!(Instant::now() < deadline, "patch process barrier timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct ChildOwner(Child);
    impl Drop for ChildOwner {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // Invoked only by the isolated parent regression, never as a normal test.
    #[test]
    #[ignore]
    fn patch_process_worker() {
        let directory = std::env::var_os("CODEX_PATCH_TRANSACTION_FIXTURE").unwrap();
        let root = Path::new(&directory);
        let rollback = std::env::var_os("CODEX_PATCH_TRANSACTION_ROLLBACK").is_some();
        let patch = if rollback {
            "*** Begin Patch\n*** Update File: shared\n@@\n-a=0\n+a=1\n*** Add File: suffix\n+never\n*** End Patch"
        } else {
            "*** Begin Patch\n*** Update File: shared\n@@\n-a=0\n+a=1\n*** End Patch"
        };
        let calls = AtomicUsize::new(0);
        let cancellation = || {
            if calls.fetch_add(1, Ordering::SeqCst) == usize::from(rollback) {
                std::fs::write(root.join("ready"), b"ready").unwrap();
                wait_until(|| root.join("release").exists());
                rollback
            } else {
                false
            }
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let environment = codex_exec_server::Environment::default_for_tests();
        let cwd = PathUri::from_host_native_path(root).unwrap();
        let result = runtime.block_on(apply_patch_with_cancellation(
            patch,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            environment.get_filesystem().as_ref(),
            None,
            &cancellation,
        ));
        assert_eq!(result.is_err(), rollback);
        if let Err(failure) = result {
            assert!(failure.delta.changes.is_empty());
        }
    }

    #[tokio::test]
    async fn separate_process_update_and_rollback_exclude_aliases_but_not_other_files() {
        for rollback in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            std::fs::write(root.join("shared"), "a=0\nb=0\n").unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--ignored",
                    "--exact",
                    "transaction::tests::patch_process_worker",
                ])
                .env("CODEX_PATCH_TRANSACTION_FIXTURE", root)
                .env_remove("CODEX_PATCH_TRANSACTION_ROLLBACK")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit());
            if rollback {
                command.env("CODEX_PATCH_TRANSACTION_ROLLBACK", "1");
            }
            let mut child = ChildOwner(command.spawn().unwrap());
            wait_until(|| root.join("ready").exists());
            let environment = codex_exec_server::Environment::default_for_tests();
            let fs = environment.get_filesystem();
            let cwd = PathUri::from_host_native_path(root).unwrap();
            let apply = |body: &str| format!("*** Begin Patch\n{body}\n*** End Patch");
            // A disjoint endpoint still progresses while the peer owns shared.
            apply_patch(
                &apply("*** Add File: independent\n+ok"),
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                fs.as_ref(),
                None,
            )
            .await
            .unwrap();
            let peer = apply("*** Update File: ./shared\n@@\n-b=0\n+b=1");
            let failure = apply_patch(
                &peer,
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                fs.as_ref(),
                None,
            )
            .await
            .unwrap_err();
            assert!(
                failure
                    .to_string()
                    .contains("busy in another patch process")
            );
            assert!(failure.delta.changes.is_empty());
            std::fs::write(root.join("release"), b"release").unwrap();
            let mut status = None;
            wait_until(|| {
                status = child.0.try_wait().unwrap();
                status.is_some()
            });
            assert!(status.unwrap().success());
            assert_eq!(
                std::fs::read_to_string(root.join("shared")).unwrap(),
                if rollback { "a=0\nb=0\n" } else { "a=1\nb=0\n" }
            );
            apply_patch(
                &peer,
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                fs.as_ref(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                std::fs::read_to_string(root.join("shared")).unwrap(),
                if rollback { "a=0\nb=1\n" } else { "a=1\nb=1\n" }
            );
            assert!(!root.join("suffix").exists());
        }
    }

    #[tokio::test]
    async fn move_owns_both_endpoints_and_invalid_patch_creates_no_target_parent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("source"), "old\n").unwrap();
        let environment = codex_exec_server::Environment::default_for_tests();
        let fs = environment.get_filesystem();
        let cwd = PathUri::from_host_native_path(root).unwrap();
        let patch = "*** Begin Patch\n*** Update File: source\n*** Move to: destination\n@@\n-old\n+new\n*** End Patch";
        let hunks = crate::parse_patch(patch).unwrap().hunks;
        let held = acquire(&hunks, &cwd, fs.as_ref(), None).await.unwrap();
        for path in ["source", "destination"] {
            let peer = format!("*** Begin Patch\n*** Add File: {path}\n+peer\n*** End Patch");
            assert!(
                apply_patch(
                    &peer,
                    &cwd,
                    &mut Vec::new(),
                    &mut Vec::new(),
                    fs.as_ref(),
                    None
                )
                .await
                .is_err()
            );
        }
        drop(held);
        apply_patch(
            patch,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            fs.as_ref(),
            None,
        )
        .await
        .unwrap();
        assert!(!root.join("source").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("destination")).unwrap(),
            "new\n"
        );
        let invalid = "*** Begin Patch\n*** Add File: missing/prefix\n+never\n*** Update File: absent\n@@\n-old\n+new\n*** End Patch";
        assert!(
            apply_patch(
                invalid,
                &cwd,
                &mut Vec::new(),
                &mut Vec::new(),
                fs.as_ref(),
                None
            )
            .await
            .is_err()
        );
        assert!(!root.join("missing").exists());
    }
}
