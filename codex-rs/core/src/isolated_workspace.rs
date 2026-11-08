//! Stable dirty-worktree snapshots shared by root and worker launch admission.
use crate::FunctionCallError;
use sha2::{Digest, Sha256};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;
/// Checkout lifetime and repository-wide permission admission use separate locks.
/// The short admission lock serializes upgrades; the lifetime lock is never waited on.
#[derive(Debug)]
pub(crate) struct WorkspaceLaunch {
    _ownership: Option<std::fs::File>,
    concurrency: Option<std::sync::Mutex<std::fs::File>>,
    admission_path: PathBuf,
    exclusive: AtomicBool,
    base_permissions: codex_protocol::models::PermissionProfile,
    directory_guards: std::sync::Mutex<Vec<std::fs::File>>,
    pub(crate) source_root: PathBuf,
    pub(crate) workspace_root: PathBuf,
    pub(crate) report_directory: PathBuf,
    snapshot: Option<IsolatedWorkspace>,
}

fn open_lock(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // A live owner must not be bypassed by replacing its lock pathname.
        options.share_mode(1 | 2);
    }
    options.open(path)
}

fn admission_lock(path: &Path) -> std::io::Result<std::fs::File> {
    let file = open_lock(path)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                return Err(std::io::Error::other(format!(
                    "workspace admission busy or unavailable: {error}"
                )));
            }
        }
    }
}

impl WorkspaceLaunch {
    pub(crate) fn is_isolated(&self) -> bool {
        self.snapshot.is_some()
    }

    pub(crate) fn retain(&self) {
        tracing::info!(source = %self.source_root.display(), workspace = %self.workspace_root.display(),
            reports = %self.report_directory.display(), isolated = self.is_isolated(), "retaining admitted workspace");
        if let Some(snapshot) = &self.snapshot {
            snapshot.cleanup_required.store(false, Ordering::Release);
        }
    }

    /// A permission expansion permanently makes this owner non-shareable.
    /// Serialize unlock/upgrade against launches: there is no unprotected gap.
    pub(crate) fn require_exclusive(&self) -> std::io::Result<()> {
        if self.snapshot.is_some() {
            return Err(std::io::Error::other(
                "isolated workspace permissions cannot expand; use conflict-checked host integration",
            ));
        }
        if self.exclusive.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(concurrency) = &self.concurrency else {
            return Err(std::io::Error::other(
                "read-only workspace cannot acquire repository write authority",
            ));
        };
        let _admission = open_lock(&self.admission_path)?;
        _admission.try_lock().map_err(std::io::Error::other)?;
        let file = concurrency
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.exclusive.load(Ordering::Acquire) {
            return Ok(());
        }
        file.unlock()?;
        match file.try_lock() {
            Ok(()) => {
                self.exclusive.store(true, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                // No launch or permission expansion can enter until admission drops.
                file.try_lock_shared().map_err(std::io::Error::other)?;
                Err(std::io::Error::other(format!(
                    "concurrent workspace protection forbids unsandboxed execution or permission expansion: {error}"
                )))
            }
        }
    }

    pub(crate) fn validate_permissions(
        &self,
        permissions: &codex_protocol::models::PermissionProfile,
        unsandboxed: bool,
    ) -> std::io::Result<()> {
        if unsandboxed
            || permissions.file_system_sandbox_policy()
                != self.base_permissions.file_system_sandbox_policy()
        {
            self.require_exclusive()?;
        }
        Ok(())
    }

    pub(crate) fn validate_write_paths(
        &self,
        paths: &[codex_utils_absolute_path::AbsolutePathBuf],
        cwd: &Path,
    ) -> std::io::Result<()> {
        let policy = self.base_permissions.file_system_sandbox_policy();
        if paths
            .iter()
            .any(|path| !policy.can_write_path_with_cwd(path.as_path(), cwd))
        {
            self.require_exclusive()?;
        }
        Ok(())
    }
}

fn git_directories(root: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    let root = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(root)
        .map_err(std::io::Error::other)?;
    codex_command_analysis::git_dirs::resolve_git_directories(&root)
        .ok_or_else(|| std::io::Error::other("cannot resolve checkout ownership directory"))
}

fn ownership_file(root: &Path) -> std::io::Result<std::fs::File> {
    let (git_dir, _) = git_directories(root)?;
    open_lock(&git_dir.join("codex-workspace-ownership.lock"))
}

/// User-scoped, independent of CODEX_HOME and repository spelling. Unrestricted
/// owners must exclude writers in other repositories too: absolute paths cross cwd.
fn registry_root() -> std::io::Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
    }
    .ok_or_else(|| std::io::Error::other("cannot resolve user workspace admission storage"))?;
    let root = base.join("codex").join("workspace-admission");
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

pub(crate) async fn cleanup_workspace_launch(workspace: &WorkspaceLaunch) -> std::io::Result<()> {
    workspace
        .directory_guards
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    if let Some(snapshot) = &workspace.snapshot {
        cleanup_isolated_worktree(snapshot).await
    } else {
        Ok(())
    }
}

