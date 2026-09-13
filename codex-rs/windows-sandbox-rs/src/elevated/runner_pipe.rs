//! Named pipe helpers for the elevated Windows sandbox runner.
//!
//! This module generates paired pipe names, creates server‑side pipes with
//! sandbox-user-scoped ACLs, and waits for the runner to connect. It is
//! **elevated-path only** and is
//! used by the parent to establish the IPC channel for both unified_exec sessions
//! and elevated capture. The legacy restricted‑token path spawns the child directly
//! and does not use these helpers.

use crate::helper_materialization::HelperExecutable;
use crate::helper_materialization::resolve_helper_for_launch;
use crate::winutil::resolve_sid;
use crate::winutil::string_from_sid_bytes;
use crate::winutil::to_wide;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::SmallRng;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
use windows_sys::Win32::System::Pipes::CreateNamedPipeW;
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::Pipes::PIPE_READMODE_BYTE;
use windows_sys::Win32::System::Pipes::PIPE_TYPE_BYTE;
use windows_sys::Win32::System::Pipes::PIPE_WAIT;

/// PIPE_ACCESS_INBOUND (win32 constant), not exposed in windows-sys 0.52.
pub const PIPE_ACCESS_INBOUND: u32 = 0x0000_0001;
/// PIPE_ACCESS_OUTBOUND (win32 constant), not exposed in windows-sys 0.52.
pub const PIPE_ACCESS_OUTBOUND: u32 = 0x0000_0002;

/// Resolves the elevated command runner path, preferring the copied helper under
/// `.sandbox-bin` and falling back to the legacy sibling lookup when needed.
pub fn find_runner_exe(codex_home: &Path, log_dir: Option<&Path>) -> PathBuf {
    resolve_helper_for_launch(HelperExecutable::CommandRunner, codex_home, log_dir)
}

/// Generates a unique named-pipe path used to communicate with the runner process.
pub fn pipe_pair() -> (String, String) {
    let mut rng = SmallRng::from_os_rng();
    let nonce: u128 = rng.random();
    let base = format!(r"\\.\pipe\codex-runner-{nonce:x}");
    (format!("{base}-in"), format!("{base}-out"))
}

/// Creates a named pipe whose DACL only allows the sandbox user to connect.
pub fn create_named_pipe(name: &str, access: u32, sandbox_username: &str) -> io::Result<HANDLE> {
    let sandbox_sid = resolve_sid(sandbox_username)
        .map_err(|err| io::Error::new(io::ErrorKind::PermissionDenied, err.to_string()))?;
    let sandbox_sid = string_from_sid_bytes(&sandbox_sid)
        .map_err(|err| io::Error::new(io::ErrorKind::PermissionDenied, err))?;
    let sddl = to_wide(format!("D:(A;;GA;;;{sandbox_sid})"));
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: sddl is a terminated UTF-16 buffer and sd is writable pointer storage; a
    // successful conversion transfers its allocation to this function.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1, // SDDL_REVISION_1
            &mut sd,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        // SAFETY: GetLastError reads the current thread's native error immediately after
        // descriptor conversion failed.
        return Err(io::Error::from_raw_os_error(unsafe {
            GetLastError() as i32
        }));
    }
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let wide = to_wide(name);
    // SAFETY: wide is terminated and sa points to the live converted security descriptor;
    // Windows copies the descriptor during pipe creation.
    let h = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            access,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            65536,
            65536,
            0,
            &mut sa as *mut SECURITY_ATTRIBUTES,
        )
    };
    let result = if h.is_null() || h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(h)
    };
    // SAFETY: sd is the LocalAlloc-backed descriptor returned by successful conversion;
    // CreateNamedPipeW no longer needs it after returning.
    unsafe {
        LocalFree(sd as HLOCAL);
    }
    result
}

/// Waits for the runner to connect to a parent-created server pipe.
///
/// This is parent-side only: the runner opens the pipe with `CreateFileW`, while the
/// parent calls `ConnectNamedPipe`, tolerates the already-connected case, and
/// verifies that the connected client is the runner process we just spawned.
pub fn connect_pipe(h: HANDLE, expected_runner_pid: u32) -> io::Result<()> {
    // SAFETY: The caller keeps its server pipe handle open through the connection attempt. It
    // was opened for synchronous I/O, so no OVERLAPPED pointer is required.
    let ok = unsafe { ConnectNamedPipe(h, ptr::null_mut()) };
    if ok == 0 {
        // SAFETY: Reading the thread-local last-error code requires no pointers and immediately
        // follows the failed ConnectNamedPipe call.
        let err = unsafe { GetLastError() };
        const ERROR_PIPE_CONNECTED: u32 = 535;
        if err != ERROR_PIPE_CONNECTED {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
    }
    let mut client_pid = 0;
    // SAFETY: The caller keeps the pipe handle live, and client_pid is valid writable storage
    // for the returned process ID.
    let ok = unsafe { GetNamedPipeClientProcessId(h, &mut client_pid) };
    if ok == 0 {
        // SAFETY: Reading the thread-local last-error code requires no pointers and immediately
        // follows the failed GetNamedPipeClientProcessId call.
        return Err(io::Error::from_raw_os_error(unsafe {
            GetLastError() as i32
        }));
    }
    if client_pid != expected_runner_pid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "named pipe client pid {client_pid} did not match runner pid {expected_runner_pid}"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn invalid_named_pipe_name_preserves_the_creation_error() {
        let error = super::create_named_pipe(
            "not-a-named-pipe-path",
            super::PIPE_ACCESS_INBOUND,
            "Everyone",
        )
        .expect_err("invalid pipe name must fail");
        assert_eq!(
            error.raw_os_error(),
            Some(windows_sys::Win32::Foundation::ERROR_INVALID_NAME as i32)
        );
    }
}
