//! Windows command runner used by the **elevated** sandbox path.
//!
//! The CLI launches this binary under the sandbox user when Windows sandbox level is
//! Elevated. It connects to the IPC pipes, reads the framed `SpawnRequest`, derives a
//! restricted token from the sandbox user, and spawns the child process via ConPTY
//! (`tty=true`) or pipes (`tty=false`). It then streams output frames back to the parent,
//! accepts stdin/terminate frames, and emits a final exit frame. The legacy restricted‑token
//! path spawns the child directly and does not use this runner.

#![allow(unsafe_op_in_unsafe_fn)]

mod cwd_junction;

use anyhow::Context;
use anyhow::Result;
use codex_utils_pty::JobObject;
use codex_windows_sandbox::ConsoleMode;
use codex_windows_sandbox::EmptyPayload;
use codex_windows_sandbox::ErrorPayload;
use codex_windows_sandbox::ErrorStage;
use codex_windows_sandbox::ExitPayload;
use codex_windows_sandbox::FramedMessage;
use codex_windows_sandbox::IO_CHUNK_BYTES;
use codex_windows_sandbox::IO_WINDOW_CHUNKS;
use codex_windows_sandbox::IPC_PROTOCOL_VERSION;
use codex_windows_sandbox::LocalSid;
use codex_windows_sandbox::Message;
use codex_windows_sandbox::OUTPUT_DRAIN_AFTER_ROOT_EXIT;
use codex_windows_sandbox::OutputPayload;
use codex_windows_sandbox::OutputStream;
use codex_windows_sandbox::PipeSpawnHandles;
use codex_windows_sandbox::ResizePayload;
use codex_windows_sandbox::SpawnReady;
use codex_windows_sandbox::SpawnRequest;
use codex_windows_sandbox::StderrMode;
use codex_windows_sandbox::StdinMode;
use codex_windows_sandbox::WindowsSandboxTokenMode;
use codex_windows_sandbox::create_readonly_token_with_caps_and_user_from;
use codex_windows_sandbox::create_workspace_write_token_with_caps_and_user_from;
use codex_windows_sandbox::decode_bytes;
use codex_windows_sandbox::encode_bytes;
use codex_windows_sandbox::get_current_token_for_restriction;
use codex_windows_sandbox::hide_current_user_profile_dir;
use codex_windows_sandbox::log_note;
use codex_windows_sandbox::read_frame;
use codex_windows_sandbox::read_handle_loop;
use codex_windows_sandbox::spawn_process_with_pipes;
use codex_windows_sandbox::to_wide;
use codex_windows_sandbox::token_mode_for_permission_profile;
use codex_windows_sandbox::wait_for_output_readers;
use codex_windows_sandbox::windows_wait_timeout;
use codex_windows_sandbox::write_frame;
use std::ffi::OsStr;
use std::fs::File;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
use windows_sys::Win32::Foundation::DuplicateHandle;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::System::Console::COORD;
use windows_sys::Win32::System::Console::ResizePseudoConsole;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::GetProcessId;
use windows_sys::Win32::System::Threading::MUTEX_ALL_ACCESS;
use windows_sys::Win32::System::Threading::OpenMutexW;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::TerminateProcess;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

// Kept in sync with codex_exec_server::CODEX_FS_HELPER_ARG1 without introducing
// a dependency cycle.
const FS_HELPER_ARG: &str = "--codex-run-as-fs-helper";
const READ_ACL_MUTEX_NAME: &str = "Local\\CodexSandboxReadAcl";
const TERMINATION_WAIT_MS: u32 = 5_000;
const WAIT_OBJECT_0: u32 = 0x0000_0000;
const WAIT_TIMEOUT: u32 = 0x0000_0102;
const WAIT_FAILED: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessWaitOutcome {
    Exited,
    TimedOut,
    Failed {
        wait_result: u32,
        windows_error: Option<u32>,
    },
}

struct IpcSpawnedProcess {
    log_dir: PathBuf,
    pi: PROCESS_INFORMATION,
    job: Arc<JobObject>,
    stdout_handle: HANDLE,
    stderr_handle: HANDLE,
    stdin_handle: Option<HANDLE>,
    conpty_owner: Option<codex_windows_sandbox::ConptyInstance>,
    hpc_handle: Option<isize>,
    control_process_handle: OwnedWinHandle,
    _pipe_handles: Option<PipeSpawnHandles>,
}

