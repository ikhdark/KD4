use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

pub struct ElevatedSandboxProfileCaptureRequest<'a> {
    pub permission_profile: &'a PermissionProfile,
    pub workspace_roots: &'a [AbsolutePathBuf],
    pub codex_home: &'a Path,
    pub command: Vec<String>,
    pub cwd: &'a Path,
    pub env_map: HashMap<String, String>,
    pub timeout_ms: Option<u64>,
    pub cancellation: Option<crate::WindowsSandboxCancellationToken>,
    pub use_private_desktop: bool,
    pub proxy_enforced: bool,
    pub read_roots_override: Option<&'a [PathBuf]>,
    pub additional_read_roots: &'a [AbsolutePathBuf],
    pub read_roots_include_platform_defaults: bool,
    pub write_roots_override: Option<&'a [PathBuf]>,
    pub deny_read_paths_override: &'a [AbsolutePathBuf],
    pub deny_write_paths_override: &'a [AbsolutePathBuf],
    pub output_sink: Option<crate::CaptureOutputSink>,
    pub retained_bytes_cap: Option<usize>,
}

mod windows_impl {
    use super::ElevatedSandboxProfileCaptureRequest;
    use crate::identity::refresh_logon_sandbox_creds;
    use crate::ipc_framed::EmptyPayload;
    use crate::ipc_framed::FramedMessage;
    use crate::ipc_framed::IPC_PROTOCOL_VERSION;
    use crate::ipc_framed::Message;
    use crate::ipc_framed::OutputStream;
    use crate::ipc_framed::SpawnRequest;
    use crate::ipc_framed::decode_bytes;
    use crate::ipc_framed::read_frame;
    use crate::logging::log_failure;
    use crate::logging::log_success;
    use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
    use crate::runner_client::retry_runner_spawn_once;
    use crate::spawn_prep::ElevatedSpawnContext;
    use crate::spawn_prep::prepare_elevated_spawn_context_for_permissions;
    use anyhow::Result;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use std::fs::File;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    pub use crate::windows_impl::CaptureResult;