fn confinement_supported(config: &crate::config::Config) -> bool {
    use crate::windows_sandbox::WindowsSandboxLevelExt;
    use codex_sandboxing::{SandboxType, SandboxablePreference};
    let permissions = config.permissions.effective_permission_profile();
    #[cfg(windows)]
    if codex_protocol::config_types::WindowsSandboxLevel::from_config(config)
        != codex_protocol::config_types::WindowsSandboxLevel::Elevated
    {
        return false; // Restricted-token backend cannot guarantee delete/absolute-path boundaries.
    }
    codex_sandboxing::select_initial(
        &permissions.file_system_sandbox_policy(),
        permissions.network_sandbox_policy(),
        SandboxablePreference::Require,
        codex_protocol::config_types::WindowsSandboxLevel::from_config(config),
        false,
    ) != SandboxType::None
}

fn shareable(config: &crate::config::Config, root: &Path) -> bool {
    use codex_protocol::permissions::FileSystemPath;
    let policy = config.permissions.file_system_sandbox_policy();
    confinement_supported(config)
        && matches!(
            config.permissions.permission_profile(),
            codex_protocol::models::PermissionProfile::Managed { .. }
        )
        && !policy.has_full_disk_write_access()
        && !policy.can_write_path_with_cwd(config.codex_home.as_path(), config.cwd.as_path())
        && registry_root()
            .is_ok_and(|registry| !policy.can_write_path_with_cwd(&registry, config.cwd.as_path()))
        && !root
            .ancestors()
            .skip(1)
            .any(|parent| parent.join(".git").exists())
        && policy
            .entries
            .iter()
            .filter(|entry| entry.access.can_write())
            .all(|entry| {
                match &entry.path {
                    FileSystemPath::Path { path } => path.starts_with(root),
                    // A shared temp grant can contain another session's checkout.
                    _ => false,
                }
            })
}

/// ACL setup canonicalizes write grants. Pin their directory chains so an
/// in-checkout alias cannot grant access to a peer or be retargeted after admission.
fn guard_write_roots(config: &crate::config::Config) -> std::io::Result<Vec<std::fs::File>> {
    use codex_protocol::permissions::FileSystemPath;
    use std::os::windows::fs::MetadataExt;
    let mut guards = Vec::new();
    for entry in config.permissions.file_system_sandbox_policy().entries {
        if !entry.access.can_write() {
            continue;
        }
        let FileSystemPath::Path { path } = entry.path else {
            return Err(std::io::Error::other(
                "shared writes require explicit checkout paths",
            ));
        };
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(std::io::Error::other(
                "shared write roots cannot be reparse points",
            ));
        }
        if !metadata.is_dir() {
            return Err(std::io::Error::other(
                "shared write roots must be existing directories",
            ));
        }
        guards.extend(codex_file_system::lock_directory_chain(path.as_path())?);
    }
    Ok(guards)
}

fn create_report_directory(root: &Path) -> std::io::Result<(PathBuf, Vec<std::fs::File>)> {
    let mut path = std::fs::canonicalize(root)?;
    let mut guards = codex_file_system::lock_directory_chain(&path)?;
    for component in [
        "audits".to_owned(),
        "runs".to_owned(),
        Uuid::now_v7().to_string(),
    ] {
        path.push(component);
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        // Pin before creating the next child; never follow a redirected parent.
        guards.extend(codex_file_system::lock_directory_chain(&path)?);
    }
    Ok((path, guards))
}

fn grant_report_permissions(
    config: &mut crate::config::Config,
    reports: &Path,
) -> Result<(), FunctionCallError> {
    use codex_protocol::permissions::{
        FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry,
    };
    let mut policy = config.permissions.file_system_sandbox_policy();
    if policy.can_write_path_with_cwd(reports, config.cwd.as_path()) {
        return Ok(());
    }
    policy.entries.push(FileSystemSandboxEntry {
        path: FileSystemPath::Path {
            path: codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(reports)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?,
        },
        access: FileSystemAccessMode::Write,
    });
    let profile = codex_protocol::models::PermissionProfile::from_runtime_permissions(
        &policy,
        config.permissions.network_sandbox_policy(),
    );
    config
        .permissions
        .set_permission_profile(profile)
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "workspace report permission refused: {error}"
            ))
        })
}

fn narrow_workspace_permissions(
    config: &mut crate::config::Config,
    source: &Path,
    root: &Path,
    writable: bool,
    reports: &Path,
) -> Result<(), FunctionCallError> {
    use codex_protocol::permissions::{
        FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSandboxPolicy,
        FileSystemSpecialPath,
    };
    let root = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(root)
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    let reports = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(reports)
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    let original = config.permissions.file_system_sandbox_policy();
    let mut policy = original.clone();
    for entry in &mut policy.entries {
        if entry.access.can_write() {
            entry.access = FileSystemAccessMode::Read;
        }
    }
    if writable && original.has_full_disk_write_access() {
        let own = FileSystemSandboxPolicy::workspace_write(std::slice::from_ref(&root), true, true)
            .materialize_project_roots_with_workspace_roots(std::slice::from_ref(&root));
        policy
            .entries
            .extend(own.entries.into_iter().filter(|entry| {
                !matches!(
                    entry.path,
                    FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root
                    }
                )
            }));
    }
    // Preserve source read denials and metadata carveouts in the copied checkout too.
    for mut entry in original.entries {
        if let FileSystemPath::Path { path } = &entry.path
            && let Ok(relative) = path.strip_prefix(source)
        {
            entry.path = FileSystemPath::Path {
                path: codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
                    root.join(relative),
                )
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?,
            };
            if entry.access.can_write() && !writable {
                continue;
            }
            policy.entries.push(entry);
        }
    }
    // Reports are one host-selected run directory, never another checkout.
    if writable {
        policy.entries.push(FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: reports },
            access: FileSystemAccessMode::Write,
        });
    }
    let profile = codex_protocol::models::PermissionProfile::from_runtime_permissions(
        &policy,
        config.permissions.network_sandbox_policy(),
    );
    config
        .permissions
        .set_permission_profile(profile)
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "workspace confinement refused by permission requirements: {error}"
            ))
        })?;
    config.workspace_roots = vec![root.clone()];
    config.permissions.set_workspace_roots(vec![root]);
    Ok(())
}

