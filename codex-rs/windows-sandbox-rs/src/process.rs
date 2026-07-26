use crate::desktop::LaunchDesktop;
use crate::logging;
use crate::proc_thread_attr::ProcThreadAttributeList;
use crate::winutil::argv_to_command_line;
use crate::winutil::format_last_error;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::ptr;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::SetHandleInformation;
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Console::GetStdHandle;
use windows_sys::Win32::System::Console::STD_ERROR_HANDLE;
use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;
use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::JobObjects::TerminateJobObject;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
use windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT;
use windows_sys::Win32::System::Threading::CreateProcessAsUserW;
use windows_sys::Win32::System::Threading::EXTENDED_STARTUPINFO_PRESENT;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::ResumeThread;
use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
use windows_sys::Win32::System::Threading::STARTUPINFOEXW;
use windows_sys::Win32::System::Threading::STARTUPINFOW;
use windows_sys::Win32::System::Threading::TerminateProcess;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

pub(crate) unsafe fn attach_to_job_and_resume(
    process_info: &PROCESS_INFORMATION,
    containment_job: Option<HANDLE>,
) -> Result<()> {
    let Some(job) = containment_job else {
        return Ok(());
    };
    if AssignProcessToJobObject(job, process_info.hProcess) == 0 {
        let err = GetLastError();
        let _ = TerminateProcess(process_info.hProcess, 1);
        let _ = WaitForSingleObject(process_info.hProcess, 5_000);
        if process_info.hThread != 0 {
            CloseHandle(process_info.hThread);
        }
        if process_info.hProcess != 0 {
            CloseHandle(process_info.hProcess);
        }
        return Err(anyhow!(
            "AssignProcessToJobObject failed before child resume: {err}"
        ));
    }
    if ResumeThread(process_info.hThread) == u32::MAX {
        let err = GetLastError();
        let _ = TerminateJobObject(job, 1);
        let _ = TerminateProcess(process_info.hProcess, 1);
        let _ = WaitForSingleObject(process_info.hProcess, 5_000);
        if process_info.hThread != 0 {
            CloseHandle(process_info.hThread);
        }
        if process_info.hProcess != 0 {
            CloseHandle(process_info.hProcess);
        }
        return Err(anyhow!("ResumeThread failed for contained child: {err}"));
    }
    Ok(())
}