/// Small RAII wrapper for raw Win32 handles.
///
/// The elevated runner has a few early-return paths where we acquire a token, job, or pipe
/// handle and then may fail while preparing the child. Keeping those handles in a guard makes
/// the error paths read more directly and closes the gaps that were previously leaking them.
struct OwnedWinHandle(usize);

impl OwnedWinHandle {
    fn new(handle: HANDLE) -> Self {
        Self(handle as usize)
    }

    fn raw(&self) -> HANDLE {
        self.0 as HANDLE
    }

    fn duplicate(handle: HANDLE) -> Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            anyhow::bail!("cannot duplicate an invalid Windows handle");
        }
        // SAFETY: GetCurrentProcess returns a borrowed pseudo-handle and takes no pointers or
        // ownership.
        let current_process = unsafe { GetCurrentProcess() };
        let mut duplicate = std::ptr::null_mut();
        // SAFETY: The caller keeps handle open through duplication; current_process is its process
        // pseudo-handle, and duplicate is a writable output slot. A successful duplicate is
        // immediately owned by this wrapper.
        let duplicated = unsafe {
            DuplicateHandle(
                current_process,
                handle,
                current_process,
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if duplicated == 0 {
            anyhow::bail!("DuplicateHandle failed: {}", unsafe { GetLastError() });
        }
        Ok(Self::new(duplicate))
    }

    fn into_raw(mut self) -> HANDLE {
        // Transfer ownership to the caller. After this point the caller is responsible for
        // eventually closing the returned HANDLE.
        let handle = self.raw();
        self.0 = 0;
        handle
    }
}

impl Drop for OwnedWinHandle {
    fn drop(&mut self) {
        let handle = self.raw();
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            // SAFETY: This wrapper owns the non-null, non-invalid handle; into_raw clears it before
            // ownership transfer, so Drop closes it only once.
            unsafe {
                CloseHandle(handle);
            }
        }
    }
}

/// Open a named pipe created by the parent process.
fn open_pipe(name: &str, access: u32) -> Result<HANDLE> {
    let path = to_wide(name);
    // SAFETY: path is NUL-terminated UTF-16 and lives through CreateFileW. The null
    // security/template pointers request defaults, and failure is checked before returning
    // the owned handle.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            0,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        // SAFETY: GetLastError reads this thread's last-error value immediately after CreateFileW
        // fails.
        let err = unsafe { GetLastError() };
        return Err(anyhow::anyhow!("CreateFileW failed for pipe {name}: {err}"));
    }
    Ok(handle)
}

/// Send an error frame back to the parent process.
fn send_error(
    writer: &Arc<StdMutex<File>>,
    stage: ErrorStage,
    windows_error_code: Option<u32>,
    message: String,
) -> Result<()> {
    let msg = FramedMessage {
        version: IPC_PROTOCOL_VERSION,
        message: Message::Error {
            payload: ErrorPayload {
                message,
                stage,
                windows_error_code,
            },
        },
    };
    if let Ok(mut guard) = writer.lock() {
        write_frame(&mut *guard, &msg)?;
    }
    Ok(())
}

fn windows_error_code(err: &anyhow::Error) -> Option<u32> {
    err.chain().find_map(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .and_then(|code| u32::try_from(code).ok())
    })
}

/// Read and validate the initial spawn request frame.
fn read_spawn_request(reader: &mut File) -> Result<SpawnRequest> {
    let Some(msg) = read_frame(reader)? else {
        anyhow::bail!("runner: pipe closed before spawn_request");
    };
    if msg.version != IPC_PROTOCOL_VERSION {
        anyhow::bail!("runner: unsupported protocol version {}", msg.version);
    }
    match msg.message {
        Message::SpawnRequest { payload } => Ok(*payload),
        other => anyhow::bail!("runner: expected spawn_request, got {other:?}"),
    }
}

fn read_acl_mutex_exists() -> Result<bool> {
    let name = to_wide(OsStr::new(READ_ACL_MUTEX_NAME));
    // SAFETY: name is a live NUL-terminated UTF-16 mutex name; the access flags are scalar and the
    // returned handle is checked before use.
    let handle = unsafe { OpenMutexW(MUTEX_ALL_ACCESS, 0, name.as_ptr()) };
    if handle.is_null() {
        // SAFETY: GetLastError reads this thread's last-error value immediately after OpenMutexW
        // fails.
        let err = unsafe { GetLastError() };
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        return Err(anyhow::anyhow!("OpenMutexW failed: {err}"));
    }
    // SAFETY: OpenMutexW returned a non-null handle owned by this function; it is closed once after
    // the existence probe.
    unsafe {
        CloseHandle(handle);
    }
    Ok(true)
}

