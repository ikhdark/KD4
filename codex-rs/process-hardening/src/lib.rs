#[cfg(unix)]
use std::ffi::OsString;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

/// This is designed to be called pre-main() (using `#[ctor::ctor]`) to perform
/// various process hardening steps, such as
/// - disabling core dumps
/// - disabling ptrace attach on Linux and macOS.
/// - removing dangerous environment variables such as LD_PRELOAD and DYLD_*
///
/// # Safety
/// On Unix, the caller must exclude concurrent environment access (including
/// native readers). Call during single-threaded startup, after any dotenv
/// loading and before creating runtime threads.
pub unsafe fn pre_main_hardening() {
    harden_process();

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd"
    ))]
    // SAFETY: The caller guarantees exclusive access to the environment.
    unsafe {
        remove_env_vars_with_prefix(b"LD_");
    }

    #[cfg(target_os = "macos")]
    // SAFETY: The caller guarantees exclusive access to the environment.
    unsafe {
        remove_env_vars_with_prefix(b"DYLD_");
    }
}

/// Applies native process policy without mutating the environment.
pub fn harden_process() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pre_main_hardening_linux();

    #[cfg(target_os = "macos")]
    pre_main_hardening_macos();

    // On FreeBSD and OpenBSD, apply similar hardening to Linux/macOS:
    #[cfg(any(target_os = "freebsd", target_os = "openbsd"))]
    pre_main_hardening_bsd();

    #[cfg(windows)]
    pre_main_hardening_windows();
}

/// Keeps latency-sensitive runtime work independent of Windows' automatic
/// foreground/background CPU QoS classification. Call before starting workers.
/// This does not raise scheduling priority or change timer-resolution policy.
/// Other platforms are unchanged.
pub fn disable_execution_speed_throttling() -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        use windows_sys::Win32::System::Threading::GetProcessInformation;
        use windows_sys::Win32::System::Threading::PROCESS_POWER_THROTTLING_CURRENT_VERSION;
        use windows_sys::Win32::System::Threading::PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
        use windows_sys::Win32::System::Threading::PROCESS_POWER_THROTTLING_STATE;
        use windows_sys::Win32::System::Threading::ProcessPowerThrottling;
        use windows_sys::Win32::System::Threading::SetProcessInformation;

        let mut state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: 0,
            StateMask: 0,
        };
        let size = std::mem::size_of_val(&state) as u32;
        // SAFETY: The pseudo-handle refers to this process; state is writable,
        // correctly sized, and remains alive for the synchronous call.
        let process = unsafe { GetCurrentProcess() };
        if unsafe {
            GetProcessInformation(
                process,
                ProcessPowerThrottling,
                std::ptr::from_mut(&mut state).cast(),
                size,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        // Preserve all other mechanisms, including explicit timer policy.
        // A zero ControlMask would restore heuristics rather than opt out.
        state.ControlMask |= PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
        state.StateMask &= !PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
        // SAFETY: The same process handle and fully initialized state remain
        // valid; SetProcessInformation only reads this structure.
        if unsafe {
            SetProcessInformation(
                process,
                ProcessPowerThrottling,
                std::ptr::from_ref(&state).cast(),
                size,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const PRCTL_FAILED_EXIT_CODE: i32 = 5;

#[cfg(target_os = "macos")]
const PTRACE_DENY_ATTACH_FAILED_EXIT_CODE: i32 = 6;

#[cfg(unix)]
const SET_RLIMIT_CORE_FAILED_EXIT_CODE: i32 = 7;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn pre_main_hardening_linux() {
    // Disable ptrace attach / mark process non-dumpable.
    let ret_code = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    if ret_code != 0 {
        fatal_hardening_error("prctl(PR_SET_DUMPABLE, 0)", PRCTL_FAILED_EXIT_CODE);
    }

    // For "defense in depth," set the core file size limit to 0.
    set_core_file_size_limit_to_zero();
}

/// Mark the current Linux process non-dumpable so same-user processes cannot attach with ptrace.
#[cfg(target_os = "linux")]
pub fn disable_process_dumping() -> std::io::Result<()> {
    let ret_code = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    if ret_code == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "freebsd", target_os = "openbsd"))]
pub(crate) fn pre_main_hardening_bsd() {
    // FreeBSD/OpenBSD: set RLIMIT_CORE to 0.
    set_core_file_size_limit_to_zero();
}

#[cfg(target_os = "macos")]
pub(crate) fn pre_main_hardening_macos() {
    // Prevent debuggers from attaching to this process.
    let ret_code = unsafe { libc::ptrace(libc::PT_DENY_ATTACH, 0, std::ptr::null_mut(), 0) };
    if ret_code == -1 {
        fatal_hardening_error(
            "ptrace(PT_DENY_ATTACH)",
            PTRACE_DENY_ATTACH_FAILED_EXIT_CODE,
        );
    }

    // Set the core file size limit to 0 to prevent core dumps.
    set_core_file_size_limit_to_zero();
}

#[cfg(unix)]
fn set_core_file_size_limit_to_zero() {
    let rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    let ret_code = unsafe { libc::setrlimit(libc::RLIMIT_CORE, &rlim) };
    if ret_code != 0 {
        fatal_hardening_error("setrlimit(RLIMIT_CORE)", SET_RLIMIT_CORE_FAILED_EXIT_CODE);
    }
}

#[cfg(unix)]
fn fatal_hardening_error(_operation: &str, exit_code: i32) -> ! {
    // This can run before Rust initializes SIGPIPE. Diagnostic I/O can block
    // indefinitely or replace our reserved exit code with a signal status.
    // SAFETY: _exit terminates without invoking cleanup or flushing any streams.
    unsafe { libc::_exit(exit_code) }
}

#[cfg(windows)]
pub(crate) fn pre_main_hardening_windows() {
    // TODO(mbolin): Perform the appropriate configuration for Windows.
}

#[cfg(unix)]
unsafe fn remove_env_vars_with_prefix(prefix: &[u8]) {
    for key in env_keys_with_prefix(std::env::vars_os(), prefix) {
        // SAFETY: The startup caller excludes concurrent environment access.
        unsafe {
            std::env::remove_var(key);
        }
    }
}

#[cfg(unix)]
fn env_keys_with_prefix<I>(vars: I, prefix: &[u8]) -> Vec<OsString>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    vars.into_iter()
        .filter_map(|(key, _)| {
            key.as_os_str()
                .as_bytes()
                .starts_with(prefix)
                .then_some(key)
        })
        .collect()
}

