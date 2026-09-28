use anyhow::Result;
use codex_windows_sandbox::to_wide;
use std::ffi::OsStr;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Threading::MUTEX_ALL_ACCESS;
use windows_sys::Win32::System::Threading::OpenMutexW;
use windows_sys::Win32::System::Threading::ReleaseMutex;

const READ_ACL_MUTEX_NAME: &str = "Local\\CodexSandboxReadAcl";

pub(super) struct ReadAclMutexGuard {
    handle: HANDLE,
}

impl Drop for ReadAclMutexGuard {
    fn drop(&mut self) {
        // SAFETY: This guard owns the mutex handle returned by creation; no handle ownership is
        // transferred before Drop closes it.
        unsafe {
            let _ = ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

pub(super) fn read_acl_mutex_exists() -> Result<bool> {
    let name = to_wide(OsStr::new(READ_ACL_MUTEX_NAME));
    // SAFETY: name is a terminated UTF-16 buffer kept alive for the call; OpenMutexW returns a
    // separately owned handle on success.
    let handle = unsafe { OpenMutexW(MUTEX_ALL_ACCESS, 0, name.as_ptr()) };
    if handle.is_null() {
        // SAFETY: GetLastError reads only this thread's error code immediately after OpenMutexW
        // failed.
        let err = unsafe { GetLastError() };
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        return Err(anyhow::anyhow!("OpenMutexW failed: {err}"));
    }
    // SAFETY: OpenMutexW returned this new handle; it is closed here without having been
    // transferred to another owner.
    unsafe {
        CloseHandle(handle);
    }
    Ok(true)
}

pub(super) fn acquire_read_acl_mutex() -> Result<Option<ReadAclMutexGuard>> {
    let name = to_wide(OsStr::new(READ_ACL_MUTEX_NAME));
    // SAFETY: name is a terminated UTF-16 buffer; the null security attributes request defaults
    // and initial ownership is handled below.
    let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 1, name.as_ptr()) };
    if handle.is_null() {
        return Err(anyhow::anyhow!("CreateMutexW failed: {}", unsafe {
            GetLastError()
        }));
    }
    // SAFETY: GetLastError reads only the current thread's error code immediately after
    // CreateMutexW.
    let err = unsafe { GetLastError() };
    if err == ERROR_ALREADY_EXISTS {
        // SAFETY: This newly opened existing-mutex handle was not transferred to a guard and is
        // closed exactly once.
        unsafe {
            CloseHandle(handle);
        }
        return Ok(None);
    }
    Ok(Some(ReadAclMutexGuard { handle }))
}
