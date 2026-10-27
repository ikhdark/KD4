use crate::winutil::to_wide;
use anyhow::Result;
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

const SETUP_MUTEX_NAME: &str = "Global\\CodexSandboxSetup";

pub struct SetupMutexGuard {
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

pub fn acquire_setup_mutex() -> Result<SetupMutexGuard> {
    acquire_named_setup_mutex(SETUP_MUTEX_NAME, INFINITE)
}

pub(crate) fn acquire_setup_mutex_for_launch() -> Result<SetupMutexGuard> {
    acquire_named_setup_mutex(SETUP_MUTEX_NAME, 5_000)
}

fn acquire_named_setup_mutex(name: &str, timeout_ms: u32) -> Result<SetupMutexGuard> {
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
    let wait_result = unsafe { WaitForSingleObject(handle, timeout_ms) };
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