/// Pick an effective CWD, using a junction if the ACL helper is active.
fn effective_cwd(req_cwd: &Path, log_dir: Option<&Path>) -> PathBuf {
    let use_junction = match read_acl_mutex_exists() {
        Ok(exists) => exists,
        Err(err) => {
            log_note(
                &format!(
                    "junction: failed to probe ACL mutex state: {err}; defaulting to junction cwd"
                ),
                log_dir,
            );
            true
        }
    };
    if use_junction {
        cwd_junction::create_cwd_junction(req_cwd, log_dir).unwrap_or_else(|| req_cwd.to_path_buf())
    } else {
        req_cwd.to_path_buf()
    }
}

fn spawn_ipc_process(req: &SpawnRequest) -> Result<IpcSpawnedProcess> {
    let log_dir = req.codex_home.clone();
    hide_current_user_profile_dir(req.codex_home.as_path());
    let token_mode = token_mode_for_permission_profile(
        &req.permission_profile,
        &req.workspace_roots,
        &req.cwd,
        &req.env,
    )
    .context("resolve permission profile token mode")?;
    let mut cap_psids: Vec<LocalSid> = Vec::new();
    for sid in &req.cap_sids {
        cap_psids.push(
            LocalSid::from_string(sid)
                .context("ConvertStringSidToSidW failed for capability SID")?,
        );
    }
    if cap_psids.is_empty() {
        anyhow::bail!("runner: empty capability SID list");
    }

    // The token helpers still take raw SID pointers, but we keep ownership in `LocalSid`
    // wrappers for as long as possible. That way any failure after SID parsing but before the
    // child is fully spawned still releases the backing LocalAlloc memory automatically.
    let cap_psid_ptrs: Vec<*mut _> = cap_psids.iter().map(LocalSid::as_ptr).collect();
    // SAFETY: The token helper returns a newly opened token handle, which is immediately put under
    // OwnedWinHandle ownership.
    let base = OwnedWinHandle::new(unsafe { get_current_token_for_restriction()? });
    // SAFETY: base owns the valid source token; cap_psids owns every SID referenced by
    // cap_psid_ptrs throughout the synchronous token creation. The returned token is
    // immediately owned.
    let h_token = OwnedWinHandle::new(unsafe {
        match token_mode {
            WindowsSandboxTokenMode::ReadOnlyCapability => {
                create_readonly_token_with_caps_and_user_from(base.raw(), &cap_psid_ptrs)
            }
            WindowsSandboxTokenMode::WritableRootsCapability => {
                create_workspace_write_token_with_caps_and_user_from(base.raw(), &cap_psid_ptrs)
            }
        }
    }?);

    let effective_cwd = effective_cwd(&req.cwd, Some(log_dir.as_path()));

    let mut conpty_owner = None;
    let mut hpc_handle: Option<isize> = None;
    let mut pipe_handles = None;
    let (pi, job, stdout_handle, stderr_handle, stdin_handle) = if req.tty {
        // SAFETY: `h_token` owns the valid primary token through this synchronous spawn.
        let (pi, mut conpty) = unsafe {
            codex_windows_sandbox::spawn_conpty_process_as_user(
                h_token.raw(),
                &req.command,
                &effective_cwd,
                &req.env,
                req.use_private_desktop,
                Some(log_dir.as_path()),
            )
        }?;
        let job = conpty
            .job()
            .context("spawned ConPTY is missing its process job")?;
        hpc_handle = conpty.raw_handle();
        let input_write = conpty.take_input_write();
        let output_read = conpty.take_output_read();
        conpty_owner = Some(conpty);
        let stdin_handle = if req.stdin_open {
            Some(input_write)
        } else {
            // SAFETY: take_input_write transferred this pipe end out of the ConPTY owner. No stdin
            // consumer is created in this branch, so this is its sole close.
            unsafe {
                CloseHandle(input_write);
            }
            None
        };
        (
            pi,
            job,
            output_read,
            windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
            stdin_handle,
        )
    } else {
        let stdin_mode = if req.stdin_open {
            StdinMode::Open
        } else {
            StdinMode::Closed
        };
        // SAFETY: `h_token` owns the valid primary token through this synchronous spawn.
        let spawned_pipes: PipeSpawnHandles = unsafe {
            spawn_process_with_pipes(
                h_token.raw(),
                &req.command,
                &effective_cwd,
                &req.env,
                stdin_mode,
                StderrMode::Separate,
                if req.command.get(1).is_some_and(|arg| arg == FS_HELPER_ARG) {
                    ConsoleMode::NoWindow
                } else {
                    ConsoleMode::Inherit
                },
                req.use_private_desktop,
                Some(log_dir.as_path()),
            )
        }?;
        let pi = spawned_pipes.process;
        let stdout_handle = spawned_pipes.stdout_read;
        let stderr_handle = spawned_pipes
            .stderr_read
            .unwrap_or(windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE);
        let stdin_handle = spawned_pipes.stdin_write;
        let job = spawned_pipes.job();
        pipe_handles = Some(spawned_pipes);
        (pi, job, stdout_handle, stderr_handle, stdin_handle)
    };
    let control_process_handle = match OwnedWinHandle::duplicate(pi.hProcess) {
        Ok(handle) => handle,
        Err(err) => {
            terminate_job_or_process(&job, pi.hProcess, Some(log_dir.as_path()));
            return Err(err.context("failed to retain child process handle for runner control"));
        }
    };
    Ok(IpcSpawnedProcess {
        log_dir,
        pi,
        job,
        stdout_handle,
        stderr_handle,
        stdin_handle,
        conpty_owner,
        hpc_handle,
        control_process_handle,
        _pipe_handles: pipe_handles,
    })
}

