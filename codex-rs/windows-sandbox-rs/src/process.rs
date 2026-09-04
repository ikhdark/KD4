use crate::desktop::LaunchDesktop;
use crate::logging;
use crate::proc_thread_attr::ProcThreadAttributeList;
use crate::winutil::argv_to_command_line;
use crate::winutil::format_last_error;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use codex_utils_pty::JobObject;
use codex_utils_pty::WindowsChildCreationScope;
use codex_utils_pty::with_windows_child_creation;
use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetHandleInformation;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
use windows_sys::Win32::Foundation::HANDLE_FLAG_PROTECT_FROM_CLOSE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::SetHandleInformation;
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Console::GetStdHandle;
use windows_sys::Win32::System::Console::STD_ERROR_HANDLE;
use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;
use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
use windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT;
use windows_sys::Win32::System::Threading::CreateProcessAsUserW;
use windows_sys::Win32::System::Threading::EXTENDED_STARTUPINFO_PRESENT;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
use windows_sys::Win32::System::Threading::STARTUPINFOEXW;
use windows_sys::Win32::System::Threading::STARTUPINFOW;
use windows_sys::Win32::System::Threading::TerminateProcess;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

pub struct CreatedProcess {
    pub process_info: PROCESS_INFORMATION,
    pub startup_info: STARTUPINFOW,
    pub(crate) job: Arc<JobObject>,
    _desktop: LaunchDesktop,
}

/// Controls console creation for pipe-backed child processes.
pub enum ConsoleMode {
    Inherit,
    NoWindow,
}

pub fn make_env_block(env: &HashMap<String, String>) -> Vec<u16> {
    let mut items: Vec<(String, String)> =
        env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    items.sort_by(|a, b| {
        a.0.to_uppercase()
            .cmp(&b.0.to_uppercase())
            .then(a.0.cmp(&b.0))
    });
    let mut w: Vec<u16> = Vec::new();
    for (k, v) in items {
        let mut s = to_wide(format!("{k}={v}"));
        s.pop();
        w.extend_from_slice(&s);
        w.push(0);
    }
    w.push(0);
    w
}

unsafe fn standard_handles() -> io::Result<[HANDLE; 3]> {
    let mut handles = [ptr::null_mut(); 3];
    for (slot, kind) in
        handles
            .iter_mut()
            .zip([STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE])
    {
        let handle = GetStdHandle(kind);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::other(format!(
                "GetStdHandle({kind}) returned an invalid handle: {}",
                GetLastError()
            )));
        }
        *slot = handle;
    }
    Ok(handles)
}

fn unique_handles(handles: [HANDLE; 3]) -> Vec<HANDLE> {
    let mut unique = Vec::with_capacity(handles.len());
    for handle in handles {
        if !unique.contains(&handle) {
            unique.push(handle);
        }
    }
    unique
}

unsafe fn snapshot_handle_flags(handles: &[HANDLE]) -> io::Result<Vec<(HANDLE, u32)>> {
    handles
        .iter()
        .map(|&handle| {
            let mut flags = 0;
            if GetHandleInformation(handle, &mut flags) == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok((handle, flags))
            }
        })
        .collect()
}

