use super::windows_common::finish_driver_spawn;
use crate::conpty::ConptyInstance;
use crate::conpty::spawn_conpty_process_as_user;
use crate::desktop::LaunchDesktop;
use crate::logging::log_failure;
use crate::logging::log_note;
use crate::logging::log_success;
use crate::process::ConsoleMode;
use crate::process::StderrMode;
use crate::process::StdinMode;
use crate::process::read_handle_loop;
use crate::process::spawn_process_with_pipes;
use crate::spawn_prep::LegacyAclSids;
use crate::spawn_prep::SpawnPrepOptions;
use crate::spawn_prep::allow_null_device_for_workspace_write;
use crate::spawn_prep::apply_legacy_session_acl_rules;
use crate::spawn_prep::legacy_session_capability_roots;
use crate::spawn_prep::prepare_legacy_session_security;
use crate::spawn_prep::prepare_legacy_spawn_context;
use anyhow::Result;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_pty::JobObject;
use codex_utils_pty::ProcessDriver;
use codex_utils_pty::SpawnedProcess;
use codex_utils_pty::TerminalSize;
use codex_utils_pty::WindowsTtyInputNormalizer;
use std::collections::HashMap;
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Console::COORD;
use windows_sys::Win32::System::Console::ResizePseudoConsole;
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::TerminateProcess;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const WAIT_OBJECT_0: u32 = 0x0000_0000;
const WAIT_TIMEOUT: u32 = 0x0000_0102;
const WAIT_FAILED: u32 = u32::MAX;
const TERMINATION_WAIT_MS: u32 = 5_000;

type SendableHandle = usize;

fn sendable_handle(handle: HANDLE) -> SendableHandle {
    handle as usize
}

fn raw_handle(handle: SendableHandle) -> HANDLE {
    handle as HANDLE
}

struct LegacyProcessHandles {
    process: PROCESS_INFORMATION,
    job: Arc<JobObject>,
    output_join: std::thread::JoinHandle<()>,
    writer_handle: tokio::task::JoinHandle<()>,
    hpc: Option<isize>,
    conpty_owner: Option<ConptyInstance>,
    token_handle: HANDLE,
    desktop: Option<LaunchDesktop>,
}

#[allow(clippy::too_many_arguments)]
fn spawn_legacy_process(
    h_token: HANDLE,
    command: &[String],
    cwd: &Path,
    env_map: &HashMap<String, String>,
    use_private_desktop: bool,
    tty: bool,
    stdin_open: bool,
    stdout_tx: broadcast::Sender<Vec<u8>>,
    stderr_tx: Option<broadcast::Sender<Vec<u8>>>,
    writer_rx: mpsc::Receiver<Vec<u8>>,
    logs_base_dir: Option<&Path>,
) -> Result<LegacyProcessHandles> {
    let (pi, job, output_join, writer_handle, hpc, conpty_owner, desktop) = if tty {
        // SAFETY: the caller owns `h_token` for the lifetime of this synchronous spawn.
        let (pi, mut conpty) = unsafe {
            spawn_conpty_process_as_user(
                h_token,
                command,
                cwd,
                env_map,
                use_private_desktop,
                logs_base_dir,
            )
        }?;
        let job = conpty
            .job()
            .ok_or_else(|| anyhow::anyhow!("spawned ConPTY is missing its process job"))?;
        let hpc = conpty.raw_handle();
        let output_join = spawn_output_reader(conpty.take_output_read(), stdout_tx);
        let writer_handle = spawn_input_writer(
            Some(conpty.take_input_write()),
            writer_rx,
            /*normalize_newlines*/ true,
        );
        (pi, job, output_join, writer_handle, hpc, Some(conpty), None)
    } else {
        // SAFETY: the caller owns `h_token` for the lifetime of this synchronous spawn.
        let pipe_handles = unsafe {
            spawn_process_with_pipes(
                h_token,
                command,
                cwd,
                env_map,
                if stdin_open {
                    StdinMode::Open
                } else {
                    StdinMode::Closed
                },
                StderrMode::Separate,
                ConsoleMode::Inherit,
                use_private_desktop,
                logs_base_dir,
            )
        }?;
        let stdout_join = spawn_output_reader(pipe_handles.stdout_read, stdout_tx);
        let Some(stderr_read) = pipe_handles.stderr_read else {
            anyhow::bail!("separate stderr handle should be present");
        };
        let Some(stderr_tx) = stderr_tx else {
            anyhow::bail!("separate stderr channel should be present");
        };
        let stderr_join = spawn_output_reader(stderr_read, stderr_tx);
        let output_join = std::thread::spawn(move || {
            let _ = stdout_join.join();
            let _ = stderr_join.join();
        });
        let writer_handle = spawn_input_writer(
            pipe_handles.stdin_write,
            writer_rx,
            /*normalize_newlines*/ false,
        );
        (
            pipe_handles.process,
            pipe_handles.job(),
            output_join,
            writer_handle,
            None,
            None,
            Some(pipe_handles.desktop),
        )
    };
    Ok(LegacyProcessHandles {
        process: pi,
        job,
        output_join,
        writer_handle,
        hpc,
        conpty_owner,
        token_handle: h_token,
        desktop,
    })
}

