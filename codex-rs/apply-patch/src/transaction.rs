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