/// Shared admissions are possible only for already-confined writers. An unrestricted
/// incumbent cannot be tightened while its children run; conflicting launches refuse.
pub(crate) async fn prepare_workspace_launch(
    config: &mut crate::config::Config,
    environments: &mut [codex_protocol::protocol::TurnEnvironmentSelection],
    writable: bool,
    force_isolated: bool,
    cancellation: &CancellationToken,
    terminal_tasks: &TaskTracker,
) -> Result<Option<WorkspaceLaunch>, FunctionCallError> {
    let remote = environments.first().is_some_and(|environment| {
        environment.environment_id != codex_exec_server::LOCAL_ENVIRONMENT_ID
    });
    let cwd = config.cwd.to_path_buf();
    let root = if remote {
        None
    } else {
        tokio::task::spawn_blocking(move || {
            crate::git_workspace::WorkspaceRootCache::default().resolve(&cwd)
        })
        .await
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!("workspace admission: {error}"))
        })?
    };
    let supported_repository = root.is_some();
    let source_root = root.unwrap_or_else(|| config.cwd.to_path_buf());
    let registry =
        registry_root().map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    let admission_path = registry.join("admission.lock");
    let concurrency_path = registry.join("concurrency.lock");
    let storage = registry.clone();
    let lock_path = admission_path.clone();
    let admission = tokio::task::spawn_blocking(move || admission_lock(&lock_path))
        .await
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    let concurrency = if writable {
        Some(
            open_lock(&concurrency_path)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?,
        )
    } else {
        None
    };
    let mut directory_guards =
        if writable && supported_repository && shareable(config, &source_root) {
            guard_write_roots(config).ok()
        } else {
            None
        };
    let eligible = directory_guards.is_some();
    let mut exclusive = false;
    let mut cohort_contended = false;
    if let Some(file) = &concurrency {
        let result = if eligible {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        match result {
            Ok(()) => exclusive = !eligible,
            Err(std::fs::TryLockError::WouldBlock) => {
                cohort_contended = true;
                // Only a previously confined cohort can admit another writer.
                if !supported_repository || !confinement_supported(config) {
                    return Err(FunctionCallError::RespondToModel(
                        "conflicting launch refused: shell/filesystem confinement is unavailable; no shared fallback".into()));
                }
                file.try_lock_shared().map_err(|_| FunctionCallError::RespondToModel(
                    "conflicting launch refused: an incumbent has unrestricted or expanded permissions; wait for it to shut down".into()))?;
            }
            Err(error) => return Err(FunctionCallError::RespondToModel(error.to_string())),
        }
    }
    let mut ownership = if writable && supported_repository {
        Some(
            ownership_file(&source_root)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?,
        )
    } else {
        None
    };
    let contended = match ownership.as_ref().map(std::fs::File::try_lock) {
        None | Some(Ok(())) => false,
        Some(Err(std::fs::TryLockError::WouldBlock)) => true,
        Some(Err(error)) => return Err(FunctionCallError::RespondToModel(error.to_string())),
    };
    let isolate = supported_repository
        && (force_isolated || (writable && (contended || (cohort_contended && !eligible))));
    if isolate && writable && !confinement_supported(config) {
        return Err(FunctionCallError::RespondToModel(
            "conflicting launch refused: no enforceable shell/filesystem sandbox".into(),
        ));
    }
    // Reserve source ownership before releasing admission; snapshots carry their own UUID.
    if isolate {
        ownership = None;
    }
    drop(admission);
    let snapshot = if isolate {
        let workspace = create_isolated_worktree(
            &source_root,
            &storage,
            "session",
            cancellation,
            terminal_tasks,
        )
        .await?;
        let lock = ownership_file(&workspace.path)
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        lock.try_lock()
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        ownership = Some(lock);
        Some(workspace)
    } else {
        None
    };
    let workspace_root = snapshot
        .as_ref()
        .map_or_else(|| source_root.clone(), |value| value.path.clone());
    let (report_directory, report_guards) = if writable {
        // This bounded directory-only operation has no detached writer that can
        // outlive cancellation and race snapshot rollback.
        create_report_directory(&workspace_root).map_err(|error| {
            FunctionCallError::RespondToModel(format!("workspace reports: {error}"))
        })?
    } else {
        let path = registry
            .join("audit-reports")
            .join(Uuid::now_v7().to_string());
        tokio::fs::create_dir_all(&path).await.map_err(|error| {
            FunctionCallError::RespondToModel(format!("workspace reports: {error}"))
        })?;
        (path, Vec::new())
    };
    if let Some(snapshot) = &snapshot {
        retain_snapshot_receipt(snapshot, &report_directory, cancellation, terminal_tasks).await?;
        let relative = config
            .cwd
            .strip_prefix(&source_root)
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        config.cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
            workspace_root.join(relative),
        )
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        for environment in environments {
            if environment.environment_id == codex_exec_server::LOCAL_ENVIRONMENT_ID {
                if let Ok(cwd) = environment.cwd.to_abs_path()
                    && let Ok(relative) = cwd.strip_prefix(&source_root)
                {
                    let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
                        workspace_root.join(relative),
                    )
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                    environment.cwd = codex_utils_path_uri::PathUri::from_abs_path(&cwd);
                }
            }
        }
    }
    if isolate || !writable || (!exclusive && !eligible) {
        narrow_workspace_permissions(
            config,
            &source_root,
            &workspace_root,
            writable,
            &report_directory,
        )?;
        if writable {
            directory_guards = Some(guard_write_roots(config).map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "workspace write-root confinement: {error}"
                ))
            })?);
        }
    } else if writable {
        // Keep narrow checkout grants intact; add only the host-selected report leaf.
        grant_report_permissions(config, &report_directory)?;
    }
    let mut directory_guards = directory_guards.unwrap_or_default();
    directory_guards.extend(report_guards);
    // Do not allow a narrow-root launch to opt back into its parent's roots.
    let context = format!(
        "Checkout: {}. Run-unique reports: {}. Source: {}. Isolated changes require conflict-checked integration after source ownership is released.",
        workspace_root.display(),
        report_directory.display(),
        source_root.display()
    );
    config.developer_instructions = Some(match config.developer_instructions.take() {
        Some(existing) => format!("{existing}\n\n{context}"),
        None => context,
    });
    Ok(Some(WorkspaceLaunch {
        _ownership: ownership,
        concurrency: concurrency.map(std::sync::Mutex::new),
        admission_path,
        exclusive: AtomicBool::new(exclusive),
        base_permissions: config.permissions.effective_permission_profile(),
        directory_guards: std::sync::Mutex::new(directory_guards),
        source_root,
        workspace_root,
        report_directory,
        snapshot,
    }))
}
async fn retain_snapshot_receipt(
    workspace: &IsolatedWorkspace,
    reports: &Path,
    cancellation: &CancellationToken,
    tasks: &TaskTracker,
) -> Result<(), FunctionCallError> {
    let absolute = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&workspace.path)
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    let (git_dir, _) = codex_command_analysis::git_dirs::resolve_git_directories(&absolute)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel("isolated Git directory disappeared".into())
        })?;
    let scratch = tempfile::tempdir_in(&git_dir)
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    let index = scratch.path().join("index");
    tokio::fs::copy(git_dir.join("index"), &index)
        .await
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    let mut outputs = Vec::new();
    for args in [
        &["add", "-A", "--", "."][..],
        &["write-tree"][..],
        &["rev-parse", "HEAD"][..],
    ] {
        let mut command = Command::new(codex_git_utils::git_executable_async().await);
        command
            .arg("-C")
            .arg(&workspace.path)
            .env("GIT_INDEX_FILE", &index)
            .arg("-c")
            .arg(format!(
                "core.hooksPath={}",
                codex_git_utils::DISABLED_HOOKS_PATH
            ))
            .args(args);
        let result = run_worktree_git(command, None, cancellation, tasks)
            .await
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        if !result.status.success() {
            return Err(FunctionCallError::RespondToModel(format!(
                "workspace baseline capture failed: {}",
                String::from_utf8_lossy(&result.stderr)
            )));
        }
        outputs.push(String::from_utf8_lossy(&result.stdout).trim().to_owned());
    }
    let mut command = Command::new(codex_git_utils::git_executable_async().await);
    command
        .arg("-C")
        .arg(&workspace.path)
        .arg("-c")
        .arg(format!(
            "core.hooksPath={}",
            codex_git_utils::DISABLED_HOOKS_PATH
        ))
        .args(["-c", "commit.gpgsign=false"])
        .env("GIT_AUTHOR_NAME", "Codex")
        .env("GIT_AUTHOR_EMAIL", "codex@localhost")
        .env("GIT_COMMITTER_NAME", "Codex")
        .env("GIT_COMMITTER_EMAIL", "codex@localhost")
        .args([
            "commit-tree",
            &outputs[1],
            "-p",
            &outputs[2],
            "-m",
            "Codex workspace snapshot",
        ]);
    let result = run_worktree_git(command, None, cancellation, tasks)
        .await
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    if !result.status.success() {
        return Err(FunctionCallError::RespondToModel(format!(
            "workspace snapshot commit failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    let snapshot_commit = String::from_utf8_lossy(&result.stdout).trim().to_owned();
    // A per-worktree ref keeps the dirty baseline reachable without changing HEAD,
    // the shared index, or the child's captured working files.
    let snapshot_ref = "refs/worktree/codex-workspace-snapshot";
    let mut command = Command::new(codex_git_utils::git_executable_async().await);
    command
        .arg("-C")
        .arg(&workspace.path)
        .arg("-c")
        .arg(format!(
            "core.hooksPath={}",
            codex_git_utils::DISABLED_HOOKS_PATH
        ))
        .args(["update-ref", snapshot_ref, &snapshot_commit]);
    let result = run_worktree_git(command, None, cancellation, tasks)
        .await
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    if !result.status.success() {
        return Err(FunctionCallError::RespondToModel(format!(
            "workspace snapshot retention failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    let receipt = serde_json::json!({"schema":1,"owner":"codex_workspace",
        "name":workspace.path.file_name().and_then(|name| name.to_str()),
        "repo":workspace.main_repo_root,"workspace":workspace.path,"head":outputs[2],
        "snapshot_commit":snapshot_commit,"snapshot_tree":outputs[1],"snapshot_ref":snapshot_ref,"linked_storage":[],
        "report_directory":reports,"created_utc":chrono::Utc::now().to_rfc3339(),"deliveries":[]});
    tokio::fs::write(
        git_dir.join("codex-workspace.json"),
        serde_json::to_vec_pretty(&receipt)
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?,
    )
    .await
    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))
}

const MAX_OVERLAY_SNAPSHOT_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OVERLAY_SNAPSHOT_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_WORKTREE_GIT_STDOUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_WORKTREE_GIT_STDERR_BYTES: u64 = 1024 * 1024;
const WORKTREE_GIT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub(crate) struct IsolatedWorkspace {
    pub(crate) main_repo_root: PathBuf,
    pub(crate) path: PathBuf,
    pub(crate) cleanup_required: AtomicBool,
    terminal_tasks: TaskTracker,
}

impl Drop for IsolatedWorkspace {
    fn drop(&mut self) {
        if !self.cleanup_required.swap(false, Ordering::AcqRel) {
            return;
        }
        let repo_root = self.main_repo_root.clone();
        let path = self.path.clone();
        // The handler can be dropped between two awaited admission steps.
        // Preserve rollback ownership even when there is no caller left to await it.
        let terminal_tasks = self.terminal_tasks.clone();
        self.terminal_tasks.spawn(async move {
            if let Err(error) = cleanup_isolated_worktree_paths(&repo_root, &path, &terminal_tasks).await {
                tracing::warn!(path = %path.display(), %error, "failed to roll back isolated worktree");
            }
        });
    }
}

#[derive(Debug, Eq, PartialEq)]
struct WorkspaceOverlay {
    head: String,
    tracked_diff: Vec<u8>,
    working_files: Vec<(PathBuf, Vec<u8>)>,
}

pub(crate) async fn create_isolated_worktree(
    repo_root: &Path,
    codex_home: &Path,
    task_name: &str,
    cancellation_token: &CancellationToken,
    terminal_tasks: &TaskTracker,
) -> Result<IsolatedWorkspace, FunctionCallError> {
    let mut initial_overlay =
        capture_workspace_overlay(repo_root, cancellation_token, terminal_tasks).await?;
    let repository_key = hex::encode(Sha256::digest(repo_root.to_string_lossy().as_bytes()));
    let safe_task_name = task_name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(24)
        .collect::<String>();
    let leaf = format!(
        "{}-{}",
        if safe_task_name.is_empty() {
            "task"
        } else {
            &safe_task_name
        },
        Uuid::now_v7()
    );
    let parent = codex_home
        .join("isolated-worktrees")
        .join(&repository_key[..16]);
    tokio::fs::create_dir_all(&parent).await.map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "spawn_agent: could not create isolated-worktree directory: {error}"
        ))
    })?;
    let path = parent.join(leaf);
    let workspace = Arc::new(IsolatedWorkspace {
        main_repo_root: repo_root.to_path_buf(),
        path,
        cleanup_required: AtomicBool::new(true),
        terminal_tasks: terminal_tasks.clone(),
    });
    let mut command = Command::new(codex_git_utils::git_executable_async().await);
    command
        .arg("-C")
        .arg(repo_root)
        .args(["-c", "checkout.workers=0"])
        .arg("-c")
        .arg(format!(
            "core.hooksPath={}",
            codex_git_utils::DISABLED_HOOKS_PATH
        ))
        .args(["worktree", "add", "--detach"])
        .arg(&workspace.path)
        .arg(&initial_overlay.head);
    let output = run_worktree_git(command, None, cancellation_token, terminal_tasks).await;
    let error = match output {
        Ok(output) if output.status.success() => None,
        Ok(output) => Some(format!(
            "spawn_agent: git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(error) => Some(format!(
            "spawn_agent: could not complete git worktree add: {error}"
        )),
    };
    if let Some(error) = error {
        if let Err(cleanup_error) = cleanup_isolated_worktree(&workspace).await {
            tracing::warn!(path = %workspace.path.display(), %cleanup_error, "failed to clean isolated worktree after checkout failure");
        }
        return Err(FunctionCallError::RespondToModel(error));
    }
    let populate_result = Box::pin(async {
        // Nothing has been applied yet. Absorb a short writer burst here rather
        // than spending a model generation recreating the same empty worktree.
        for attempt in 0..=2 {
            let current_overlay = capture_workspace_overlay(repo_root, cancellation_token, terminal_tasks).await?;
            if current_overlay == initial_overlay {
                break;
            }
            if attempt == 2 || current_overlay.head != initial_overlay.head {
                return Err(FunctionCallError::RespondToModel(
                    "spawn_agent: the shared worktree did not stabilize during bounded snapshot retries; retry after the current writer finishes".to_string(),
                ));
            }
            initial_overlay = current_overlay;
            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel(
                    "spawn_agent: cancelled while waiting for a stable workspace snapshot".into())),
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        }
        let expected_overlay = Arc::new(initial_overlay);
        if !expected_overlay.tracked_diff.is_empty() {
            let mut command = Command::new(codex_git_utils::git_executable_async().await);
            // Keep snapshot additions in the child's tracked-path inventory so
            // validation can compare the overlay independently of parent staging.
            command.arg("-C")
                .arg(&workspace.path)
                .args(["apply", "--intent-to-add", "--binary", "--whitespace=nowarn", "-"]);
            let output = run_worktree_git(command, Some(&expected_overlay.tracked_diff), cancellation_token, terminal_tasks)
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "spawn_agent: could not apply the shared-worktree snapshot: {error}"
                    ))
                })?;
            if !output.status.success() {
                return Err(FunctionCallError::RespondToModel(format!(
                    "spawn_agent: isolated snapshot apply failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        use futures::StreamExt;
        let copies = futures::stream::iter((0..expected_overlay.working_files.len()).map(|index| {
            let workspace = Arc::clone(&workspace);
            let expected_overlay = Arc::clone(&expected_overlay);
            async move {
                tokio::task::spawn_blocking(move || {
                    // Keep rollback ownership in the blocking worker, including
                    // when the caller is cancelled while this write is in flight.
                    let (relative_path, bytes) = &expected_overlay.working_files[index];
                    let target = workspace.path.join(relative_path);
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent).map_err(|error| {
                            FunctionCallError::RespondToModel(format!(
                                "spawn_agent: could not create an isolated snapshot directory: {error}"
                            ))
                        })?;
                    }
                    std::fs::write(&target, bytes).map_err(|error| {
                        FunctionCallError::RespondToModel(format!(
                            "spawn_agent: could not copy a working file into the isolated snapshot: {error}"
                        ))
                    })
                })
                .await
                .map_err(|error| FunctionCallError::RespondToModel(format!(
                    "spawn_agent: isolated snapshot copy worker failed: {error}"
                )))?
            }
        }))
        .buffer_unordered(std::thread::available_parallelism().map_or(4, usize::from))
        .collect::<Vec<_>>()
        .await;
        // Drain every write before rollback: dropping an in-flight Tokio file
        // operation does not cancel the underlying blocking filesystem call.
        for result in copies {
            result?;
        }
        let isolated_overlay =
            capture_workspace_overlay(&workspace.path, cancellation_token, terminal_tasks).await?;
        if isolated_overlay != *expected_overlay {
            return Err(FunctionCallError::RespondToModel(
                "spawn_agent: the isolated worktree does not match the captured source snapshot".into(),
            ));
        }
        Ok(())
    })
    .await;
    if let Err(error) = populate_result {
        if let Err(cleanup_error) = cleanup_isolated_worktree(&workspace).await {
            tracing::warn!(
                path = %workspace.path.display(),
                %cleanup_error,
                "failed to clean isolated worktree after snapshot failure"
            );
        }
        return Err(error);
    }
    Arc::into_inner(workspace).ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "spawn_agent: isolated snapshot copy is still in progress".to_string(),
        )
    })
}

