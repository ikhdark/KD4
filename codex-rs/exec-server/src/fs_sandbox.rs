use std::collections::HashMap;

use codex_exec_server_protocol::JSONRPCErrorError;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxDirectSpawnTransformRequest;
use codex_sandboxing::SandboxExecRequest;
use codex_sandboxing::SandboxTransformRequest;
use codex_sandboxing::SandboxType;
use codex_sandboxing::SandboxablePreference;
use codex_sandboxing::select_initial;
use codex_sandboxing::transform_for_direct_spawn;
use codex_sandboxing::transform_for_direct_spawn_with_codex_home;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::canonicalize_preserving_symlinks;
use codex_utils_path_uri::PathUri;

use crate::ExecServerRuntimePaths;
use crate::FileSystemSandboxContext;
use crate::fs_helper::CODEX_FS_HELPER_ARG1;
use crate::fs_helper::FsHelperPayload;
use crate::fs_helper::FsHelperRequest;
use crate::fs_helper::FsHelperResponse;
use crate::local_file_system::current_sandbox_cwd;
use crate::rpc::internal_error;
use crate::rpc::invalid_request;

const FS_HELPER_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "TMP",
    "TEMP",
    "CODEX_WINDOWS_SANDBOX_DENY_PROVISIONING",
];

#[derive(Debug, PartialEq, Eq)]
struct SandboxCwd {
    uri: PathUri,
    native: AbsolutePathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct FileSystemSandboxRunner {
    runtime_paths: ExecServerRuntimePaths,
    helper_env: HashMap<String, String>,
}

impl FileSystemSandboxRunner {
    pub(crate) fn new(runtime_paths: ExecServerRuntimePaths) -> Self {
        Self {
            runtime_paths,
            helper_env: helper_env(),
        }
    }

    pub(crate) fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.helper_env.extend(env);
        self
    }

    pub(crate) async fn run(
        &self,
        sandbox: &FileSystemSandboxContext,
        request: FsHelperRequest,
    ) -> Result<FsHelperPayload, JSONRPCErrorError> {
        let preparation_cancelled = tokio_util::sync::CancellationToken::new();
        // Only pure preparation is cancellable. Once a helper starts, its
        // existing native/mutation custodian owns settlement and cleanup.
        let preparation_guard = preparation_cancelled.clone().drop_guard();
        let runner = self.clone();
        let sandbox = sandbox.clone();
        let (command, request_json) = tokio::task::spawn_blocking(move || {
            let check_cancelled = || {
                if preparation_cancelled.is_cancelled() {
                    Err(invalid_request("fs sandbox preparation cancelled".to_string()))
                } else {
                    Ok(())
                }
            };
            check_cancelled()?;
            let capture_lock = prepare_capture_lock(&request)?;
            check_cancelled()?;
            let command = runner.prepare_command(&sandbox, capture_lock)?;
            check_cancelled()?;
            let request_json = serde_json::to_vec(&request).map_err(json_error)?;
            check_cancelled()?;
            Ok::<_, JSONRPCErrorError>((command, request_json))
        })
        .await
        .map_err(|error| {
            internal_error(format!("fs sandbox preparation task failed: {error}"))
        })??;
        drop(preparation_guard);
        run_command(command, request_json).await
    }

    fn prepare_command(
        &self,
        sandbox: &FileSystemSandboxContext,
        capture_lock: Option<AbsolutePathBuf>,
    ) -> Result<SandboxExecRequest, JSONRPCErrorError> {
        let cwd = sandbox_cwd(sandbox)?;
        let native_permissions: PermissionProfile =
            sandbox.permissions.clone().try_into().map_err(|err| {
                invalid_request(format!("invalid sandbox permission path URI: {err}"))
            })?;
        let mut file_system_policy = native_permissions.file_system_sandbox_policy();
        let mut helper_read_roots = helper_read_roots(&self.runtime_paths);
        helper_read_roots.extend(capture_lock);
        add_helper_runtime_permissions(
            &mut file_system_policy,
            &helper_read_roots,
            cwd.native.as_path(),
        );
        normalize_file_system_policy_root_aliases(&mut file_system_policy);
        let network_policy = NetworkSandboxPolicy::Restricted;
        let permission_profile = PermissionProfile::from_runtime_permissions_with_enforcement(
            native_permissions.enforcement(),
            &file_system_policy,
            network_policy,
        );
        self.sandbox_exec_request(&permission_profile, &cwd, sandbox)
    }