pub struct CreatedProcess {
    pub process_info: PROCESS_INFORMATION,
    pub startup_info: STARTUPINFOW,
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

unsafe fn ensure_inheritable_stdio(si: &mut STARTUPINFOW) -> Result<()> {
    for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let h = GetStdHandle(kind);
        if h == 0 || h == INVALID_HANDLE_VALUE {
            return Err(anyhow!("GetStdHandle failed: {}", GetLastError()));
        }
        if SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
            return Err(anyhow!("SetHandleInformation failed: {}", GetLastError()));
        }
    }
    si.dwFlags |= STARTF_USESTDHANDLES;
    si.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
    si.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
    si.hStdError = GetStdHandle(STD_ERROR_HANDLE);
    Ok(())
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
    containment_job: Option<HANDLE>,
) -> Result<CreatedProcess> {
    let cmdline_str = argv_to_command_line(argv);
    let mut cmdline: Vec<u16> = to_wide(&cmdline_str);
    let env_block = make_env_block(env_map);
    let desktop = LaunchDesktop::prepare(use_private_desktop, logs_base_dir)?;
    let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
    let cwd_wide = to_wide(cwd);
    let env_block_len = env_block.len();
    match stdio {
        Some((stdin_h, stdout_h, stderr_h)) => {
            let mut si: STARTUPINFOEXW = std::mem::zeroed();
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            // Some processes (e.g., PowerShell) can fail with STATUS_DLL_INIT_FAILED
            // if lpDesktop is not set when launching with a restricted token.
            // Point explicitly at the interactive desktop or a private desktop.
            si.StartupInfo.lpDesktop = desktop.startup_info_desktop();
            si.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = stdin_h;
            si.StartupInfo.hStdOutput = stdout_h;
            si.StartupInfo.hStdError = stderr_h;
            let mut inherited_handles = vec![stdin_h, stdout_h];
            if !inherited_handles.contains(&stderr_h) {
                inherited_handles.push(stderr_h);
            }
            for &handle in &inherited_handles {
                if SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
                    return Err(anyhow!(
                        "SetHandleInformation failed for stdio handle: {}",
                        GetLastError()
                    ));
                }
            }
            let mut attrs = ProcThreadAttributeList::new(/*attr_count*/ 1)?;
            attrs.set_handle_list(inherited_handles)?;
            si.lpAttributeList = attrs.as_mut_ptr();

            let creation_flags = CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT
                | if containment_job.is_some() {
                    CREATE_SUSPENDED
                } else {
                    0
                }
                | match console_mode {
                    ConsoleMode::Inherit => 0,
                    ConsoleMode::NoWindow => CREATE_NO_WINDOW,
                };
            let ok = CreateProcessAsUserW(
                h_token,
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                1,
                creation_flags,
                env_block.as_ptr() as *mut c_void,
                cwd_wide.as_ptr(),
                &si.StartupInfo,
                &mut pi,
            );
            if ok == 0 {
                let err = GetLastError() as i32;
                let msg = format!(
                    "CreateProcessAsUserW failed: {} ({}) | cwd={} | cmd={} | env_u16_len={} | si_flags={} | creation_flags={}",
                    err,
                    format_last_error(err),
                    cwd.display(),
                    cmdline_str,
                    env_block_len,
                    si.StartupInfo.dwFlags,
                    creation_flags,
                );
                logging::debug_log(&msg, logs_base_dir);
                return Err(std::io::Error::from_raw_os_error(err)).context(msg);
            }
            attach_to_job_and_resume(&pi, containment_job)?;
            Ok(CreatedProcess {
                process_info: pi,
                startup_info: si.StartupInfo,
                _desktop: desktop,
            })
        }
        None => {
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            si.lpDesktop = desktop.startup_info_desktop();
            ensure_inheritable_stdio(&mut si)?;

            let creation_flags = CREATE_UNICODE_ENVIRONMENT
                | if containment_job.is_some() {
                    CREATE_SUSPENDED
                } else {
                    0
                };
            let ok = CreateProcessAsUserW(
                h_token,
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                1,
                creation_flags,
                env_block.as_ptr() as *mut c_void,
                cwd_wide.as_ptr(),
                &si,
                &mut pi,
            );
            if ok == 0 {
                let err = GetLastError() as i32;
                let msg = format!(
                    "CreateProcessAsUserW failed: {} ({}) | cwd={} | cmd={} | env_u16_len={} | si_flags={} | creation_flags={}",
                    err,
                    format_last_error(err),
                    cwd.display(),
                    cmdline_str,
                    env_block_len,
                    si.dwFlags,
                    creation_flags,
                );
                logging::debug_log(&msg, logs_base_dir);
                return Err(std::io::Error::from_raw_os_error(err)).context(msg);
            }
            attach_to_job_and_resume(&pi, containment_job)?;
            Ok(CreatedProcess {
                process_info: pi,
                startup_info: si,
                _desktop: desktop,
            })
        }
    }
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
    pub stdin_write: Option<HANDLE>,
    pub stdout_read: HANDLE,
    pub stderr_read: Option<HANDLE>,
    pub(crate) desktop: LaunchDesktop,
}