/// Stream stdout/stderr from the child into Output frames.
///
/// Critical sections only adjust bounded counters and the stop flag and cannot
/// panic, so a poisoned lock still guards a consistent window.
#[derive(Default)]
struct OutputWindow {
    state: StdMutex<([usize; 2], bool)>,
    changed: Condvar,
}

impl OutputWindow {
    fn take(&self, stream: OutputStream) -> bool {
        let index = usize::from(stream == OutputStream::Stderr);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        while state.0[index] == IO_WINDOW_CHUNKS && !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if state.1 {
            return false;
        }
        state.0[index] += 1;
        true
    }
    fn acknowledge(&self, stream: OutputStream, chunks: usize) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let index = usize::from(stream == OutputStream::Stderr);
        if chunks == 0 || chunks > state.0[index] {
            return false;
        }
        state.0[index] -= chunks;
        self.changed.notify_all();
        true
    }
    fn stop(&self) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
        self.changed.notify_all();
    }
}

#[derive(Clone)]
struct RunnerFlow {
    writer: Arc<StdMutex<File>>,
    output: Arc<OutputWindow>,
}

impl RunnerFlow {
    fn send(&self, message: Message) -> bool {
        self.writer.lock().ok().is_some_and(|mut writer| {
            write_frame(
                &mut *writer,
                &FramedMessage {
                    version: IPC_PROTOCOL_VERSION,
                    message,
                },
            )
            .is_ok()
        })
    }
}

fn spawn_output_reader(
    writer: Arc<StdMutex<File>>,
    handle: HANDLE,
    stream: OutputStream,
    log_dir: Option<PathBuf>,
    window: Arc<OutputWindow>,
) -> std::thread::JoinHandle<()> {
    read_handle_loop(handle, move |chunk| {
        for chunk in chunk.chunks(IO_CHUNK_BYTES) {
            // Never hold the shared pipe mutex while waiting for output admission:
            // actual stdin write receipts use that pipe even with blocked stdout.
            if !window.take(stream) {
                return;
            }
            let msg = FramedMessage {
                version: IPC_PROTOCOL_VERSION,
                message: Message::Output {
                    payload: OutputPayload {
                        data_b64: encode_bytes(chunk),
                        stream,
                    },
                },
            };
            if let Ok(mut guard) = writer.lock()
                && let Err(err) = write_frame(&mut *guard, &msg)
            {
                window.stop();
                log_note(
                    &format!("runner output write failed: {err}"),
                    log_dir.as_deref(),
                );
            }
        }
    })
}

fn terminate_job_or_process(job: &JobObject, process: HANDLE, log_dir: Option<&Path>) {
    if let Err(job_err) = job.terminate() {
        log_note(
            &format!("runner failed to terminate process tree: {job_err}"),
            log_dir,
        );
        // SAFETY: The caller retains its process handle while termination is requested;
        // TerminateProcess does not transfer or close that handle.
        if unsafe { TerminateProcess(process, 1) } == 0 {
            log_note(
                &format!("runner failed to terminate root process: {}", unsafe {
                    GetLastError()
                }),
                log_dir,
            );
        }
    }
}