    fn sandbox_exec_request(
        &self,
        permission_profile: &PermissionProfile,
        cwd: &SandboxCwd,
        sandbox_context: &FileSystemSandboxContext,
    ) -> Result<SandboxExecRequest, JSONRPCErrorError> {
        let helper = &self.runtime_paths.codex_self_exe;
        let (file_system_policy, network_policy) = permission_profile.to_runtime_permissions();
        let sandbox = select_initial(
            &file_system_policy,
            network_policy,
            SandboxablePreference::Auto,
            sandbox_context.windows_sandbox_level,
            /*has_managed_network_requirements*/ false,
        );
        if sandbox == SandboxType::None {
            return Err(invalid_request(
                "sandbox intent cannot be enforced on this executor".to_string(),
            ));
        }
        let command = SandboxCommand {
            program: helper.as_path().as_os_str().to_owned(),
            args: vec![CODEX_FS_HELPER_ARG1.to_string()],
            cwd: cwd.uri.clone(),
            env: self.helper_env.clone(),
            additional_permissions: None,
        };
        let native_workspace_roots = sandbox_context
            .workspace_roots
            .iter()
            .map(native_workspace_root)
            .collect::<Result<Vec<_>, _>>()?;
        let workspace_roots = if native_workspace_roots.is_empty() {
            std::slice::from_ref(&cwd.native)
        } else {
            native_workspace_roots.as_slice()
        };
        let request = SandboxDirectSpawnTransformRequest {
            workspace_roots,
            windows_sandbox_proxy_settings_mode:
                codex_sandboxing::WindowsSandboxProxySettingsMode::Preserve,
            transform: SandboxTransformRequest {
                command,
                permissions: permission_profile,
                sandbox,
                sandbox_policy_cwd: &cwd.uri,
                windows_sandbox_level: sandbox_context.windows_sandbox_level,
                windows_sandbox_private_desktop: sandbox_context.windows_sandbox_private_desktop,
            },
        };
        // Keep wrapper ACL ownership and the helper's trusted runtime environment
        // on the same home. Per-instance overrides must not use the parent home.
        let transformed = match self.helper_env.get("CODEX_HOME") {
            Some(home) => {
                transform_for_direct_spawn_with_codex_home(request, std::path::Path::new(home))
            }
            None => transform_for_direct_spawn(request),
        };
        transformed.map_err(|err| invalid_request(format!("failed to prepare fs sandbox: {err}")))
    }
}

fn prepare_capture_lock(
    request: &FsHelperRequest,
) -> Result<Option<AbsolutePathBuf>, JSONRPCErrorError> {
    // Source I/O stays in the sandbox. The parent initializes only the empty,
    // temp-side coordination file so the helper never needs write permissions
    // outside its policy, including on a path's very first read or mutation.
    let path = match request {
        FsHelperRequest::ReadFile(params) => &params.path,
        FsHelperRequest::ReadFileCapped { path, .. } => path,
        FsHelperRequest::ReadFileBounded(params)
        | FsHelperRequest::ReadFileBoundedWithSource(params) => &params.path,
        FsHelperRequest::GetMetadata(params) => &params.path,
        FsHelperRequest::MutateFileIfMatches(params) => &params.path,
        _ => return Ok(None),
    };
    let path = path.to_abs_path().map_err(crate::fs_helper::map_fs_error)?;
    let lock = match codex_file_system::prepare_patch_capture(path.as_path()) {
        Ok(lock) => lock,
        // Preserve the operation's ordinary missing-parent result in the helper.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(crate::fs_helper::map_fs_error(error)),
    };
    AbsolutePathBuf::from_absolute_path(lock)
        .map(Some)
        .map_err(|error| internal_error(error.to_string()))
}

fn sandbox_cwd(sandbox: &FileSystemSandboxContext) -> Result<SandboxCwd, JSONRPCErrorError> {
    if let Some(uri) = &sandbox.cwd {
        return Ok(SandboxCwd {
            native: native_sandbox_cwd(uri)?,
            uri: uri.clone(),
        });
    }

    if sandbox.has_cwd_dependent_permissions() {
        return Err(invalid_request(
            "file system sandbox context with dynamic permissions requires cwd".to_string(),
        ));
    }

    let native = AbsolutePathBuf::from_absolute_path(current_sandbox_cwd().map_err(io_error)?)
        .map_err(|err| invalid_request(format!("current directory is not absolute: {err}")))?;
    let uri = PathUri::from_abs_path(&native);
    Ok(SandboxCwd { uri, native })
}

fn native_sandbox_cwd(cwd: &PathUri) -> Result<AbsolutePathBuf, JSONRPCErrorError> {
    cwd.to_abs_path()
        .map_err(|err| invalid_request(err.to_string()))
}

fn native_workspace_root(root: &PathUri) -> Result<AbsolutePathBuf, JSONRPCErrorError> {
    root.to_abs_path().map_err(|err| {
        invalid_request(format!(
            "file system sandbox workspace root is not native to this exec-server host: {err}"
        ))
    })
}

fn helper_read_roots(runtime_paths: &ExecServerRuntimePaths) -> Vec<AbsolutePathBuf> {
    let mut roots = Vec::new();
    for path in std::iter::once(runtime_paths.codex_self_exe.as_path()) {
        if let Some(parent) = path.parent()
            && let Ok(root) = AbsolutePathBuf::from_absolute_path(parent)
            && !roots.contains(&root)
        {
            roots.push(root);
        }
    }
    roots
}

