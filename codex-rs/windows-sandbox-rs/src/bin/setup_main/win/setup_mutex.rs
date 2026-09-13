use anyhow::Result;
use codex_windows_sandbox::to_wide;
use std::ffi::OsStr;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::WAIT_ABANDONED;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::ReleaseMutex;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const SETUP_MUTEX_NAME: &str = "Local\\CodexSandboxSetup";

pub(super) struct SetupMutexGuard {
    handle: HANDLE,
}

impl Drop for SetupMutexGuard {
    fn drop(&mut self) {
        // SAFETY: This guard owns the mutex handle acquired by a successful wait; its handle is
        // closed exactly once on drop.
        unsafe {
            let _ = ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

pub(super) fn acquire_setup_mutex() -> Result<SetupMutexGuard> {
    acquire_named_setup_mutex(SETUP_MUTEX_NAME)
}

fn acquire_named_setup_mutex(name: &str) -> Result<SetupMutexGuard> {
    let name = to_wide(OsStr::new(name));
    // SAFETY: name is a terminated UTF-16 buffer retained for the call, and the optional
    // security attributes are null.
    let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(anyhow::anyhow!("CreateMutexW failed: {}", unsafe {
            GetLastError()
        }));
    }

    // SAFETY: handle is the live mutex handle returned by CreateMutexW and is not closed while
    // the wait is in progress.
    let wait_result = unsafe { WaitForSingleObject(handle, INFINITE) };
    if wait_result != WAIT_OBJECT_0 && wait_result != WAIT_ABANDONED {
        // SAFETY: GetLastError reads only this thread's error code immediately after the
        // unsuccessful wait.
        let err = unsafe { GetLastError() };
        // SAFETY: The failed wait did not transfer handle ownership to a guard, so this closes
        // the newly created handle once.
        unsafe {
            CloseHandle(handle);
        }
        return Err(anyhow::anyhow!(
            "WaitForSingleObject for setup mutex failed: result={wait_result}, error={err}"
        ));
    }

    Ok(SetupMutexGuard { handle })
}

#[cfg(test)]
#[path = "setup_mutex_tests.rs"]
mod tests;