fn wait_for_process(process: HANDLE, timeout_ms: u32) -> ProcessWaitOutcome {
    // SAFETY: The caller keeps the process handle open for the duration of this synchronous wait.
    let wait_result = unsafe { WaitForSingleObject(process, timeout_ms) };
    match wait_result {
        WAIT_OBJECT_0 => ProcessWaitOutcome::Exited,
        WAIT_TIMEOUT => ProcessWaitOutcome::TimedOut,
        WAIT_FAILED => ProcessWaitOutcome::Failed {
            wait_result,
            // SAFETY: GetLastError reads this thread's last-error value immediately after
            // WaitForSingleObject reports WAIT_FAILED.
            windows_error: Some(unsafe { GetLastError() }),
        },
        _ => ProcessWaitOutcome::Failed {
            wait_result,
            windows_error: None,
        },
    }
}

fn log_failed_wait(
    context: &str,
    wait_result: u32,
    windows_error: Option<u32>,
    log_dir: Option<&Path>,
) {
    let error_suffix = windows_error
        .map(|error| format!(", Windows error {error}"))
        .unwrap_or_default();
    log_note(
        &format!("{context}: wait result 0x{wait_result:08x}{error_suffix}"),
        log_dir,
    );
}

fn wait_for_terminated_root(process: HANDLE, log_dir: Option<&Path>) -> bool {
    match wait_for_process(process, TERMINATION_WAIT_MS) {
        ProcessWaitOutcome::Exited => true,
        ProcessWaitOutcome::TimedOut => {
            log_note(
                "runner root process did not exit after termination",
                log_dir,
            );
            false
        }
        ProcessWaitOutcome::Failed {
            wait_result,
            windows_error,
        } => {
            log_failed_wait(
                "runner failed to wait for root process after termination",
                wait_result,
                windows_error,
                log_dir,
            );
            false
        }
    }
}

/// Read stdin/terminate frames and forward to the child process.
fn spawn_input_loop(
    reader: File,
    stdin_handle: Option<HANDLE>,
    hpc_handle: Arc<StdMutex<Option<isize>>>,
    job: Arc<JobObject>,
    process: OwnedWinHandle,
    log_dir: Option<PathBuf>,
    flow: Option<RunnerFlow>,
) -> std::thread::JoinHandle<()> {
    let termination_log_dir = log_dir.clone();
    spawn_input_loop_with_terminator(reader, stdin_handle, hpc_handle, log_dir, flow, move || {
        terminate_job_or_process(&job, process.raw(), termination_log_dir.as_deref())
    })
}