async fn capture_workspace_overlay(
    repo_root: &Path,
    cancellation_token: &CancellationToken,
    terminal_tasks: &TaskTracker,
) -> Result<WorkspaceOverlay, FunctionCallError> {
    let mut index_command = Command::new(codex_git_utils::git_executable_async().await);
    index_command
        .arg("-C")
        .arg(repo_root)
        .args(["ls-files", "--stage", "-z"]);
    let index = run_worktree_git(index_command, None, cancellation_token, terminal_tasks)
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "spawn_agent: snapshot index capture failed: {error}"
            ))
        })?;
    if !index.status.success() {
        return Err(FunctionCallError::RespondToModel(
            "spawn_agent: could not inspect snapshot index".into(),
        ));
    }
    if index
        .stdout
        .split(|byte| *byte == 0)
        .any(|entry| entry.starts_with(b"160000 "))
    {
        return Err(FunctionCallError::RespondToModel(
            "spawn_agent: unsupported_snapshot_submodule: isolated snapshots do not materialize submodules; use a shared workspace or an explicitly prepared checkout".into(),
        ));
    }
    let mut head_command = Command::new(codex_git_utils::git_executable_async().await);
    head_command
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--verify", "HEAD"]);
    let head = run_worktree_git(head_command, None, cancellation_token, terminal_tasks)
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "spawn_agent: could not capture snapshot HEAD: {error}"
            ))
        })?;
    if !head.status.success() {
        return Err(FunctionCallError::RespondToModel(
            "spawn_agent: could not resolve snapshot HEAD".into(),
        ));
    }
    let head = String::from_utf8(head.stdout).map_err(|error| {
        FunctionCallError::RespondToModel(format!("spawn_agent: invalid snapshot HEAD: {error}"))
    })?;
    let head = head.trim().to_string();
    let mut diff_command = Command::new(codex_git_utils::git_executable_async().await);
    diff_command.arg("-C").arg(repo_root).args([
        "diff",
        "--binary",
        "--no-ext-diff",
        "--no-textconv",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        &head,
        "--",
    ]);
    let mut untracked_command = Command::new(codex_git_utils::git_executable_async().await);
    untracked_command.arg("-C").arg(repo_root).args([
        "ls-files",
        "--others",
        "--exclude-standard",
        "-z",
    ]);
    let mut tracked_paths_command = Command::new(codex_git_utils::git_executable_async().await);
    tracked_paths_command.arg("-C").arg(repo_root).args([
        "diff",
        "--name-only",
        "--no-renames",
        "--diff-filter=ACMRT",
        "-z",
        &head,
        "--",
    ]);
    let (diff, untracked, tracked_paths) = tokio::join!(
        run_worktree_git(diff_command, None, cancellation_token, terminal_tasks),
        run_worktree_git(untracked_command, None, cancellation_token, terminal_tasks),
        run_worktree_git(
            tracked_paths_command,
            None,
            cancellation_token,
            terminal_tasks
        ),
    );
    let diff = diff.map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "spawn_agent: could not capture tracked workspace changes: {error}"
        ))
    })?;
    if !diff.status.success() {
        return Err(FunctionCallError::RespondToModel(format!(
            "spawn_agent: git diff failed while creating an isolated snapshot: {}",
            String::from_utf8_lossy(&diff.stderr).trim()
        )));
    }
    let untracked = untracked.map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "spawn_agent: could not enumerate untracked workspace files: {error}"
        ))
    })?;
    if !untracked.status.success() {
        return Err(FunctionCallError::RespondToModel(format!(
            "spawn_agent: git ls-files failed while creating an isolated snapshot: {}",
            String::from_utf8_lossy(&untracked.stderr).trim()
        )));
    }
    let tracked_paths = tracked_paths.map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "spawn_agent: could not enumerate changed tracked files: {error}"
        ))
    })?;
    if !tracked_paths.status.success() {
        return Err(FunctionCallError::RespondToModel(format!(
            "spawn_agent: git diff could not enumerate changed tracked files: {}",
            String::from_utf8_lossy(&tracked_paths.stderr).trim()
        )));
    }
    let repo_root = repo_root.to_path_buf();
    let cancellation = cancellation_token.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    // One tracked blocking job per capture, not metadata/open/read jobs for every
    // file. Observer drops cancel read-only work between files.
    let working_files = terminal_tasks
        .spawn_blocking(move || {
            let paths = untracked
                .stdout
                .split(|byte| *byte == 0)
                .map(|path| (path, false))
                .chain(
                    tracked_paths
                        .stdout
                        .split(|byte| *byte == 0)
                        .map(|path| (path, true)),
                );
            capture_working_file_snapshot(&repo_root, paths, &cancellation)
        })
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "spawn_agent: working file snapshot worker failed: {error}"
            ))
        })??;
    Ok(WorkspaceOverlay {
        head,
        tracked_diff: diff.stdout,
        working_files,
    })
}

