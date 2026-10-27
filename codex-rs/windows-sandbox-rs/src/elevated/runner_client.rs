use crate::identity::SandboxCreds;
use crate::ipc_framed::ErrorPayload;
use crate::ipc_framed::ErrorStage;
use crate::ipc_framed::FramedMessage;
use crate::ipc_framed::IPC_PROTOCOL_VERSION;
use crate::ipc_framed::Message;
use crate::ipc_framed::SpawnRequest;
use crate::ipc_framed::read_frame;
use crate::ipc_framed::write_frame;
use crate::runner_pipe::PIPE_ACCESS_INBOUND;
use crate::runner_pipe::PIPE_ACCESS_OUTBOUND;
use crate::runner_pipe::connect_pipe;
use crate::runner_pipe::create_named_pipe;
use crate::runner_pipe::find_runner_exe;
use crate::runner_pipe::pipe_pair;
use crate::winutil::quote_windows_arg;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use std::ffi::c_void;
use std::fs::File;
use std::io::Read;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::ptr;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
use windows_sys::Win32::Foundation::ERROR_LOGON_FAILURE;
use windows_sys::Win32::Foundation::ERROR_NO_DATA;
use windows_sys::Win32::Foundation::ERROR_NO_SUCH_LOGON_SESSION;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Threading::CreateProcessWithLogonW;
use windows_sys::Win32::System::Threading::LOGON_WITH_PROFILE;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::STARTUPINFOW;
use windows_sys::Win32::System::Threading::TerminateProcess;

const RUNNER_SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(15);
const RUNNER_PIPE_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
struct RunnerLogonError {
    code: u32,
}

impl std::fmt::Display for RunnerLogonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CreateProcessWithLogonW failed: {}", self.code)
    }
}

impl std::error::Error for RunnerLogonError {}

#[derive(Debug)]
pub(crate) struct RunnerStartupError {
    payload: ErrorPayload,
}

impl RunnerStartupError {
    pub(crate) fn new(payload: ErrorPayload) -> Self {
        Self { payload }
    }
}

impl std::fmt::Display for RunnerStartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "runner failed during {:?}: {}",
            self.payload.stage, self.payload.message
        )?;
        if let Some(code) = self.payload.windows_error_code {
            write!(f, " (Windows error {code})")?;
        }
        Ok(())
    }
}

impl std::error::Error for RunnerStartupError {}

pub(crate) struct RunnerTransport {
    pipe_write: File,
    pipe_read: File,
    runner: RunnerProcess,
}

fn is_refreshable_windows_error(code: u32) -> bool {
    matches!(code, ERROR_LOGON_FAILURE | ERROR_NO_SUCH_LOGON_SESSION)
}

fn command_targets_windows_apps(command: &[String]) -> bool {
    command.first().is_some_and(|program| {
        Path::new(program).components().any(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case("WindowsApps")
        })
    })
}

pub(crate) fn is_refreshable_sandbox_creds_error(err: &anyhow::Error, command: &[String]) -> bool {
    if err
        .downcast_ref::<RunnerLogonError>()
        .is_some_and(|err| is_refreshable_windows_error(err.code))
    {
        return true;
    }

    err.downcast_ref::<RunnerStartupError>().is_some_and(|err| {
        err.payload.stage == ErrorStage::SpawnChild
            && err.payload.windows_error_code.is_some_and(|code| {
                // AppX activation can return 1312 for a healthy sandbox token. Rotating the
                // account password cannot make the same WindowsApps command launch.
                is_refreshable_windows_error(code)
                    && (code != ERROR_NO_SUCH_LOGON_SESSION
                        || !command_targets_windows_apps(command))
            })
    })
}

pub(crate) fn retry_runner_spawn_once<T>(
    sandbox_creds: SandboxCreds,
    command: &[String],
    mut spawn: impl FnMut(SandboxCreds) -> Result<T>,
    refresh: impl FnOnce() -> Result<SandboxCreds>,
) -> Result<T> {
    match spawn(sandbox_creds) {
        Ok(result) => Ok(result),
        Err(err) if is_refreshable_sandbox_creds_error(&err, command) => spawn(refresh()?),
        Err(err) => Err(err),
    }
}