fn spawn_input_loop_with_terminator<F>(
    mut reader: File,
    stdin_handle: Option<HANDLE>,
    hpc_handle: Arc<StdMutex<Option<isize>>>,
    log_dir: Option<PathBuf>,
    flow: Option<RunnerFlow>,
    terminate_process_tree: F,
) -> std::thread::JoinHandle<()>
where
    F: Fn() + Send + 'static,
{
    let outstanding = Arc::new(AtomicUsize::new(0));
    let mut stdin_tx = stdin_handle.map(|handle| {
        spawn_stdin_writer(
            handle,
            log_dir.clone(),
            flow.clone(),
            Arc::clone(&outstanding),
        )
    });
    std::thread::spawn(move || {
        loop {
            let msg = match read_frame(&mut reader) {
                Ok(Some(v)) => v,
                Ok(None) => {
                    terminate_process_tree();
                    log_note(
                        "runner control pipe reached EOF; terminating process tree",
                        log_dir.as_deref(),
                    );
                    break;
                }
                Err(err) => {
                    terminate_process_tree();
                    log_note(
                        &format!(
                            "runner control pipe read failed; terminating process tree: {err}"
                        ),
                        log_dir.as_deref(),
                    );
                    break;
                }
            };
            match msg.message {
                Message::Stdin { payload } => {
                    let bytes = match decode_bytes(&payload.data_b64) {
                        Ok(bytes) => bytes,
                        Err(err) => {
                            terminate_process_tree();
                            log_note(
                                &format!(
                                    "runner received invalid stdin payload; terminating process tree: {err}"
                                ),
                                log_dir.as_deref(),
                            );
                            break;
                        }
                    };
                    if bytes.len() > IO_CHUNK_BYTES
                        || outstanding.load(Ordering::Acquire) >= IO_WINDOW_CHUNKS
                    {
                        terminate_process_tree();
                        log_note(
                            "runner stdin window exceeded; terminating process tree",
                            log_dir.as_deref(),
                        );
                        break;
                    }
                    if let Some(tx) = stdin_tx.as_ref() {
                        outstanding.fetch_add(1, Ordering::AcqRel);
                        match tx.try_send(Some(bytes)) {
                            Ok(()) => {}
                            // The writer stopped after the child closed its stdin and has
                            // already reported StdinClosed; input sent before the parent
                            // observed that is dropped like any other post-close input.
                            Err(mpsc::TrySendError::Disconnected(_)) => stdin_tx = None,
                            Err(mpsc::TrySendError::Full(_)) => {
                                terminate_process_tree();
                                break;
                            }
                        }
                    } else if let Some(flow) = &flow {
                        let _ = flow.send(Message::StdinClosed {
                            payload: EmptyPayload::default(),
                        });
                    }
                }
                Message::CloseStdin { .. } => {
                    if let Some(tx) = stdin_tx.take() {
                        let _ = tx.try_send(None);
                    }
                }
                Message::OutputAck { payload } => {
                    if !flow
                        .as_ref()
                        .is_some_and(|flow| flow.output.acknowledge(payload.stream, payload.chunks))
                    {
                        terminate_process_tree();
                        break;
                    }
                }
                Message::Resize {
                    payload: ResizePayload { rows, cols },
                } => {
                    let (Ok(rows), Ok(cols)) = (i16::try_from(rows), i16::try_from(cols)) else {
                        log_note(
                            &format!("runner rejected invalid terminal size {cols}x{rows}"),
                            log_dir.as_deref(),
                        );
                        continue;
                    };
                    if let Ok(guard) = hpc_handle.lock()
                        && let Some(hpc) = guard.as_ref()
                    {
                        let result =
                            // SAFETY: The mutex guard prevents the shared pseudoconsole handle from
                            // being cleared during resizing; the validated dimensions fit
                            // COORD and the owner remains alive.
                            unsafe { ResizePseudoConsole(*hpc, COORD { X: cols, Y: rows }) };
                        if result != 0 {
                            log_note(
                                &format!(
                                    "runner failed to resize pseudoconsole to {cols}x{rows}: HRESULT 0x{result:08x}"
                                ),
                                log_dir.as_deref(),
                            );
                        }
                    }
                }
                Message::Terminate { .. } => {
                    terminate_process_tree();
                    break;
                }
                other => {
                    terminate_process_tree();
                    log_note(
                        &format!(
                            "runner received unexpected control message; terminating process tree: {other:?}"
                        ),
                        log_dir.as_deref(),
                    );
                    break;
                }
            }
        }
        if let Some(flow) = flow {
            flow.output.stop();
        }
    })
}

fn spawn_stdin_writer(
    handle: HANDLE,
    log_dir: Option<PathBuf>,
    flow: Option<RunnerFlow>,
    outstanding: Arc<AtomicUsize>,
) -> mpsc::SyncSender<Option<Vec<u8>>> {
    // One reserved EOF slot; the parser never blocks on this queue. The byte
    // window includes the chunk currently inside synchronous WriteFile.
    let (stdin_tx, stdin_rx) = mpsc::sync_channel::<Option<Vec<u8>>>(IO_WINDOW_CHUNKS + 1);
    let handle_addr = handle as usize;
    std::thread::spawn(move || {
        let handle = handle_addr as HANDLE;
        'writer: while let Ok(Some(bytes)) = stdin_rx.recv() {
            let mut offset = 0usize;
            while offset < bytes.len() {
                let chunk = &bytes[offset..];
                let chunk_len = chunk.len().min(u32::MAX as usize);
                let mut written = 0u32;
                // SAFETY: This worker owns handle until its final CloseHandle. chunk is readable
                // for chunk_len bytes, written is a writable u32, and null OVERLAPPED
                // requests synchronous IO.
                let ok = unsafe {
                    windows_sys::Win32::Storage::FileSystem::WriteFile(
                        handle,
                        chunk.as_ptr(),
                        chunk_len as u32,
                        &mut written,
                        ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    log_note(
                        &format!(
                            "runner stdin write failed after {offset} bytes: {}",
                            unsafe { GetLastError() }
                        ),
                        log_dir.as_deref(),
                    );
                    break 'writer;
                }
                if written == 0 {
                    log_note(
                        "runner stdin write made no progress; closing child stdin",
                        log_dir.as_deref(),
                    );
                    break 'writer;
                }
                offset += written as usize;
            }
            outstanding.fetch_sub(1, Ordering::AcqRel);
            if let Some(flow) = &flow
                && !flow.send(Message::StdinAck {
                    payload: EmptyPayload::default(),
                })
            {
                break;
            }
        }
        if let Some(flow) = &flow {
            let _ = flow.send(Message::StdinClosed {
                payload: EmptyPayload::default(),
            });
        }
        // SAFETY: Ownership of the stdin write handle was transferred to this worker, which closes
        // it exactly once after its write loop ends.
        unsafe {
            CloseHandle(handle);
        }
    });
    stdin_tx
}