/// Spawns a process with anonymous pipes and returns the relevant handles.
#[allow(clippy::too_many_arguments)]
pub fn spawn_process_with_pipes(
    h_token: HANDLE,
    argv: &[String],
    cwd: &Path,
    env_map: &HashMap<String, String>,
    stdin_mode: StdinMode,
    stderr_mode: StderrMode,
    console_mode: ConsoleMode,
    use_private_desktop: bool,
    logs_base_dir: Option<&Path>,
    containment_job: Option<HANDLE>,
) -> Result<PipeSpawnHandles> {
    let mut in_r: HANDLE = 0;
    let mut in_w: HANDLE = 0;
    let mut out_r: HANDLE = 0;
    let mut out_w: HANDLE = 0;
    let mut err_r: HANDLE = 0;
    let mut err_w: HANDLE = 0;
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
            containment_job,
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
    std::thread::spawn(move || {
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
    use crate::winutil::argv_to_command_line;
    use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
    use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    use windows_sys::Win32::System::JobObjects::JOBOBJECT_BASIC_ACCOUNTING_INFORMATION;
    use windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
    use windows_sys::Win32::System::JobObjects::JobObjectBasicAccountingInformation;
    use windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation;
    use windows_sys::Win32::System::JobObjects::QueryInformationJobObject;
    use windows_sys::Win32::System::JobObjects::SetInformationJobObject;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    use windows_sys::Win32::System::Threading::CreateProcessW;

    unsafe fn create_kill_on_close_job() -> HANDLE {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        assert_ne!(job, 0, "CreateJobObjectW failed: {}", GetLastError());
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        assert_ne!(
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            ),
            0,
            "SetInformationJobObject failed: {}",
            GetLastError()
        );
        job
    }

    unsafe fn job_accounting(job: HANDLE) -> JOBOBJECT_BASIC_ACCOUNTING_INFORMATION {
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
        assert_ne!(
            QueryInformationJobObject(
                job,
                JobObjectBasicAccountingInformation,
                &mut accounting as *mut _ as *mut _,
                std::mem::size_of_val(&accounting) as u32,
                std::ptr::null_mut(),
            ),
            0,
            "QueryInformationJobObject failed: {}",
            GetLastError()
        );
        accounting
    }

    #[test]
    fn suspended_job_assignment_contains_detached_descendants() {
        let temp = tempfile::tempdir().expect("tempdir");
        let child_script = temp.path().join("detached-child.ps1");
        let parent_script = temp.path().join("parent.ps1");
        let escaped_marker = temp
            .path()
            .join("escaped.txt")
            .to_string_lossy()
            .replace('\'', "''");
        std::fs::write(
            &child_script,
            format!(
                "Start-Sleep -Milliseconds 750\n[IO.File]::WriteAllText('{escaped_marker}', 'escaped')\n"
            ),
        )
        .expect("write child script");
        let escaped_child = child_script.to_string_lossy().replace('\'', "''");
        std::fs::write(
            &parent_script,
            format!(
                "Start-Process -FilePath powershell.exe -WindowStyle Hidden -ArgumentList @('-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File','{escaped_child}')\n"
            ),
        )
        .expect("write parent script");

        let argv = vec![
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-File".to_string(),
            parent_script.to_string_lossy().into_owned(),
        ];
        let mut command_line = to_wide(argv_to_command_line(&argv));
        let cwd = to_wide(temp.path());
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let job = unsafe { create_kill_on_close_job() };
        let created = unsafe {
            CreateProcessW(
                std::ptr::null(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_NO_WINDOW | CREATE_SUSPENDED,
                std::ptr::null(),
                cwd.as_ptr(),
                &startup,
                &mut process_info,
            )
        };
        assert_ne!(created, 0, "CreateProcessW failed: {}", unsafe {
            GetLastError()
        });
        unsafe {
            attach_to_job_and_resume(&process_info, Some(job))
                .expect("assign suspended child to job and resume");
        }
        assert_eq!(
            unsafe { WaitForSingleObject(process_info.hProcess, 10_000) },
            0,
            "parent process did not exit"
        );
        let accounting = unsafe { job_accounting(job) };
        assert!(
            accounting.TotalProcesses >= 2 && accounting.ActiveProcesses >= 1,
            "expected a live detached descendant in the job (total={}, active={})",
            accounting.TotalProcesses,
            accounting.ActiveProcesses,
        );

        assert_ne!(
            unsafe { TerminateJobObject(job, 1) },
            0,
            "TerminateJobObject failed: {}",
            unsafe { GetLastError() }
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while unsafe { job_accounting(job).ActiveProcesses } != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "contained descendants did not terminate"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        unsafe {
            CloseHandle(process_info.hThread);
            CloseHandle(process_info.hProcess);
            CloseHandle(job);
        }
        std::thread::sleep(std::time::Duration::from_millis(900));
        assert!(
            !temp.path().join("escaped.txt").exists(),
            "detached descendant outlived the Job Object"
        );
    }
}