impl RunnerTransport {
    pub(crate) fn into_capture_files(self) -> (File, File, RunnerProcess) {
        let Self { pipe_write, pipe_read, runner } = self;
        (pipe_write, pipe_read, runner)
    }

    pub(crate) fn into_files(self) -> (File, File) {
        let Self { pipe_write, pipe_read, mut runner } = self;
        runner.disarmed = true;
        (pipe_write, pipe_read)
    }
}

pub(crate) struct RunnerProcess {
    process: File,
    disarmed: bool,
}

impl RunnerProcess {
    pub(crate) fn terminate(&self) {
        // SAFETY: This owner retains the process HANDLE throughout termination.
        unsafe { TerminateProcess(self.process.as_raw_handle() as HANDLE, 1); }
    }
}

impl Drop for RunnerProcess {
    fn drop(&mut self) {
        if !self.disarmed {
            // SAFETY: This guard owns the process HANDLE until File closes it.
            unsafe { TerminateProcess(self.process.as_raw_handle() as HANDLE, 1); }
        }
    }
}

/// Cancellation and request admission share one atomic linearization point.
pub(crate) struct RunnerAdmission {
    state: std::sync::atomic::AtomicU8,
    cancelled: Box<dyn Fn() -> bool + Send + Sync>,
}

impl RunnerAdmission {
    pub(crate) fn new(cancelled: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self { state: std::sync::atomic::AtomicU8::new(0), cancelled: Box::new(cancelled) }
    }
    pub(crate) fn cancel(&self) { self.state.store(1, Ordering::Release); }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == 1 || (self.cancelled)()
    }
    fn admit(&self) -> Result<()> {
        anyhow::ensure!(!self.is_cancelled(), "runner launch cancelled before admission");
        let state = self.state.load(Ordering::Acquire);
        anyhow::ensure!(state != 1 && self.state.compare_exchange(state, 2, Ordering::AcqRel, Ordering::Acquire).is_ok(), "runner launch cancelled before admission");
        Ok(())
    }
}

fn check_startup(deadline: std::time::Instant, admission: &RunnerAdmission) -> std::io::Result<()> {
    if admission.is_cancelled() {
        return Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "runner launch cancelled"));
    }
    if std::time::Instant::now() >= deadline {
        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "runner startup timed out"));
    }
    Ok(())
}

fn connect_pipe_with_deadline(pipe: &File, expected_runner_pid: u32, deadline: std::time::Instant, admission: &RunnerAdmission) -> Result<()> {
    loop {
        check_startup(deadline, admission)?;
        match connect_pipe(pipe.as_raw_handle() as HANDLE, expected_runner_pid) {
            Ok(()) => return Ok(()),
            Err(err) if matches!(err.raw_os_error(), Some(232 | 233 | 536)) => thread::sleep(Duration::from_millis(5)), // no connected client yet
            Err(err) => return Err(err.into()),
        }
    }
}

struct StartupIo<'a> {
    pipe: &'a File,
    deadline: std::time::Instant,
    admission: &'a RunnerAdmission,
}

impl Read for StartupIo<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if bytes.is_empty() { return Ok(0); }
        loop {
            check_startup(self.deadline, self.admission)?;
            let mut read_bytes = 0;
            // File::read maps ERROR_NO_DATA to BrokenPipe and then EOF. On a
            // PIPE_NOWAIT pipe it only means the runner has not replied yet.
            // SAFETY: self.pipe owns the synchronous, nonblocking pipe handle;
            // bytes and read_bytes remain writable throughout this call.
            let ok = unsafe {
                ReadFile(
                    self.pipe.as_raw_handle() as HANDLE,
                    bytes.as_mut_ptr(),
                    bytes.len().min(u32::MAX as usize) as u32,
                    &mut read_bytes,
                    ptr::null_mut(),
                )
            };
            if ok != 0 {
                return Ok(read_bytes as usize);
            }
            // SAFETY: Capture ReadFile's thread-local error before other calls.
            match unsafe { GetLastError() } {
                ERROR_NO_DATA => thread::sleep(Duration::from_millis(5)),
                ERROR_BROKEN_PIPE => return Ok(0),
                error => return Err(std::io::Error::from_raw_os_error(error as i32)),
            }
        }
    }
}

