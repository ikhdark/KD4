use super::windows_common::finish_driver_spawn;
use super::windows_common::make_runner_resizer;
use super::windows_common::start_runner_pipe_writer;
use super::windows_common::start_runner_stdin_writer;
use super::windows_common::start_runner_stdout_reader;
use crate::identity::SandboxCreds;
use crate::identity::refresh_logon_sandbox_creds;
use crate::ipc_framed::EmptyPayload;
use crate::ipc_framed::FramedMessage;
use crate::ipc_framed::IPC_PROTOCOL_VERSION;
use crate::ipc_framed::Message;
use crate::ipc_framed::SpawnRequest;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use crate::runner_client::RunnerTransport;
use crate::runner_client::retry_runner_spawn_once;
use crate::spawn_prep::prepare_elevated_spawn_context_for_permissions;
use anyhow::Result;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_pty::ProcessDriver;
use codex_utils_pty::SpawnedProcess;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

struct RunnerTransportRequest {
    permissions: ResolvedWindowsSandboxPermissions,
    codex_home: PathBuf,
    cwd: PathBuf,
    env_map: HashMap<String, String>,
    logs_base_dir: Option<PathBuf>,
    spawn_request: SpawnRequest,
    read_roots_override: Option<Vec<PathBuf>>,
    additional_read_roots: Vec<PathBuf>,
    read_roots_include_platform_defaults: bool,
    write_roots_override: Option<Vec<PathBuf>>,
    deny_read_paths_override: Vec<PathBuf>,
    deny_write_paths_override: Vec<PathBuf>,
    proxy_enforced: bool,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
}

fn spawn_runner_transport_with_retry<T>(
    sandbox_creds: SandboxCreds,
    request: &RunnerTransportRequest,
    mut spawn: impl FnMut(&Path, &Path, &SandboxCreds, Option<&Path>, SpawnRequest) -> Result<T>,
    refresh: impl FnOnce(
        &ResolvedWindowsSandboxPermissions,
        &Path,
        &HashMap<String, String>,
        &Path,
        Option<&[PathBuf]>,
        &[PathBuf],
        bool,
        Option<&[PathBuf]>,
        &[PathBuf],
        &[PathBuf],
        bool,
        crate::WindowsSandboxProxySettingsMode,
    ) -> Result<SandboxCreds>,
) -> Result<T> {
    retry_runner_spawn_once(
        sandbox_creds,
        &request.spawn_request.command,
        |sandbox_creds| {
            spawn(
                &request.codex_home,
                &request.cwd,
                &sandbox_creds,
                request.logs_base_dir.as_deref(),
                request.spawn_request.clone(),
            )
        },
        || {
            refresh(
                &request.permissions,
                &request.cwd,
                &request.env_map,
                &request.codex_home,
                request.read_roots_override.as_deref(),
                &request.additional_read_roots,
                request.read_roots_include_platform_defaults,
                request.write_roots_override.as_deref(),
                &request.deny_read_paths_override,
                &request.deny_write_paths_override,
                request.proxy_enforced,
                request.proxy_settings_mode,
            )
        },
    )
}

async fn spawn_runner_transport_task(
    sandbox_creds: SandboxCreds,
    request: RunnerTransportRequest,
) -> Result<RunnerTransport> {
    use crate::runner_client::{RunnerAdmission, spawn_runner_transport_with_admission};
    let admission = std::sync::Arc::new(RunnerAdmission::new(|| false));
    struct CancelOnDrop(std::sync::Arc<RunnerAdmission>);
    impl Drop for CancelOnDrop { fn drop(&mut self) { self.0.cancel(); } }
    let _cancel = CancelOnDrop(std::sync::Arc::clone(&admission));
    let (result_tx, result_rx) = oneshot::channel();
    tokio::task::spawn_blocking(move || {
        if result_tx.is_closed() || admission.is_cancelled() { return; }
        let result = spawn_runner_transport_with_retry(
            sandbox_creds,
            &request,
            |home, cwd, creds, logs, request| spawn_runner_transport_with_admission(home, cwd, creds, logs, request, &admission),
            |permissions, cwd, env, home, read, additional, defaults, write, deny_read, deny_write, proxy, mode| {
                anyhow::ensure!(!admission.is_cancelled(), "runner launch cancelled before credential refresh");
                let token = crate::WindowsSandboxCancellationToken::new({ let admission = std::sync::Arc::clone(&admission); move || admission.is_cancelled() });
                refresh_logon_sandbox_creds(permissions, cwd, env, home, read, additional, defaults, write, deny_read, deny_write, proxy, mode, Some(&token))
            },
        );
        // An undelivered transport still owns process rollback, so cancellation
        // after admission cannot strand a runner or a requested process tree.
        let _ = result_tx.send(result);
    });
    result_rx.await.map_err(|err| anyhow::anyhow!("runner handshake worker stopped: {err}"))?
}