fn add_helper_runtime_permissions(
    file_system_policy: &mut FileSystemSandboxPolicy,
    helper_read_roots: &[AbsolutePathBuf],
    cwd: &std::path::Path,
) {
    if !file_system_policy.has_full_disk_read_access() {
        let minimal_read_entry = FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Minimal,
            },
            access: FileSystemAccessMode::Read,
        };
        if !file_system_policy.entries.contains(&minimal_read_entry) {
            file_system_policy.entries.push(minimal_read_entry);
        }
    }

    for helper_read_root in helper_read_roots {
        if file_system_policy.can_read_path_with_cwd(helper_read_root.as_path(), cwd) {
            continue;
        }

        file_system_policy.entries.push(FileSystemSandboxEntry {
            path: FileSystemPath::Path {
                path: helper_read_root.clone(),
            },
            access: FileSystemAccessMode::Read,
        });
    }
}

fn normalize_file_system_policy_root_aliases(file_system_policy: &mut FileSystemSandboxPolicy) {
    for entry in &mut file_system_policy.entries {
        if let FileSystemPath::Path { path } = &mut entry.path {
            *path = normalize_top_level_alias(path.clone());
        }
    }
}

fn normalize_top_level_alias(path: AbsolutePathBuf) -> AbsolutePathBuf {
    let raw_path = path.to_path_buf();
    for ancestor in raw_path.ancestors() {
        if std::fs::symlink_metadata(ancestor).is_err() {
            continue;
        }
        let Ok(normalized_ancestor) = canonicalize_preserving_symlinks(ancestor) else {
            continue;
        };
        if normalized_ancestor == ancestor {
            continue;
        }
        let Ok(suffix) = raw_path.strip_prefix(ancestor) else {
            continue;
        };
        if let Ok(normalized_path) =
            AbsolutePathBuf::from_absolute_path(normalized_ancestor.join(suffix))
        {
            return normalized_path;
        }
    }
    path
}

fn helper_env() -> HashMap<String, String> {
    helper_env_from_vars(std::env::vars_os())
}

fn helper_env_from_vars(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> HashMap<String, String> {
    vars.into_iter()
        .filter_map(|(key, value)| {
            let key = key.to_string_lossy();
            helper_env_key_is_allowed(&key)
                .then(|| (key.into_owned(), value.to_string_lossy().into_owned()))
        })
        .collect()
}

fn helper_env_key_is_allowed(key: &str) -> bool {
    FS_HELPER_ENV_ALLOWLIST.contains(&key) || key.eq_ignore_ascii_case("PATH")
}

async fn run_command(
    command: SandboxExecRequest,
    request_json: Vec<u8>,
) -> Result<FsHelperPayload, JSONRPCErrorError> {
    let codex_utils_pty::SpawnedProcess {
        session,
        stdout_rx,
        stderr_rx,
        exit_rx,
    } = spawn_command(command).await?;
    session
        .writer_sender()
        .send(request_json)
        .await
        .map_err(|_| internal_error("failed to write fs sandbox helper input".into()))?;
    session.close_stdin();
    let collect = |mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>, limit: usize| async move {
        let mut bytes = Vec::new();
        while let Some(chunk) = rx.recv().await {
            if chunk.len() > limit.saturating_sub(bytes.len()) {
                return Err(internal_error(
                    "fs sandbox helper output exceeds response limit".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    };
    let (stdout, stderr, status) = tokio::try_join!(
        collect(stdout_rx, crate::connection::MAX_JSONRPC_MESSAGE_LEN),
        collect(stderr_rx, 64 * 1024),
        async {
            exit_rx
                .await
                .map_err(|_| internal_error("fs sandbox helper exit was not confirmed".into()))
        },
    )?;
    if !session.output_capture().complete() {
        return Err(internal_error(
            "fs sandbox helper output capture is incomplete; result unavailable".into(),
        ));
    }
    session.finish();
    if status != 0 {
        return Err(internal_error(format!(
            "fs sandbox helper failed with status {status}: {stderr}",
            stderr = String::from_utf8_lossy(&stderr).trim()
        )));
    }
    let response: FsHelperResponse = serde_json::from_slice(&stdout).map_err(json_error)?;
    match response {
        FsHelperResponse::Ok(payload) => Ok(payload),
        FsHelperResponse::Error(error) => Err(error),
    }
}

async fn spawn_command(
    SandboxExecRequest {
        command: argv,
        cwd,
        env,
        arg0,
        ..
    }: SandboxExecRequest,
) -> Result<codex_utils_pty::SpawnedProcess, JSONRPCErrorError> {
    let Some((program, args)) = argv.split_first() else {
        return Err(invalid_request("fs sandbox command was empty".to_string()));
    };
    // TODO(anp): Keep PathUri through the filesystem helper launch boundary.
    let cwd = cwd.to_abs_path().map_err(io_error)?;
    // Reuse the admitted, cancellation-safe pipe owner and its confirmed reaper.
    codex_utils_pty::spawn_pipe_process(program, args, cwd.as_path(), &env, &arg0)
        .await
        .map_err(|error| internal_error(error.to_string()))
}

fn io_error(err: std::io::Error) -> JSONRPCErrorError {
    internal_error(err.to_string())
}

fn json_error(err: serde_json::Error) -> JSONRPCErrorError {
    internal_error(format!(
        "failed to encode or decode fs sandbox helper message: {err}"
    ))
}