fn spawn_output_reader(
    output_read: HANDLE,
    output_tx: broadcast::Sender<Vec<u8>>,
) -> std::thread::JoinHandle<()> {
    read_handle_loop(output_read, move |chunk| {
        let _ = output_tx.send(chunk.to_vec());
    })
}

fn spawn_input_writer(
    input_write: Option<HANDLE>,
    mut writer_rx: mpsc::Receiver<Vec<u8>>,
    normalize_newlines: bool,
) -> tokio::task::JoinHandle<()> {
    let input_write = input_write.map(|handle| handle as usize);
    tokio::task::spawn_blocking(move || {
        let mut windows_input = WindowsTtyInputNormalizer::default();
        while let Some(bytes) = writer_rx.blocking_recv() {
            let Some(handle_addr) = input_write else {
                continue;
            };
            let handle = handle_addr as HANDLE;
            let bytes = if normalize_newlines {
                windows_input.normalize(&bytes)
            } else {
                bytes
            };
            if write_all_handle(handle, &bytes).is_err() {
                break;
            }
        }
        if let Some(handle) = input_write {
            unsafe {
                CloseHandle(handle as HANDLE);
            }
        }
    })
}

fn terminate_job_or_process(
    job: &JobObject,
    process_handle: &Arc<StdMutex<Option<SendableHandle>>>,
    logs_base_dir: Option<&Path>,
) {
    if let Err(job_err) = job.terminate() {
        log_note(
            &format!("legacy spawn failed to terminate process tree: {job_err}"),
            logs_base_dir,
        );
        if let Ok(guard) = process_handle.lock()
            && let Some(handle) = guard.as_ref()
            && unsafe { TerminateProcess(raw_handle(*handle), 1) } == 0
        {
            log_note(
                &format!(
                    "legacy spawn failed to terminate root process: {}",
                    unsafe { GetLastError() }
                ),
                logs_base_dir,
            );
        }
    }
}

fn write_all_handle(handle: HANDLE, mut bytes: &[u8]) -> Result<()> {
    while !bytes.is_empty() {
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr() as *const _,
                bytes.len() as u32,
                &mut written,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() } as i32;
            return Err(anyhow::anyhow!("WriteFile failed: {err}"));
        }
        if written == 0 {
            anyhow::bail!("WriteFile returned success but wrote 0 bytes");
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finalize_exit(
    exit_tx: oneshot::Sender<i32>,
    process_handle: Arc<StdMutex<Option<SendableHandle>>>,
    thread_handle: SendableHandle,
    output_join: std::thread::JoinHandle<()>,
    logs_base_dir: Option<&Path>,
    command: Vec<String>,
    termination_requested: bool,
) {
    let wait_timeout = if termination_requested {
        TERMINATION_WAIT_MS
    } else {
        INFINITE
    };
    let (exit_code, root_exited) = match process_handle.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(handle) => {
                let handle = raw_handle(*handle);
                let wait_result = unsafe { WaitForSingleObject(handle, wait_timeout) };
                match wait_result {
                    WAIT_OBJECT_0 => {
                        let mut raw_exit = 1u32;
                        if unsafe { GetExitCodeProcess(handle, &mut raw_exit) } == 0 {
                            log_note(
                                &format!(
                                    "legacy spawn failed to read root process exit code: {}",
                                    unsafe { GetLastError() }
                                ),
                                logs_base_dir,
                            );
                            (1, false)
                        } else {
                            (raw_exit as i32, true)
                        }
                    }
                    WAIT_TIMEOUT => {
                        log_note(
                            "legacy root process did not exit after termination",
                            logs_base_dir,
                        );
                        (1, false)
                    }
                    WAIT_FAILED => {
                        log_note(
                            &format!(
                                "legacy spawn failed final root process wait: Windows error {}",
                                unsafe { GetLastError() }
                            ),
                            logs_base_dir,
                        );
                        (1, false)
                    }
                    unexpected => {
                        log_note(
                            &format!(
                                "legacy spawn received unexpected final root wait result \
                                 0x{unexpected:08x}"
                            ),
                            logs_base_dir,
                        );
                        (1, false)
                    }
                }
            }
            None => {
                log_note("legacy spawn lost its root process handle", logs_base_dir);
                (1, false)
            }
        },
        Err(_) => {
            log_note(
                "legacy spawn root process handle lock poisoned",
                logs_base_dir,
            );
            (1, false)
        }
    };

    if root_exited {
        let _ = output_join.join();
    }
    let _ = exit_tx.send(exit_code);

    unsafe {
        let thread_handle = raw_handle(thread_handle);
        if !thread_handle.is_null() && thread_handle != INVALID_HANDLE_VALUE {
            CloseHandle(thread_handle);
        }
        if let Ok(mut guard) = process_handle.lock()
            && let Some(handle) = guard.take()
        {
            CloseHandle(raw_handle(handle));
        }
    }

    if exit_code == 0 {
        log_success(&command, logs_base_dir);
    } else {
        log_failure(&command, &format!("exit code {exit_code}"), logs_base_dir);
    }
}