/// Entry point for the Windows command runner process.
pub fn main() -> Result<()> {
    let mut pipe_in = None;
    let mut pipe_out = None;
    for arg in std::env::args().skip(1) {
        if let Some(rest) = arg.strip_prefix("--pipe-in=") {
            pipe_in = Some(rest.to_string());
        } else if let Some(rest) = arg.strip_prefix("--pipe-out=") {
            pipe_out = Some(rest.to_string());
        }
    }

    let Some(pipe_in) = pipe_in else {
        anyhow::bail!("runner: no pipe-in provided");
    };
    let Some(pipe_out) = pipe_out else {
        anyhow::bail!("runner: no pipe-out provided");
    };

    // Open both pipe ends under guards first so a failure on the second open cannot leak the
    // first HANDLE. Only after both opens succeed do we transfer ownership into `File`, which
    // then becomes responsible for closing them.
    let h_pipe_in = OwnedWinHandle::new(open_pipe(&pipe_in, FILE_GENERIC_READ)?);
    let h_pipe_out = OwnedWinHandle::new(open_pipe(&pipe_out, FILE_GENERIC_WRITE)?);
    // SAFETY: into_raw relinquishes the sole OwnedWinHandle ownership of the open pipe; File takes
    // that ownership and will close it.
    let mut pipe_read = unsafe { File::from_raw_handle(h_pipe_in.into_raw() as _) };
    // SAFETY: into_raw relinquishes the sole OwnedWinHandle ownership of the open pipe; File takes
    // that ownership before being shared under the mutex.
    let pipe_write = Arc::new(StdMutex::new(unsafe {
        File::from_raw_handle(h_pipe_out.into_raw() as _)
    }));

    let req = match read_spawn_request(&mut pipe_read) {
        Ok(v) => v,
        Err(err) => {
            let _ = send_error(
                &pipe_write,
                ErrorStage::ReadSpawnRequest,
                /*windows_error_code*/ None,
                err.to_string(),
            );
            return Err(err);
        }
    };

    let ipc_spawn = match spawn_ipc_process(&req) {
        Ok(value) => value,
        Err(err) => {
            let _ = send_error(
                &pipe_write,
                ErrorStage::SpawnChild,
                windows_error_code(&err),
                err.to_string(),
            );
            return Err(err);
        }
    };
    let log_dir = Some(ipc_spawn.log_dir.as_path());
    let pi = ipc_spawn.pi;
    let stdout_handle = ipc_spawn.stdout_handle;
    let stderr_handle = ipc_spawn.stderr_handle;
    let job = Arc::clone(&ipc_spawn.job);
    let mut conpty_owner = ipc_spawn.conpty_owner;
    let stdin_handle = ipc_spawn.stdin_handle;
    let hpc_handle = Arc::new(StdMutex::new(ipc_spawn.hpc_handle));
    let control_process_handle = ipc_spawn.control_process_handle;

    let msg = FramedMessage {
        version: IPC_PROTOCOL_VERSION,
        message: Message::SpawnReady {
            payload: SpawnReady {
                // SAFETY: pi.hProcess is the live process handle retained by ipc_spawn until
                // process cleanup; GetProcessId only queries it.
                process_id: unsafe { GetProcessId(pi.hProcess) },
            },
        },
    };
    if let Err(err) = if let Ok(mut guard) = pipe_write.lock() {
        write_frame(&mut *guard, &msg)
    } else {
        anyhow::bail!("runner spawn_ready write failed: pipe_write lock poisoned");
    } {
        let _ = send_error(
            &pipe_write,
            ErrorStage::WriteSpawnReady,
            /*windows_error_code*/ None,
            err.to_string(),
        );
        return Err(err);
    }
    let log_dir_owned = log_dir.map(Path::to_path_buf);
    let output_window = Arc::new(OutputWindow::default());
    let out_thread = spawn_output_reader(
        Arc::clone(&pipe_write),
        stdout_handle,
        OutputStream::Stdout,
        log_dir_owned.clone(),
        Arc::clone(&output_window),
    );
    let err_thread = if stderr_handle != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        Some(spawn_output_reader(
            Arc::clone(&pipe_write),
            stderr_handle,
            OutputStream::Stderr,
            log_dir_owned.clone(),
            Arc::clone(&output_window),
        ))
    } else {
        None
    };

    let _input_thread = spawn_input_loop(
        pipe_read,
        stdin_handle,
        Arc::clone(&hpc_handle),
        Arc::clone(&job),
        control_process_handle,
        log_dir_owned,
        Some(RunnerFlow {
            writer: Arc::clone(&pipe_write),
            output: output_window,
        }),
    );

    let timeout = windows_wait_timeout(req.timeout_ms);
    let wait_outcome = wait_for_process(pi.hProcess, timeout);
    let timed_out = matches!(wait_outcome, ProcessWaitOutcome::TimedOut);
    let child_stopped = match wait_outcome {
        ProcessWaitOutcome::Exited => {
            if let Err(err) = job.preserve_descendants() {
                log_note(
                    &format!("runner failed to preserve descendants after root exit: {err}"),
                    log_dir,
                );
            }
            true
        }
        ProcessWaitOutcome::TimedOut => {
            terminate_job_or_process(&job, pi.hProcess, log_dir);
            wait_for_terminated_root(pi.hProcess, log_dir)
        }
        ProcessWaitOutcome::Failed {
            wait_result,
            windows_error,
        } => {
            log_failed_wait(
                "runner failed to wait for root process; terminating process tree",
                wait_result,
                windows_error,
                log_dir,
            );
            terminate_job_or_process(&job, pi.hProcess, log_dir);
            wait_for_terminated_root(pi.hProcess, log_dir)
        }
    };

    let exit_code: Option<i32>;
    // SAFETY: The spawn result owns both process/thread handles until this cleanup. raw_exit is
    // writable for GetExitCodeProcess, and each non-null handle is closed once after
    // waiting.
    unsafe {
        if timed_out && child_stopped {
            exit_code = Some(128 + 64);
        } else if child_stopped {
            let mut raw_exit: u32 = 0;
            exit_code = (GetExitCodeProcess(pi.hProcess, &mut raw_exit) != 0)
                .then_some(raw_exit as i32);
        } else {
            exit_code = None;
        }
        if !pi.hThread.is_null() {
            CloseHandle(pi.hThread);
        }
        if !pi.hProcess.is_null() {
            CloseHandle(pi.hProcess);
        }
    }

    if let Ok(mut guard) = hpc_handle.lock() {
        let _ = guard.take();
    }
    // Preserved or breakaway descendants can hold inherited output handles open indefinitely, and
    // a preserved job ignores Terminate, so reader EOF must not withhold the exit frame.
    let readers = std::iter::once(&out_thread)
        .chain(err_thread.as_ref())
        .collect::<Vec<_>>();
    if child_stopped && !wait_for_output_readers(&readers, OUTPUT_DRAIN_AFTER_ROOT_EXIT) {
        log_note(
            "runner output still open after the root stopped; descendants retain stdout/stderr",
            log_dir,
        );
    }
    if out_thread.is_finished() && out_thread.join().is_err() {
        log_note("runner stdout reader thread panicked", log_dir);
    }
    if let Some(thread) = err_thread
        && thread.is_finished()
        && thread.join().is_err()
    {
        log_note("runner stderr reader thread panicked", log_dir);
    }

    if let Some(exit_code) = exit_code {
        let exit_msg = FramedMessage {
            version: IPC_PROTOCOL_VERSION,
            message: Message::Exit {
                payload: ExitPayload {
                    exit_code,
                    timed_out,
                },
            },
        };
        if let Ok(mut guard) = pipe_write.lock()
            && let Err(err) = write_frame(&mut *guard, &exit_msg)
        {
            log_note(&format!("runner exit write failed: {err}"), log_dir);
        }
    } else {
        let _ = send_error(
            &pipe_write,
            ErrorStage::ObserveChildExit,
            None,
            "root process exit status unavailable".to_string(),
        );
    }

    // Publish root exit before potentially blocking older ConPTY teardown.
    drop(conpty_owner.take());
    // This is the runner's own failure status, never a fabricated child Exit.
    std::process::exit(exit_code.unwrap_or(1));
}