    /// Polls for cancellation and sends the runner's terminate IPC frame when requested.
    ///
    /// Polling uses a 50 ms park; control writes have a finite deadline and
    /// terminate the owned runner if its control pipe stops draining.
    fn spawn_cancel_writer(
        pipe_write: &Arc<Mutex<File>>,
        cancellation: Option<crate::WindowsSandboxCancellationToken>,
        runner: Option<Arc<crate::runner_client::RunnerProcess>>,
    ) -> Result<Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)>> {
        let Some(cancellation) = cancellation else {
            return Ok(None);
        };
        let pipe_write = Arc::clone(pipe_write);
        let done = Arc::new(AtomicBool::new(false));
        let done_for_thread = Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            while !done_for_thread.load(Ordering::SeqCst) {
                if cancellation.is_cancelled() {
                    if let Ok(pipe_write) = pipe_write.lock() {
                        let frame = FramedMessage {
                                version: IPC_PROTOCOL_VERSION,
                                message: Message::Terminate {
                                    payload: EmptyPayload::default(),
                                },
                            };
                        if crate::runner_client::write_control_frame(&pipe_write, &frame).is_err()
                            && let Some(runner) = runner.as_ref() { runner.terminate(); }
                    }
                    break;
                }
                std::thread::park_timeout(Duration::from_millis(50));
            }
        });
        Ok(Some((handle, done)))
    }

    /// Launches the command runner under the sandbox user and captures its output.
    #[allow(clippy::too_many_arguments)]
    pub fn run_windows_sandbox_capture_for_permission_profile(
        request: ElevatedSandboxProfileCaptureRequest<'_>,
    ) -> Result<CaptureResult> {
        let ElevatedSandboxProfileCaptureRequest {
            permission_profile,
            workspace_roots,
            codex_home,
            command,
            cwd,
            mut env_map,
            timeout_ms,
            cancellation,
            use_private_desktop,
            proxy_enforced,
            read_roots_override,
            additional_read_roots,
            read_roots_include_platform_defaults,
            write_roots_override,
            deny_read_paths_override,
            deny_write_paths_override,
            output_sink,
            retained_bytes_cap,
        } = request;
        crate::setup::check_setup_cancellation(cancellation.as_ref())?;
        let permissions =
            ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                permission_profile,
                workspace_roots,
            )?;
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
        // Captures and sessions must derive the same setup request and capability SIDs.
        let ElevatedSpawnContext {
            sandbox_base,
            logs_base_dir,
            sandbox_creds,
            cap_sids,
        } = prepare_elevated_spawn_context_for_permissions(
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
            crate::WindowsSandboxProxySettingsMode::Reconcile,
            cancellation.as_ref(),
        )?;
        let logs_base_dir = logs_base_dir.as_deref();

        (|| -> Result<CaptureResult> {
            let spawn_request = SpawnRequest {
                command: command.clone(),
                cwd: cwd.to_path_buf(),
                env: env_map.clone(),
                permission_profile: permission_profile.clone(),
                workspace_roots: workspace_roots.to_vec(),
                codex_home: sandbox_base.clone(),
                real_codex_home: codex_home.to_path_buf(),
                cap_sids,
                timeout_ms,
                tty: false,
                stdin_open: false,
                use_private_desktop,
            };
            let admission = crate::runner_client::RunnerAdmission::new({
                let cancellation = cancellation.clone();
                move || cancellation.as_ref().is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled)
            });
            let transport = retry_runner_spawn_once(
                sandbox_creds,
                &spawn_request.command,
                |sandbox_creds| {
                    crate::runner_client::spawn_runner_transport_with_admission(
                        codex_home,
                        cwd,
                        &sandbox_creds,
                        logs_base_dir,
                        spawn_request.clone(),
                        &admission,
                    )
                },
                || {
                    refresh_logon_sandbox_creds(
                        &permissions,
                        cwd,
                        &env_map,
                        codex_home,
                        read_roots_override,
                        &additional_read_roots,
                        read_roots_include_platform_defaults,
                        write_roots_override,
                        &deny_read_paths_override,
                        &deny_write_paths_override,
                        proxy_enforced,
                        crate::WindowsSandboxProxySettingsMode::Reconcile,
                        cancellation.as_ref(),
                    )
                },
            )?;
            let (pipe_write, mut pipe_read, runner) = transport.into_capture_files();
            crate::runner_client::set_nonblocking_pipe(&pipe_write)?;
            let runner = Arc::new(runner);
            let pipe_write = Arc::new(Mutex::new(pipe_write));
            let cancel_writer = spawn_cancel_writer(&pipe_write, cancellation.clone(), Some(Arc::clone(&runner)))?;

            let mut stdout = crate::RetainedCapture::new(retained_bytes_cap);
            let mut stderr = crate::RetainedCapture::new(retained_bytes_cap);
            let result = loop {
                let msg = match read_frame(&mut pipe_read) {
                    Ok(Some(msg)) => msg,
                    Ok(None) => break Err(anyhow::anyhow!("runner pipe closed before exit")),
                    Err(err) => break Err(err),
                };
                match msg.message {
                    Message::SpawnReady { .. } => {}
                    Message::Output { payload } => match decode_bytes(&payload.data_b64) {
                        Ok(bytes) => {
                            if bytes.len() > crate::ipc_framed::IO_CHUNK_BYTES {
                                break Err(anyhow::anyhow!("runner exceeded output chunk size"));
                            }
                            match payload.stream {
                                OutputStream::Stdout => {
                                    stdout.append(&bytes);
                                    if let Some(output_sink) = output_sink.as_ref() {
                                        output_sink(crate::CaptureOutputStream::Stdout, &bytes);
                                    }
                                }
                                OutputStream::Stderr => {
                                    stderr.append(&bytes);
                                    if let Some(output_sink) = output_sink.as_ref() {
                                        output_sink(crate::CaptureOutputStream::Stderr, &bytes);
                                    }
                                }
                            }
                            let ack = FramedMessage {
                                version: IPC_PROTOCOL_VERSION,
                                message: Message::OutputAck {
                                    payload: crate::ipc_framed::OutputCredit {
                                        stream: payload.stream,
                                        chunks: 1,
                                    },
                                },
                            };
                            // After a cancellation Terminate the runner stops reading control
                            // frames and closes that pipe while output already in flight is
                            // still delivered; it needs no further credits, and its Exit frame
                            // (or pipe EOF) still ends this loop.
                            if !cancellation.as_ref().is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled)
                                && let Ok(pipe) = pipe_write.lock()
                                && let Err(err) = crate::runner_client::write_control_frame(&pipe, &ack)
                                && !err.downcast_ref::<std::io::Error>().is_some_and(|err| err.kind() == std::io::ErrorKind::BrokenPipe)
                            {
                                runner.terminate();
                                break Err(err);
                            }
                        }
                        Err(err) => {
                            break Err(err);
                        }
                    },
                    Message::Exit { payload } => break Ok((payload.exit_code, payload.timed_out)),
                    Message::Error { payload } => {
                        break Err(anyhow::anyhow!("runner error: {}", payload.message));
                    }
                    other => {
                        break Err(anyhow::anyhow!(
                            "unexpected runner message during capture: {other:?}"
                        ));
                    }
                }
            };
            if let Some((cancel_handle, done)) = cancel_writer {
                done.store(true, Ordering::SeqCst);
                cancel_handle.thread().unpark();
                let _ = cancel_handle.join();
            }
            drop(pipe_write);
            let (exit_code, timed_out) = result?;

            if exit_code == 0 {
                log_success(&command, logs_base_dir);
            } else {
                log_failure(&command, &format!("exit code {exit_code}"), logs_base_dir);
            }

            let (stdout, stdout_truncated) = stdout.into_parts();
            let (stderr, stderr_truncated) = stderr.into_parts();
            Ok(CaptureResult {
                exit_code,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
                timed_out,
            })
        })()
    }

}

pub use windows_impl::run_windows_sandbox_capture_for_permission_profile;
