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
    let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(anyhow::anyhow!("CreateMutexW failed: {}", unsafe {
            GetLastError()
        }));
    }

    let wait_result = unsafe { WaitForSingleObject(handle, INFINITE) };
    if wait_result != WAIT_OBJECT_0 && wait_result != WAIT_ABANDONED {
        let err = unsafe { GetLastError() };
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