fn capture_working_file_snapshot<'a>(
    repo_root: &Path,
    paths: impl IntoIterator<Item = (&'a [u8], bool)>,
    cancellation_token: &CancellationToken,
) -> Result<Vec<(PathBuf, Vec<u8>)>, FunctionCallError> {
    use std::io::Read;

    let mut working_files = Vec::new();
    let mut total_file_bytes = 0u64;
    // Git patches handle deletes, modes and tracked symlinks. Preserve the
    // original regular-file bytes, including under clean filters/CRLF settings.
    for (raw_path, tracked) in paths {
        if cancellation_token.is_cancelled() {
            return Err(FunctionCallError::RespondToModel(
                "spawn_agent: working file snapshot cancelled".to_string(),
            ));
        }
        if raw_path.is_empty() {
            continue;
        }
        let path = PathBuf::from(String::from_utf8(raw_path.to_vec()).map_err(|_| {
            FunctionCallError::RespondToModel(
                "spawn_agent: a working file path is not valid UTF-8".to_string(),
            )
        })?);
        let source = repo_root.join(&path);
        let metadata = std::fs::symlink_metadata(&source).map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "spawn_agent: could not inspect working file {}: {error}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            if tracked {
                continue;
            }
            return Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: isolated snapshots reject untracked symlinks and special files: {}",
                path.display()
            )));
        }
        if metadata.len() > MAX_OVERLAY_SNAPSHOT_FILE_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: working file {} is too large for an isolated snapshot ({} bytes, max {})",
                path.display(),
                metadata.len(),
                MAX_OVERLAY_SNAPSHOT_FILE_BYTES
            )));
        }
        total_file_bytes = total_file_bytes
            .checked_add(metadata.len())
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "spawn_agent: working file snapshot size overflowed".to_string(),
                )
            })?;
        if total_file_bytes > MAX_OVERLAY_SNAPSHOT_TOTAL_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: working files exceed the isolated snapshot limit of {MAX_OVERLAY_SNAPSHOT_TOTAL_BYTES} bytes"
            )));
        }
        let file = std::fs::File::open(&source).map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "spawn_agent: could not snapshot working file {}: {error}",
                path.display()
            ))
        })?;
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        file.take(MAX_OVERLAY_SNAPSHOT_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "spawn_agent: could not snapshot working file {}: {error}",
                    path.display()
                ))
            })?;
        if bytes.len() as u64 != metadata.len() {
            return Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: working file {} changed while the isolated snapshot was captured",
                path.display()
            )));
        }
        working_files.push((path, bytes));
    }
    working_files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(working_files)
}