#[allow(clippy::too_many_arguments)]
fn prepare_runner_transport_request(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    codex_home: &Path,
    command: Vec<String>,
    cwd: &Path,
    mut env_map: HashMap<String, String>,
    proxy_enforced: bool,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
    timeout_ms: Option<u64>,
    read_roots_override: Option<&[PathBuf]>,
    additional_read_roots: &[AbsolutePathBuf],
    read_roots_include_platform_defaults: bool,
    write_roots_override: Option<&[PathBuf]>,
    deny_read_paths_override: &[AbsolutePathBuf],
    deny_write_paths_override: &[AbsolutePathBuf],
    tty: bool,
    stdin_open: bool,
    use_private_desktop: bool,
    cancellation: Option<&crate::WindowsSandboxCancellationToken>,
) -> Result<(SandboxCreds, RunnerTransportRequest)> {
    let deny_read_paths_override = deny_read_paths_override
        .iter()
        .map(AbsolutePathBuf::to_path_buf)
        .collect::<Vec<_>>();
    let deny_write_paths_override = deny_write_paths_override
        .iter()
        .map(AbsolutePathBuf::to_path_buf)
        .collect::<Vec<_>>();
    let additional_read_roots = additional_read_roots
        .iter()
        .map(AbsolutePathBuf::to_path_buf)
        .collect::<Vec<_>>();
    let permissions =
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            permission_profile,
            workspace_roots,
        )?;
    let elevated = prepare_elevated_spawn_context_for_permissions(
        permissions.clone(),
        codex_home,
        cwd,
        &mut env_map,
        &command,
        read_roots_override,
        &additional_read_roots,
        read_roots_include_platform_defaults,
        write_roots_override,
        &deny_read_paths_override,
        &deny_write_paths_override,
        proxy_enforced,
        proxy_settings_mode,
        cancellation,
    )?;

    let sandbox_creds = elevated.sandbox_creds;
    let request = RunnerTransportRequest {
        permissions,
        codex_home: codex_home.to_path_buf(),
        cwd: cwd.to_path_buf(),
        env_map: env_map.clone(),
        logs_base_dir: elevated.logs_base_dir,
        spawn_request: SpawnRequest {
            command,
            cwd: cwd.to_path_buf(),
            env: env_map,
            permission_profile: permission_profile.clone(),
            workspace_roots: workspace_roots.to_vec(),
            codex_home: elevated.sandbox_base,
            real_codex_home: codex_home.to_path_buf(),
            cap_sids: elevated.cap_sids,
            timeout_ms,
            tty,
            stdin_open,
            use_private_desktop,
        },
        read_roots_override: read_roots_override.map(<[PathBuf]>::to_vec),
        additional_read_roots,
        read_roots_include_platform_defaults,
        write_roots_override: write_roots_override.map(<[PathBuf]>::to_vec),
        deny_read_paths_override,
        deny_write_paths_override,
        proxy_enforced,
        proxy_settings_mode,
    };
    Ok((sandbox_creds, request))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_windows_sandbox_session_elevated_for_permission_profile(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    codex_home: &Path,
    command: Vec<String>,
    cwd: &Path,
    env_map: HashMap<String, String>,
    proxy_enforced: bool,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
    timeout_ms: Option<u64>,
    read_roots_override: Option<&[PathBuf]>,
    additional_read_roots: &[AbsolutePathBuf],
    read_roots_include_platform_defaults: bool,
    write_roots_override: Option<&[PathBuf]>,
    deny_read_paths_override: &[AbsolutePathBuf],
    deny_write_paths_override: &[AbsolutePathBuf],
    tty: bool,
    stdin_open: bool,
    use_private_desktop: bool,
) -> Result<SpawnedProcess> {
    let permission_profile = permission_profile.clone();
    let workspace_roots = workspace_roots.to_vec();
    let codex_home = codex_home.to_path_buf();
    let cwd = cwd.to_path_buf();
    let read_roots_override = read_roots_override.map(<[PathBuf]>::to_vec);
    let additional_read_roots = additional_read_roots.to_vec();
    let write_roots_override = write_roots_override.map(<[PathBuf]>::to_vec);
    let deny_read_paths_override = deny_read_paths_override.to_vec();
    let deny_write_paths_override = deny_write_paths_override.to_vec();
    let (prepared_tx, prepared_rx) = oneshot::channel();
    let prepared_cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct CancelPreparation(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for CancelPreparation { fn drop(&mut self) { self.0.store(true, std::sync::atomic::Ordering::Release); } }
    let _cancel_preparation = CancelPreparation(std::sync::Arc::clone(&prepared_cancelled));
    let preparation_token = crate::WindowsSandboxCancellationToken::new(move || prepared_cancelled.load(std::sync::atomic::Ordering::Acquire));
    tokio::task::spawn_blocking(move || {
        // A dropped request must not start queued persistent setup. Once setup
        // starts this worker finishes its owned security operations; it never
        // launches the requested command before the async receiver accepts it.
        if prepared_tx.is_closed() {
            return;
        }
        let prepared = prepare_runner_transport_request(
            &permission_profile,
            &workspace_roots,
            &codex_home,
            command,
            &cwd,
            env_map,
            proxy_enforced,
            proxy_settings_mode,
            timeout_ms,
            read_roots_override.as_deref(),
            &additional_read_roots,
            read_roots_include_platform_defaults,
            write_roots_override.as_deref(),
            &deny_read_paths_override,
            &deny_write_paths_override,
            tty,
            stdin_open,
            use_private_desktop,
            Some(&preparation_token),
        );
        // The prepared value owns strings/configuration only. Cancellation
        // drops it here without losing a child process or a native token handle.
        let _ = prepared_tx.send(prepared);
    });
    let (sandbox_creds, request) = prepared_rx.await.map_err(|error| {
        anyhow::anyhow!("Windows elevated preparation worker stopped: {error}")
    })??;
    let transport = spawn_runner_transport_task(sandbox_creds, request).await?;
    let (pipe_write, pipe_read) = transport.into_files();

    // One queued caller-owned buffer, plus the current buffer being segmented.
    // The protocol itself admits only 128 KiB of outstanding raw stdin, not an
    // unbounded stream of base64 frames. Shared byte permits follow each input
    // buffer through the runner's native-write acknowledgements.
    let (writer_tx, writer_rx) = codex_utils_pty::stdin_channel(1);
    let (stdin_close_tx, stdin_close_receipt) = tokio::sync::watch::channel(None);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(256);
    let stderr_rx = if tty {
        None
    } else {
        Some(mpsc::channel::<Vec<u8>>(256))
    };
    let (exit_tx, exit_rx) = oneshot::channel::<i32>();

    let outbound_tx = start_runner_pipe_writer(pipe_write);
    let writer_handle = start_runner_stdin_writer(writer_rx, outbound_tx.clone(), tty, stdin_open, stdin_close_tx);
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::sync_channel(1);
    let terminator = {
        let outbound_tx = outbound_tx.clone();
        let mut shutdown = super::runner_shutdown::RunnerShutdown::new(shutdown_rx);
        Some(Box::new(move || {
            shutdown.confirm(|| {
                outbound_tx
                .send(FramedMessage {
                    version: IPC_PROTOCOL_VERSION,
                    message: Message::Terminate {
                        payload: EmptyPayload::default(),
                    },
                }).map_err(|error| std::io::Error::new(std::io::ErrorKind::BrokenPipe,
                    format!("runner cancellation request failed: {error}")))
            })
        })
            as Box<dyn FnMut() -> std::io::Result<()> + Send + Sync>)
    };

    start_runner_stdout_reader(
        pipe_read,
        stdout_tx,
        stderr_rx.as_ref().map(|(tx, _rx)| tx.clone()),
        exit_tx,
        Some(outbound_tx.clone()),
        shutdown_tx,
    );

    Ok(finish_driver_spawn(
        ProcessDriver {
            writer_tx,
            stdout_rx: stdout_rx.into(),
            stderr_rx: stderr_rx.map(|(_tx, rx)| rx.into()),
            exit_rx,
            terminator,
            writer_handle: Some(writer_handle),
            stdin_close_receipt: Some(stdin_close_receipt),
            resizer: if tty {
                Some(make_runner_resizer(outbound_tx))
            } else {
                None
            },
        },
        stdin_open,
    ))
}

