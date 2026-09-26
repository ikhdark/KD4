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
    use crate::ipc_framed::write_frame;
    use crate::logging::log_failure;
    use crate::logging::log_success;
    use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
    use crate::runner_client::retry_runner_spawn_once;
    use crate::runner_client::spawn_runner_transport;
    use crate::spawn_prep::ElevatedSpawnContext;
    use crate::spawn_prep::prepare_elevated_spawn_context_for_permissions;
    use anyhow::Result;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use std::fs::File;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    pub use crate::windows_impl::CaptureResult;

    /// Polls for cancellation and sends the runner's terminate IPC frame when requested.
    ///
    /// The 50 ms park bounds cancellation latency without busy-waiting.
    fn spawn_cancel_writer(
        pipe_write: &File,
        cancellation: Option<crate::WindowsSandboxCancellationToken>,
    ) -> Result<Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)>> {
        let Some(cancellation) = cancellation else {
            return Ok(None);
        };
        let mut pipe_write = pipe_write.try_clone()?;
        let done = Arc::new(AtomicBool::new(false));
        let done_for_thread = Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            while !done_for_thread.load(Ordering::SeqCst) {
                if cancellation.is_cancelled() {
                    let _ = write_frame(
                        &mut pipe_write,
                        &FramedMessage {
                            version: IPC_PROTOCOL_VERSION,
                            message: Message::Terminate {
                                payload: EmptyPayload::default(),
                            },
                        },
                    );
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
            let transport = retry_runner_spawn_once(
                sandbox_creds,
                &spawn_request.command,
                |sandbox_creds| {
                    spawn_runner_transport(
                        codex_home,
                        cwd,
                        &sandbox_creds,
                        logs_base_dir,
                        spawn_request.clone(),
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
                    )
                },
            )?;
            let (pipe_write, mut pipe_read) = transport.into_files();
            let cancel_writer = spawn_cancel_writer(&pipe_write, cancellation)?;

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
                        Ok(bytes) => match payload.stream {
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
                        },
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

    #[cfg(test)]
    mod tests {
        use super::spawn_cancel_writer;
        use crate::ipc_framed::Message;
        use crate::ipc_framed::read_frame;
        use std::io::Seek;

        #[test]
        fn cancellation_sends_a_terminate_frame_the_runner_accepts() -> anyhow::Result<()> {
            let mut pipe = tempfile::tempfile()?;
            let cancellation = crate::WindowsSandboxCancellationToken::new(|| true);
            let (writer, _done) =
                spawn_cancel_writer(&pipe, Some(cancellation))?.expect("cancel writer");
            writer.join().expect("join cancel writer");

            pipe.rewind()?;
            let frame = read_frame(&mut pipe)?.expect("runner must receive a terminate frame");
            assert!(matches!(frame.message, Message::Terminate { .. }));
            Ok(())
        }
    }
}

pub use windows_impl::run_windows_sandbox_capture_for_permission_profile;