#[cfg(unix)]
struct WorktreeProcessGroup(u32);

#[cfg(unix)]
impl Drop for WorktreeProcessGroup {
    fn drop(&mut self) {
        let _ = codex_utils_pty::process_group::kill_process_group(self.0);
    }
}

#[expect(
    clippy::expect_used,
    reason = "Successful spawn creates both handles configured with Stdio::piped"
)]
async fn run_worktree_git(
    mut command: Command,
    input: Option<&[u8]>,
    cancellation_token: &CancellationToken,
    terminal_tasks: &TaskTracker,
) -> std::io::Result<std::process::Output> {
    command
        .kill_on_drop(true)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(codex_utils_pty::WINDOWS_CREATE_SUSPENDED);
    let input = input.map(<[u8]>::to_vec);
    let cancellation = cancellation_token.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    // A session-owned worker retains the process through termination and reaping,
    // including cancellation while Command::spawn is on the blocking pool.
    terminal_tasks.spawn(async move {
        #[cfg(windows)]
        let managed = codex_utils_pty::ManagedRootProcess::reserve_with_reclaim().await?;
        #[cfg(windows)]
        managed.require_descendant_containment()?;
        if cancellation.is_cancelled() {
            return Err(std::io::Error::new(ErrorKind::Interrupted, "worktree Git command cancelled"));
        }
        let mut child = tokio::task::spawn_blocking(move || command.spawn())
            .await.map_err(std::io::Error::other)??;
        let pid = child.id().ok_or_else(|| std::io::Error::other("missing worktree Git pid"))?;
        #[cfg(unix)]
        let _process_group = WorktreeProcessGroup(pid);
        #[cfg(windows)]
        if let Err(error) = managed.attach_and_resume(pid) {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(error);
        }
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().expect("piped worktree stdout");
        let mut stderr = child.stderr.take().expect("piped worktree stderr");
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        let result = {
            let operation = async {
                let write_input = async {
                    if let (Some(mut stdin), Some(input)) = (stdin, input) {
                        stdin.write_all(&input).await?;
                        stdin.shutdown().await?;
                    }
                    Ok::<(), std::io::Error>(())
                };
                let (_, _, _, status) = tokio::try_join!(write_input,
                    read_worktree_output(&mut stdout, &mut stdout_bytes, MAX_WORKTREE_GIT_STDOUT_BYTES),
                    read_worktree_output(&mut stderr, &mut stderr_bytes, MAX_WORKTREE_GIT_STDERR_BYTES), child.wait())?;
                Ok(status)
            };
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(std::io::Error::new(ErrorKind::Interrupted, "worktree Git command cancelled")),
                result = tokio::time::timeout(WORKTREE_GIT_TIMEOUT, operation) => {
                    result.unwrap_or_else(|_| Err(std::io::Error::new(ErrorKind::TimedOut, "worktree Git command timed out")))
                }
            }
        };
        if result.is_err() {
            #[cfg(windows)]
            let _ = managed.terminate();
            #[cfg(unix)]
            let _ = codex_utils_pty::process_group::kill_process_group(pid);
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        result.map(|status| std::process::Output {status, stdout: stdout_bytes, stderr: stderr_bytes})
    }).await.map_err(std::io::Error::other)?
}