impl std::io::Write for StartupIo<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.is_empty() { return Ok(0); }
        loop {
            check_startup(self.deadline, self.admission)?;
            match std::io::Write::write(&mut self.pipe, bytes) {
                Ok(0) => thread::sleep(Duration::from_millis(5)),
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> { check_startup(self.deadline, self.admission) }
}

fn restore_blocking_pipe(pipe: &File) -> Result<()> {
    set_pipe_wait_mode(pipe, false)
}

pub(crate) fn set_nonblocking_pipe(pipe: &File) -> Result<()> { set_pipe_wait_mode(pipe, true) }

fn set_pipe_wait_mode(pipe: &File, nonblocking: bool) -> Result<()> {
    let mode = windows_sys::Win32::System::Pipes::PIPE_READMODE_BYTE
        | if nonblocking { windows_sys::Win32::System::Pipes::PIPE_NOWAIT } else { windows_sys::Win32::System::Pipes::PIPE_WAIT };
    // SAFETY: This owner retains the duplex pipe and its FILE_WRITE_ATTRIBUTES
    // access throughout the mode change; no other thread performs I/O yet.
    if unsafe { windows_sys::Win32::System::Pipes::SetNamedPipeHandleState(pipe.as_raw_handle() as HANDLE, &mode, ptr::null(), ptr::null()) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Caller owns a duplex server pipe already placed in PIPE_NOWAIT mode.
pub(crate) fn write_control_frame(pipe: &File, message: &FramedMessage) -> Result<()> {
    let admission = RunnerAdmission::new(|| false);
    write_frame(StartupIo { pipe, deadline: std::time::Instant::now() + Duration::from_secs(5), admission: &admission }, message)
}

pub(crate) fn spawn_runner_transport_with_admission(
    codex_home: &Path,
    cwd: &Path,
    sandbox_creds: &SandboxCreds,
    log_dir: Option<&Path>,
    spawn_request: SpawnRequest,
    admission: &RunnerAdmission,
) -> Result<RunnerTransport> {
    let deadline = std::time::Instant::now() + RUNNER_PIPE_CONNECT_TIMEOUT + RUNNER_SPAWN_READY_TIMEOUT;
    check_startup(deadline, admission)?;
    let (pipe_in_name, pipe_out_name) = pipe_pair();
    let h_pipe_in =
        create_named_pipe(&pipe_in_name, PIPE_ACCESS_OUTBOUND | PIPE_ACCESS_INBOUND, &sandbox_creds.username)?;
    // SAFETY: The successful pipe handle is uniquely owned here; File takes its close obligation.
    let pipe_write = unsafe { File::from_raw_handle(h_pipe_in as _) };
    let h_pipe_out =
        create_named_pipe(&pipe_out_name, PIPE_ACCESS_OUTBOUND | PIPE_ACCESS_INBOUND, &sandbox_creds.username)?;
    // SAFETY: The successful pipe handle is uniquely owned here; File takes its close obligation.
    let pipe_read = unsafe { File::from_raw_handle(h_pipe_out as _) };

    let runner_exe = find_runner_exe(codex_home, log_dir)?;
    let runner_cmdline = runner_exe
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("runner executable path is not valid Unicode"))?;
    let runner_full_cmd = format!(
        "{} {} {}",
        quote_windows_arg(&runner_cmdline),
        quote_windows_arg(&format!("--pipe-in={pipe_in_name}")),
        quote_windows_arg(&format!("--pipe-out={pipe_out_name}"))
    );
    let mut cmdline_vec = to_wide(&runner_full_cmd);
    let exe_w = to_wide(&runner_cmdline);
    let cwd_w = to_wide(cwd);
    let user_w = to_wide(&sandbox_creds.username);
    let domain_w = to_wide(".");
    let password_w = to_wide(&sandbox_creds.password);
    // SAFETY: STARTUPINFOW consists of integer fields and nullable pointers; zero is valid
    // initialization before cb is assigned.
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    // SAFETY: PROCESS_INFORMATION is an output structure containing nullable handles and integer
    // IDs, so zero is a valid initial representation.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let env_block: Option<Vec<u16>> = None;
    // Serialize policy validation plus suspended-root assignment with setup.
    // Only this short launch transaction holds the mutex, never pipe readiness.
    let policy_launch = sandbox_creds.offline_proxy_settings.as_ref().map(|settings| -> Result<_> {
        let guard = crate::setup_mutex::acquire_setup_mutex_for_launch()?;
        anyhow::ensure!(crate::offline_network_policy::policy_matches(
            &sandbox_creds.username, &settings.proxy_ports, settings.allow_local_binding,
        )?, "offline network policy changed during launch preparation; retry the command");
        Ok((guard, crate::offline_network_policy::OfflinePolicyJob::open()?))
    }).transpose()?;

    // CreateProcessWithLogonW uses the default child error mode by default.
    // Do not exchange the parent's process-wide mode: overlapping launches
    // could restore one another's temporary values, and affect unrelated work.
    // SAFETY: All credential, executable and cwd strings are live NUL-terminated buffers;
    // cmdline_vec is writable, si has its required size, and pi is a writable output
    // structure. A null environment requests inheritance.
    let spawn_res = unsafe {
        CreateProcessWithLogonW(
            user_w.as_ptr(),
            domain_w.as_ptr(),
            password_w.as_ptr(),
            LOGON_WITH_PROFILE,
            exe_w.as_ptr(),
            cmdline_vec.as_mut_ptr(),
            windows_sys::Win32::System::Threading::CREATE_NO_WINDOW
                | windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT
                | if policy_launch.is_some() { windows_sys::Win32::System::Threading::CREATE_SUSPENDED } else { 0 },
            env_block
                .as_ref()
                .map(|block| block.as_ptr() as *const c_void)
                .unwrap_or(ptr::null()),
            cwd_w.as_ptr(),
            &si,
            &mut pi,
        )
    };
    // SAFETY: Capture the native creation error immediately, before cleanup.
    let spawn_error = unsafe { GetLastError() };
    if spawn_res == 0 {
        return Err(
            anyhow::Error::from(RunnerLogonError { code: spawn_error }).context(format!(
                "launch runner {} (exists: {}) in {} (exists: {})",
                runner_exe.display(),
                runner_exe.is_file(),
                cwd.display(),
                cwd.is_dir(),
            )),
        );
    }
    // Own process rollback until the async caller accepts transport delivery.
    // SAFETY: CreateProcessWithLogonW returned this unique owned process handle.
    let runner = RunnerProcess { process: unsafe { File::from_raw_handle(pi.hProcess as _) }, disarmed: false };
    // SAFETY: The successful process creation returned this owned thread handle.
    let thread = unsafe { File::from_raw_handle(pi.hThread as _) };
    if let Some((_, job)) = &policy_launch {
        job.attach(pi.hProcess)?;
        crate::logging::custody_diagnostic(|| format!(
            "event=policy_attach runner_pid={} attached=true", pi.dwProcessId), log_dir);
        // SAFETY: Assignment succeeded while the newly-created root is suspended.
        if unsafe { windows_sys::Win32::System::Threading::ResumeThread(pi.hThread) } == u32::MAX {
            return Err(std::io::Error::last_os_error()).context("resume offline policy-contained runner");
        }
    }
    drop(thread);
    drop(policy_launch);
    connect_pipe_with_deadline(&pipe_write, pi.dwProcessId, deadline, admission)?;
    connect_pipe_with_deadline(&pipe_read, pi.dwProcessId, deadline, admission)?;
    admission.admit()?;
    let request = FramedMessage { version: IPC_PROTOCOL_VERSION, message: Message::SpawnRequest { payload: Box::new(spawn_request) } };
    write_frame(StartupIo { pipe: &pipe_write, deadline, admission }, &request)?;
    let response = read_frame(StartupIo { pipe: &pipe_read, deadline, admission })?
        .ok_or_else(|| anyhow::anyhow!("runner pipe closed before spawn_ready"))?;
    match response.message {
        Message::SpawnReady { .. } => {}
        Message::Error { payload } => return Err(RunnerStartupError::new(payload).into()),
        other => anyhow::bail!("expected spawn_ready from runner, got {other:?}"),
    }
    restore_blocking_pipe(&pipe_write)?;
    restore_blocking_pipe(&pipe_read)?;
    Ok(RunnerTransport { pipe_write, pipe_read, runner })
}