fn resize_conpty_handle(hpc: &Arc<StdMutex<Option<isize>>>, size: TerminalSize) -> Result<()> {
    let guard = hpc
        .lock()
        .map_err(|_| anyhow::anyhow!("failed to lock ConPTY handle"))?;
    let hpc = guard
        .as_ref()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("process is not attached to a PTY"))?;
    let result = unsafe {
        ResizePseudoConsole(
            hpc,
            COORD {
                X: size.cols as i16,
                Y: size.rows as i16,
            },
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "failed to resize console: HRESULT {result}"
        ))
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_windows_sandbox_session_legacy(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    codex_home: &Path,
    command: Vec<String>,
    cwd: &Path,
    mut env_map: HashMap<String, String>,
    timeout_ms: Option<u64>,
    additional_deny_read_paths: &[AbsolutePathBuf],
    additional_deny_write_paths: &[AbsolutePathBuf],
    tty: bool,
    stdin_open: bool,
    use_private_desktop: bool,
) -> Result<SpawnedProcess> {
    crate::ensure_legacy_delete_child_safety(
        crate::legacy_restricted_token_enforces_delete_child(),
    )?;
    let common = prepare_legacy_spawn_context(
        permission_profile,
        workspace_roots,
        codex_home,
        cwd,
        &mut env_map,
        &command,
        SpawnPrepOptions {
            inherit_path: false,
            add_git_safe_directory: false,
        },
    )?;
    if !common.permissions.has_full_disk_read_access() {
        anyhow::bail!("Restricted read-only access requires the elevated Windows sandbox backend");
    }
    // WRITE_RESTRICTED tokens consult restricting SIDs only for writes, so this
    // backend cannot make capability-SID deny-read ACLs authoritative.
    if !additional_deny_read_paths.is_empty() {
        anyhow::bail!("deny-read overrides require the elevated Windows sandbox backend");
    }
    let additional_deny_write_paths = additional_deny_write_paths
        .iter()
        .map(AbsolutePathBuf::to_path_buf)
        .collect::<Vec<_>>();
    let capability_roots = legacy_session_capability_roots(
        &common.permissions,
        &common.current_dir,
        &env_map,
        codex_home,
    );
    let security = prepare_legacy_session_security(
        common.uses_write_capabilities,
        codex_home,
        cwd,
        capability_roots,
    )?;
    allow_null_device_for_workspace_write(common.uses_write_capabilities);

    apply_legacy_session_acl_rules(
        &common.permissions,
        codex_home,
        &common.current_dir,
        &env_map,
        &[],
        &additional_deny_write_paths,
        LegacyAclSids {
            readonly_sid: security.readonly_sid.as_ref(),
            readonly_sid_str: security.readonly_sid_str.as_deref(),
            write_root_sids: &security.write_root_sids,
        },
    )?;

    let (writer_tx, writer_rx) = mpsc::channel::<Vec<u8>>(128);
    let (stdout_tx, stdout_rx) = broadcast::channel::<Vec<u8>>(256);
    let stderr_rx = if tty {
        None
    } else {
        Some(broadcast::channel::<Vec<u8>>(256))
    };
    let (exit_tx, exit_rx) = oneshot::channel::<i32>();

    let LegacyProcessHandles {
        process: pi,
        job,
        output_join,
        writer_handle,
        hpc,
        mut conpty_owner,
        token_handle,
        desktop,
    } = match spawn_legacy_process(
        security.h_token,
        &command,
        cwd,
        &env_map,
        use_private_desktop,
        tty,
        stdin_open,
        stdout_tx,
        stderr_rx.as_ref().map(|(tx, _rx)| tx.clone()),
        writer_rx,
        common.logs_base_dir.as_deref(),
    ) {
        Ok(handles) => handles,
        Err(err) => {
            unsafe {
                CloseHandle(security.h_token);
            }
            return Err(err);
        }
    };
    let hpc_handle = hpc.map(|hpc| Arc::new(StdMutex::new(Some(hpc))));

    let process_handle = Arc::new(StdMutex::new(Some(sendable_handle(pi.hProcess))));
    let wait_handle = Arc::clone(&process_handle);
    let job_for_wait = Arc::clone(&job);
    let command_for_wait = command.clone();
    let hpc_for_wait = hpc_handle.clone();
    let wait_logs_base_dir = common.logs_base_dir.clone();
    let process_handle_addr = sendable_handle(pi.hProcess);
    let thread_handle_addr = sendable_handle(pi.hThread);
    let token_handle_addr = sendable_handle(token_handle);
    std::thread::spawn(move || {
        let process_handle = raw_handle(process_handle_addr);
        let _desktop = desktop;
        let timeout = crate::windows_wait_timeout(timeout_ms);
        let wait_res = unsafe { WaitForSingleObject(process_handle, timeout) };
        let termination_requested = match wait_res {
            WAIT_OBJECT_0 => {
                if let Err(err) = job_for_wait.preserve_descendants() {
                    log_note(
                        &format!(
                            "legacy spawn failed to preserve descendants after root exit: {err}"
                        ),
                        wait_logs_base_dir.as_deref(),
                    );
                }
                false
            }
            WAIT_TIMEOUT => {
                terminate_job_or_process(
                    &job_for_wait,
                    &wait_handle,
                    wait_logs_base_dir.as_deref(),
                );
                true
            }
            WAIT_FAILED => {
                let wait_error = unsafe { GetLastError() };
                log_note(
                    &format!(
                        "legacy spawn failed to wait for root process; terminating process tree: \
                         Windows error {wait_error}"
                    ),
                    wait_logs_base_dir.as_deref(),
                );
                terminate_job_or_process(
                    &job_for_wait,
                    &wait_handle,
                    wait_logs_base_dir.as_deref(),
                );
                true
            }
            unexpected => {
                log_note(
                    &format!(
                        "legacy spawn received unexpected root wait result \
                         0x{unexpected:08x}; terminating process tree"
                    ),
                    wait_logs_base_dir.as_deref(),
                );
                terminate_job_or_process(
                    &job_for_wait,
                    &wait_handle,
                    wait_logs_base_dir.as_deref(),
                );
                true
            }
        };
        if let Some(hpc) = hpc_for_wait
            && let Ok(mut guard) = hpc.lock()
        {
            let _ = guard.take();
        }
        drop(conpty_owner.take());
        unsafe {
            let token_handle = raw_handle(token_handle_addr);
            if !token_handle.is_null() && token_handle != INVALID_HANDLE_VALUE {
                CloseHandle(token_handle);
            }
        }
        finalize_exit(
            exit_tx,
            wait_handle,
            thread_handle_addr,
            output_join,
            wait_logs_base_dir.as_deref(),
            command_for_wait,
            termination_requested,
        );
    });

    let terminator = {
        let job = Arc::clone(&job);
        let process_handle = Arc::clone(&process_handle);
        let logs_base_dir = common.logs_base_dir;
        Some(Box::new(move || {
            terminate_job_or_process(&job, &process_handle, logs_base_dir.as_deref());
        }) as Box<dyn FnMut() + Send + Sync>)
    };

    let driver = ProcessDriver {
        writer_tx,
        stdout_rx,
        stderr_rx: stderr_rx.map(|(_tx, rx)| rx),
        exit_rx,
        terminator,
        writer_handle: Some(writer_handle),
        resizer: hpc_handle.map(|hpc| {
            Box::new(move |size| resize_conpty_handle(&hpc, size))
                as Box<dyn FnMut(TerminalSize) -> Result<()> + Send>
        }),
    };

    Ok(finish_driver_spawn(driver, stdin_open))
}

#[cfg(test)]
#[path = "legacy_tests.rs"]
mod tests;