async fn read_worktree_output(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    bytes: &mut Vec<u8>,
    limit: u64,
) -> std::io::Result<()> {
    reader.take(limit + 1).read_to_end(bytes).await?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::other(
            "snapshot_git_output_limit: Git output exceeded its independent byte budget",
        ));
    }
    Ok(())
}

pub(crate) async fn cleanup_isolated_worktree(
    workspace: &IsolatedWorkspace,
) -> Result<(), std::io::Error> {
    let result = cleanup_isolated_worktree_paths(
        &workspace.main_repo_root,
        &workspace.path,
        &workspace.terminal_tasks,
    )
    .await;
    if result.is_ok() {
        workspace.cleanup_required.store(false, Ordering::Release);
    }
    result
}

async fn cleanup_isolated_worktree_paths(
    repo_root: &Path,
    path: &Path,
    terminal_tasks: &TaskTracker,
) -> std::io::Result<()> {
    let mut command = Command::new(codex_git_utils::git_executable_async().await);
    command
        .arg("-C")
        .arg(repo_root)
        .arg("-c")
        .arg(format!(
            "core.hooksPath={}",
            codex_git_utils::DISABLED_HOOKS_PATH
        ))
        .args(["worktree", "remove", "--force"])
        .arg(path);
    let output = run_worktree_git(command, None, &CancellationToken::new(), terminal_tasks).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