unsafe fn set_inheritable(handles: &[(HANDLE, u32)]) -> io::Result<()> {
    for &(handle, _) in handles {
        if SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

unsafe fn restore_handle_flags(handles: &[(HANDLE, u32)]) -> io::Result<()> {
    let mut first_error = None;
    for &(handle, original_flags) in handles.iter().rev() {
        let restored = (|| {
            let mut current_flags = 0;
            if GetHandleInformation(handle, &mut current_flags) == 0 {
                return Err(io::Error::last_os_error());
            }
            if current_flags != original_flags {
                let restorable_flags = HANDLE_FLAG_INHERIT | HANDLE_FLAG_PROTECT_FROM_CLOSE;
                if SetHandleInformation(handle, restorable_flags, original_flags & restorable_flags)
                    == 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            let mut restored_flags = 0;
            if GetHandleInformation(handle, &mut restored_flags) == 0 {
                return Err(io::Error::last_os_error());
            }
            if restored_flags != original_flags {
                return Err(io::Error::other(format!(
                    "Windows handle flags were not restored exactly: expected {original_flags:#x}, got {restored_flags:#x}"
                )));
            }
            Ok(())
        })();
        if let Err(error) = restored
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn combine_restoration_error(primary: io::Error, restoration: io::Error) -> io::Error {
    io::Error::other(format!(
        "{primary}; exact handle-inheritance restoration also failed: {restoration}"
    ))
}

unsafe fn close_created_process_handles(pi: &PROCESS_INFORMATION) -> io::Result<()> {
    let mut first_error = None;
    for handle in [pi.hThread, pi.hProcess] {
        if CloseHandle(handle) == 0 && first_error.is_none() {
            first_error = Some(io::Error::last_os_error());
        }
    }
    first_error.map_or(Ok(()), Err)
}

unsafe fn cleanup_after_restoration_failure(
    job: &JobObject,
    pi: &PROCESS_INFORMATION,
    restoration_error: io::Error,
) -> io::Error {
    const CLEANUP_WAIT_MS: u32 = 30_000;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_FAILED: u32 = u32::MAX;

    let mut cleanup_errors = Vec::new();
    if let Err(job_error) = job.terminate()
        && TerminateProcess(pi.hProcess, 1) == 0
    {
        cleanup_errors.push(format!(
            "failed to terminate Job ({job_error}); process fallback also failed: {}",
            io::Error::last_os_error()
        ));
    }
    match WaitForSingleObject(pi.hProcess, CLEANUP_WAIT_MS) {
        WAIT_OBJECT_0 => {}
        WAIT_FAILED => cleanup_errors.push(format!(
            "failed to wait for terminated child: {}",
            io::Error::last_os_error()
        )),
        result => cleanup_errors.push(format!(
            "timed out or received unexpected result while reaping child: 0x{result:08x}"
        )),
    }
    if let Err(error) = close_created_process_handles(pi) {
        cleanup_errors.push(format!("failed to close created child handles: {error}"));
    }

    if cleanup_errors.is_empty() {
        restoration_error
    } else {
        io::Error::other(format!(
            "{restoration_error}; contained child cleanup also failed: {}",
            cleanup_errors.join("; ")
        ))
    }
}

#[cfg(test)]
static INJECT_POST_CREATE_RESTORATION_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
fn take_injected_post_create_restoration_failure() -> bool {
    INJECT_POST_CREATE_RESTORATION_FAILURE.swap(false, std::sync::atomic::Ordering::SeqCst)
}

#[cfg(not(test))]
fn take_injected_post_create_restoration_failure() -> bool {
    false
}

enum CoordinatedProcessCreation {
    Created(PROCESS_INFORMATION),
    CreatedNeedsCleanup {
        process_info: PROCESS_INFORMATION,
        restoration_error: io::Error,
    },
}

unsafe fn create_process_with_inherited_stdio(
    scope: &WindowsChildCreationScope,
    h_token: HANDLE,
    cmdline: &mut [u16],
    cwd_wide: &[u16],
    env_block: &[u16],
    creation_flags: u32,
    stdio: Option<(HANDLE, HANDLE, HANDLE)>,
    attrs: &mut ProcThreadAttributeList,
    si: &mut STARTUPINFOEXW,
) -> io::Result<CoordinatedProcessCreation> {
    let stdio_handles = match stdio {
        Some(handles) => [handles.0, handles.1, handles.2],
        None => standard_handles()?,
    };
    let inherited_handles = unique_handles(stdio_handles);
    attrs.set_handle_list(inherited_handles.clone())?;

    si.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = stdio_handles[0];
    si.StartupInfo.hStdOutput = stdio_handles[1];
    si.StartupInfo.hStdError = stdio_handles[2];

    let original_flags = snapshot_handle_flags(&inherited_handles)?;
    let mutation_result = set_inheritable(&original_flags);
    if let Err(mutation_error) = mutation_result {
        return match restore_handle_flags(&original_flags) {
            Ok(()) => Err(mutation_error),
            Err(restoration_error) => {
                scope.mark_inheritance_state_compromised();
                Err(combine_restoration_error(mutation_error, restoration_error))
            }
        };
    }

    si.lpAttributeList = attrs.as_mut_ptr();
    let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
    let created = CreateProcessAsUserW(
        h_token,
        ptr::null(),
        cmdline.as_mut_ptr(),
        ptr::null_mut(),
        ptr::null_mut(),
        1,
        creation_flags,
        env_block.as_ptr() as *mut c_void,
        cwd_wide.as_ptr(),
        &si.StartupInfo,
        &mut pi,
    );
    let creation_result = if created == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(pi)
    };

    let mut restoration_result = restore_handle_flags(&original_flags);
    if creation_result.is_ok() && take_injected_post_create_restoration_failure() {
        restoration_result = Err(io::Error::other(
            "injected post-create exact handle-inheritance restoration failure",
        ));
    }

    match (creation_result, restoration_result) {
        (Ok(pi), Ok(())) => Ok(CoordinatedProcessCreation::Created(pi)),
        (Err(creation_error), Ok(())) => Err(creation_error),
        (Err(creation_error), Err(restoration_error)) => {
            scope.mark_inheritance_state_compromised();
            Err(combine_restoration_error(creation_error, restoration_error))
        }
        (Ok(pi), Err(restoration_error)) => {
            scope.mark_inheritance_state_compromised();
            Ok(CoordinatedProcessCreation::CreatedNeedsCleanup {
                process_info: pi,
                restoration_error,
            })
        }
    }
}

/// # Safety
/// Caller must provide a valid primary token handle (`h_token`) with appropriate access,
/// and the `argv`, `cwd`, and `env_map` must remain valid for the duration of the call.
// Low-level CreateProcessAsUserW wrapper mirrors the Windows API shape.
#[allow(clippy::too_many_arguments)]
pub unsafe fn create_process_as_user(
    h_token: HANDLE,
    argv: &[String],
    cwd: &Path,
    env_map: &HashMap<String, String>,
    logs_base_dir: Option<&Path>,
    stdio: Option<(HANDLE, HANDLE, HANDLE)>,
    console_mode: ConsoleMode,
    use_private_desktop: bool,
) -> Result<CreatedProcess> {
    let cmdline_str = argv_to_command_line(argv);
    let mut cmdline: Vec<u16> = to_wide(&cmdline_str);
    let env_block = make_env_block(env_map);
    let desktop = LaunchDesktop::prepare(use_private_desktop, logs_base_dir)?;
    let job = Arc::new(JobObject::create().context("create process job")?);
    let cwd_wide = to_wide(cwd);
    let env_block_len = env_block.len();
    let console_flags = match (&stdio, console_mode) {
        (Some(_), ConsoleMode::NoWindow) => CREATE_NO_WINDOW,
        (Some(_), ConsoleMode::Inherit)
        | (None, ConsoleMode::Inherit)
        | (None, ConsoleMode::NoWindow) => 0,
    };
    let mut attrs = ProcThreadAttributeList::new(2)?;
    attrs.set_job(job.as_raw_handle() as HANDLE)?;

    let mut si: STARTUPINFOEXW = std::mem::zeroed();
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    // Some processes (e.g., PowerShell) can fail with STATUS_DLL_INIT_FAILED
    // if lpDesktop is not set when launching with a restricted token.
    // Point explicitly at the interactive desktop or a private desktop.
    si.StartupInfo.lpDesktop = desktop.startup_info_desktop();
    let creation_flags = CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT | console_flags;
    let coordinated_outcome = with_windows_child_creation(|scope| unsafe {
        create_process_with_inherited_stdio(
            scope,
            h_token,
            &mut cmdline,
            &cwd_wide,
            &env_block,
            creation_flags,
            stdio,
            &mut attrs,
            &mut si,
        )
    });
    let process_result = coordinated_outcome.and_then(|outcome| match outcome {
        CoordinatedProcessCreation::Created(pi) => Ok(pi),
        CoordinatedProcessCreation::CreatedNeedsCleanup {
            process_info,
            restoration_error,
        } => Err(unsafe {
            cleanup_after_restoration_failure(&job, &process_info, restoration_error)
        }),
    });
    let pi = match process_result {
        Ok(pi) => pi,
        Err(error) => {
            let err = error.raw_os_error().unwrap_or_default();
            let msg = format!(
                "Windows process creation failed: {} ({}) | cwd={} | cmd={} | env_u16_len={} | si_flags={} | creation_flags={}",
                err,
                format_last_error(err),
                cwd.display(),
                cmdline_str,
                env_block_len,
                si.StartupInfo.dwFlags,
                creation_flags,
            );
            logging::debug_log(&msg, logs_base_dir);
            return Err(error).context(msg);
        }
    };

    Ok(CreatedProcess {
        process_info: pi,
        startup_info: si.StartupInfo,
        job,
        _desktop: desktop,
    })
}

/// Controls whether the child's stdin handle is kept open for writing.
#[allow(dead_code)]
pub enum StdinMode {
    Closed,
    Open,
}

/// Controls how stderr is wired for a pipe-spawned process.
#[allow(dead_code)]
pub enum StderrMode {
    MergeStdout,
    Separate,
}

/// Handles returned by `spawn_process_with_pipes`.
#[allow(dead_code)]
pub struct PipeSpawnHandles {
    pub process: PROCESS_INFORMATION,
    job: Arc<JobObject>,
    pub stdin_write: Option<HANDLE>,
    pub stdout_read: HANDLE,
    pub stderr_read: Option<HANDLE>,
    pub(crate) desktop: LaunchDesktop,
}

impl PipeSpawnHandles {
    /// Returns the Job Object containing the spawned process.
    pub fn job(&self) -> Arc<JobObject> {
        Arc::clone(&self.job)
    }
}

/// Spawns a process with anonymous pipes and returns the relevant handles.
///
/// # Safety
///
/// `h_token` must be a valid primary token handle with the access rights required by
/// `CreateProcessAsUserW`, and it must remain valid for the duration of this call.
#[allow(clippy::too_many_arguments)]
pub unsafe fn spawn_process_with_pipes(
    h_token: HANDLE,
    argv: &[String],
    cwd: &Path,
    env_map: &HashMap<String, String>,
    stdin_mode: StdinMode,
    stderr_mode: StderrMode,
    console_mode: ConsoleMode,
    use_private_desktop: bool,
    logs_base_dir: Option<&Path>,
) -> Result<PipeSpawnHandles> {
    let mut in_r: HANDLE = ptr::null_mut();
    let mut in_w: HANDLE = ptr::null_mut();
    let mut out_r: HANDLE = ptr::null_mut();
    let mut out_w: HANDLE = ptr::null_mut();
    let mut err_r: HANDLE = ptr::null_mut();
    let mut err_w: HANDLE = ptr::null_mut();
    unsafe {
        if CreatePipe(&mut in_r, &mut in_w, ptr::null_mut(), 0) == 0 {
            return Err(anyhow!("CreatePipe stdin failed: {}", GetLastError()));
        }
        if CreatePipe(&mut out_r, &mut out_w, ptr::null_mut(), 0) == 0 {
            CloseHandle(in_r);
            CloseHandle(in_w);
            return Err(anyhow!("CreatePipe stdout failed: {}", GetLastError()));
        }
        if matches!(stderr_mode, StderrMode::Separate)
            && CreatePipe(&mut err_r, &mut err_w, ptr::null_mut(), 0) == 0
        {
            CloseHandle(in_r);
            CloseHandle(in_w);
            CloseHandle(out_r);
            CloseHandle(out_w);
            return Err(anyhow!("CreatePipe stderr failed: {}", GetLastError()));
        }
    }

    let stderr_handle = match stderr_mode {
        StderrMode::MergeStdout => out_w,
        StderrMode::Separate => err_w,
    };

    let stdio = Some((in_r, out_w, stderr_handle));
    let spawn_result = unsafe {
        create_process_as_user(
            h_token,
            argv,
            cwd,
            env_map,
            logs_base_dir,
            stdio,
            console_mode,
            use_private_desktop,
        )
    };
    let created = match spawn_result {
        Ok(v) => v,
        Err(err) => {
            unsafe {
                CloseHandle(in_r);
                CloseHandle(in_w);
                CloseHandle(out_r);
                CloseHandle(out_w);
                if matches!(stderr_mode, StderrMode::Separate) {
                    CloseHandle(err_r);
                    CloseHandle(err_w);
                }
            }
            return Err(err);
        }
    };
    let CreatedProcess {
        process_info: pi,
        job,
        _desktop: desktop,
        ..
    } = created;

    unsafe {
        CloseHandle(in_r);
        CloseHandle(out_w);
        if matches!(stderr_mode, StderrMode::Separate) {
            CloseHandle(err_w);
        }
        if matches!(stdin_mode, StdinMode::Closed) {
            CloseHandle(in_w);
        }
    }

    Ok(PipeSpawnHandles {
        process: pi,
        job,
        stdin_write: match stdin_mode {
            StdinMode::Open => Some(in_w),
            StdinMode::Closed => None,
        },
        stdout_read: out_r,
        stderr_read: match stderr_mode {
            StderrMode::Separate => Some(err_r),
            StderrMode::MergeStdout => None,
        },
        desktop,
    })
}

/// Reads a HANDLE until EOF and invokes `on_chunk` for each read.
pub fn read_handle_loop<F>(handle: HANDLE, mut on_chunk: F) -> std::thread::JoinHandle<()>
where
    F: FnMut(&[u8]) + Send + 'static,
{
    // Raw Win32 handles are pointer-typed in windows-sys 0.61. Transfer the
    // address value across the thread boundary and reconstruct the opaque
    // handle in the reader thread; the caller retains responsibility for the
    // handle's lifetime.
    let handle_addr = handle as usize;
    std::thread::spawn(move || {
        let handle = handle_addr as HANDLE;
        let mut buf = [0u8; 8192];
        loop {
            let mut read_bytes: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    handle,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut read_bytes,
                    ptr::null_mut(),
                )
            };
            if ok == 0 || read_bytes == 0 {
                break;
            }
            on_chunk(&buf[..read_bytes as usize]);
        }
        unsafe {
            CloseHandle(handle);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::get_current_token_for_restriction;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    use std::os::windows::io::RawHandle;
    use std::time::Duration;
    use std::time::Instant;
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    use windows_sys::Win32::System::Threading::INFINITE;

    const HANDLE_PROBE_ENV: &str = "KD4_SANDBOX_HANDLE_ALLOWLIST_PROBE";
    const CLEANUP_CHILD_ENV: &str = "KD4_SANDBOX_RESTORATION_CLEANUP_CHILD";
    const COMPROMISE_HELPER_ENV: &str = "KD4_SANDBOX_COMPROMISE_HELPER";

    unsafe fn owned_handle(handle: HANDLE) -> OwnedHandle {
        OwnedHandle::from_raw_handle(handle as RawHandle)
    }

    unsafe fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
        let mut read = ptr::null_mut();
        let mut write = ptr::null_mut();
        if CreatePipe(&mut read, &mut write, ptr::null_mut(), 0) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((owned_handle(read), owned_handle(write)))
    }

    unsafe fn flags(handle: HANDLE) -> io::Result<u32> {
        let mut flags = 0;
        if GetHandleInformation(handle, &mut flags) == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(flags)
        }
    }

    struct ProtectFromCloseReset(HANDLE);

    impl Drop for ProtectFromCloseReset {
        fn drop(&mut self) {
            unsafe {
                SetHandleInformation(self.0, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0);
            }
        }
    }

    unsafe fn protect_from_close(handle: HANDLE) -> io::Result<ProtectFromCloseReset> {
        if SetHandleInformation(
            handle,
            HANDLE_FLAG_PROTECT_FROM_CLOSE,
            HANDLE_FLAG_PROTECT_FROM_CLOSE,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(ProtectFromCloseReset(handle))
    }

    unsafe fn final_handle_path(handle: HANDLE) -> io::Result<String> {
        let mut buffer = vec![0u16; 32_768];
        let length = GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0);
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length as usize >= buffer.len() {
            return Err(io::Error::other("final handle path exceeded test buffer"));
        }
        buffer.truncate(length as usize);
        String::from_utf16(&buffer).map_err(io::Error::other)
    }

    unsafe fn current_token() -> Result<OwnedHandle> {
        Ok(owned_handle(get_current_token_for_restriction()?))
    }

    fn child_argv(test_name: &str) -> Result<Vec<String>> {
        Ok(vec![
            std::env::current_exe()?.to_string_lossy().into_owned(),
            "--exact".to_string(),
            test_name.to_string(),
            "--nocapture".to_string(),
        ])
    }

    fn current_environment() -> HashMap<String, String> {
        std::env::vars().collect()
    }

    unsafe fn wait_for_created_process(created: CreatedProcess) -> Result<u32> {
        let pi = created.process_info;
        let waited = WaitForSingleObject(pi.hProcess, INFINITE);
        if waited != 0 {
            let error = if waited == u32::MAX {
                io::Error::last_os_error()
            } else {
                io::Error::other(format!("unexpected child wait result 0x{waited:08x}"))
            };
            let _ = close_created_process_handles(&pi);
            return Err(error.into());
        }
        let mut exit_code = u32::MAX;
        if GetExitCodeProcess(pi.hProcess, &mut exit_code) == 0 {
            let error = io::Error::last_os_error();
            let _ = close_created_process_handles(&pi);
            return Err(error.into());
        }
        close_created_process_handles(&pi)?;
        Ok(exit_code)
    }

    unsafe fn create_test_process(
        token: HANDLE,
        test_name: &str,
        env: &HashMap<String, String>,
        stdio: Option<(HANDLE, HANDLE, HANDLE)>,
    ) -> Result<CreatedProcess> {
        create_process_as_user(
            token,
            &child_argv(test_name)?,
            &std::env::current_dir()?,
            env,
            None,
            stdio,
            ConsoleMode::NoWindow,
            false,
        )
    }

    #[test]
    fn create_process_restores_mixed_duplicate_handle_flags_on_success() -> Result<()> {
        unsafe {
            let (stdin_read, _stdin_write) = pipe()?;
            let (_output_read, output_write) = pipe()?;
            let stdin_handle = stdin_read.as_raw_handle() as HANDLE;
            let output_handle = output_write.as_raw_handle() as HANDLE;
            if SetHandleInformation(output_handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
                return Err(io::Error::last_os_error().into());
            }
            let _protect_from_close = protect_from_close(output_handle)?;
            let original_stdin_flags = flags(stdin_handle)?;
            let original_output_flags = flags(output_handle)?;
            assert_eq!(original_stdin_flags & HANDLE_FLAG_INHERIT, 0);
            assert_ne!(original_output_flags & HANDLE_FLAG_INHERIT, 0);
            assert_ne!(original_output_flags & HANDLE_FLAG_PROTECT_FROM_CLOSE, 0);

            let token = current_token()?;
            let created = create_test_process(
                token.as_raw_handle() as HANDLE,
                "process::tests::handle_probe_child",
                &current_environment(),
                Some((stdin_handle, output_handle, output_handle)),
            )?;
            assert_ne!(created.startup_info.dwFlags & STARTF_USESTDHANDLES, 0);
            assert_eq!(created.startup_info.hStdError, output_handle);
            assert_eq!(wait_for_created_process(created)?, 0);
            assert_eq!(flags(stdin_handle)?, original_stdin_flags);
            assert_eq!(flags(output_handle)?, original_output_flags);
        }
        Ok(())
    }

    #[test]
    fn create_process_restores_mixed_duplicate_handle_flags_on_failure() -> Result<()> {
        unsafe {
            let (stdin_read, _stdin_write) = pipe()?;
            let (_output_read, output_write) = pipe()?;
            let stdin_handle = stdin_read.as_raw_handle() as HANDLE;
            let output_handle = output_write.as_raw_handle() as HANDLE;
            if SetHandleInformation(output_handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
                return Err(io::Error::last_os_error().into());
            }
            let _protect_from_close = protect_from_close(output_handle)?;
            let original_stdin_flags = flags(stdin_handle)?;
            let original_output_flags = flags(output_handle)?;
            assert_eq!(original_stdin_flags & HANDLE_FLAG_PROTECT_FROM_CLOSE, 0);
            assert_ne!(original_output_flags & HANDLE_FLAG_PROTECT_FROM_CLOSE, 0);

            let error = create_test_process(
                INVALID_HANDLE_VALUE,
                "process::tests::handle_probe_child",
                &current_environment(),
                Some((stdin_handle, output_handle, output_handle)),
            )
            .err()
            .expect("an invalid token must fail process creation");
            assert!(
                error
                    .to_string()
                    .contains("Windows process creation failed")
            );
            assert_eq!(flags(stdin_handle)?, original_stdin_flags);
            assert_eq!(flags(output_handle)?, original_output_flags);
        }
        Ok(())
    }

    #[test]
    fn none_stdio_uses_valid_standard_handles_and_excludes_unlisted_handles() -> Result<()> {
        unsafe {
            let unlisted_file = tempfile::NamedTempFile::new()?;
            let unlisted_handle = unlisted_file.as_file().as_raw_handle() as HANDLE;
            if SetHandleInformation(unlisted_handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0
            {
                return Err(io::Error::last_os_error().into());
            }
            let original_unlisted_flags = flags(unlisted_handle)?;
            let unlisted_path = final_handle_path(unlisted_handle)?;
            let original_standard_flags = standard_handles()?
                .into_iter()
                .map(|handle| Ok((handle, flags(handle)?)))
                .collect::<io::Result<Vec<_>>>()?;
            let mut env = current_environment();
            env.insert(
                HANDLE_PROBE_ENV.to_string(),
                format!("{}|{unlisted_path}", unlisted_handle as usize),
            );

            let token = current_token()?;
            let created = create_test_process(
                token.as_raw_handle() as HANDLE,
                "process::tests::handle_probe_child",
                &env,
                None,
            )?;
            assert_ne!(created.startup_info.dwFlags & STARTF_USESTDHANDLES, 0);
            for handle in [
                created.startup_info.hStdInput,
                created.startup_info.hStdOutput,
                created.startup_info.hStdError,
            ] {
                assert!(!handle.is_null());
                assert_ne!(handle, INVALID_HANDLE_VALUE);
            }
            assert_eq!(wait_for_created_process(created)?, 0);
            assert_eq!(flags(unlisted_handle)?, original_unlisted_flags);
            for (handle, original_flags) in original_standard_flags {
                assert_eq!(flags(handle)?, original_flags);
            }
        }
        Ok(())
    }

    #[test]
    fn handle_probe_child() {
        let Some(raw_handle) = std::env::var_os(HANDLE_PROBE_ENV) else {
            return;
        };
        let encoded = raw_handle.to_string_lossy();
        let (raw_handle, expected_path) = encoded
            .split_once('|')
            .expect("probe must include the handle and expected file path");
        let handle = raw_handle
            .parse::<usize>()
            .expect("probe handle must be numeric") as HANDLE;
        let inherited_same_file = unsafe { final_handle_path(handle) }
            .is_ok_and(|actual_path| actual_path == expected_path);
        assert!(
            !inherited_same_file,
            "the explicit stdio handle list must exclude an unrelated inheritable file"
        );
    }

    #[test]
    fn restoration_cleanup_child() {
        let Some(marker) = std::env::var_os(CLEANUP_CHILD_ENV) else {
            return;
        };
        std::thread::sleep(Duration::from_secs(30));
        std::fs::write(marker, b"child escaped cleanup").expect("write cleanup failure marker");
    }

    #[test]
    fn post_create_restoration_failure_compromises_only_isolated_process() -> Result<()> {
        if std::env::var_os(COMPROMISE_HELPER_ENV).is_none() {
            let mut command = std::process::Command::new(std::env::current_exe()?);
            command
                .arg("--exact")
                .arg(
                    "process::tests::post_create_restoration_failure_compromises_only_isolated_process",
                )
                .arg("--nocapture")
                .env(COMPROMISE_HELPER_ENV, "1");
            let mut child = with_windows_child_creation(|_| command.spawn())?;
            let status = child.wait()?;
            assert!(
                status.success(),
                "isolated compromise helper failed: {status}"
            );
            return Ok(());
        }

        unsafe {
            let marker_dir = tempfile::tempdir()?;
            let marker = marker_dir.path().join("escaped.txt");
            let mut env = current_environment();
            env.insert(
                CLEANUP_CHILD_ENV.to_string(),
                marker.to_string_lossy().into_owned(),
            );
            let token = current_token()?;
            INJECT_POST_CREATE_RESTORATION_FAILURE.store(true, std::sync::atomic::Ordering::SeqCst);
            let started = Instant::now();
            let error = create_test_process(
                token.as_raw_handle() as HANDLE,
                "process::tests::restoration_cleanup_child",
                &env,
                None,
            )
            .err()
            .expect("injected restoration failure must reject the created child");
            let error_chain = format!("{error:#}");
            assert!(
                error_chain.contains("injected post-create"),
                "unexpected restoration error: {error:#}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "cleanup did not promptly terminate and reap the child"
            );
            std::thread::sleep(Duration::from_millis(100));
            assert!(!marker.exists(), "the rejected child escaped Job cleanup");

            for _ in 0..2 {
                let compromised = with_windows_child_creation(|_| Ok(()))
                    .expect_err("compromise latch must permanently fail closed");
                assert!(compromised.to_string().contains("compromised"));
            }
        }
        Ok(())
    }
}
