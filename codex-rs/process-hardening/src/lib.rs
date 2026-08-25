use std::ffi::c_void;
use std::io;
use std::mem::size_of_val;

use windows_sys::Win32::System::SystemServices::PROCESS_MITIGATION_DEP_POLICY;
use windows_sys::Win32::System::SystemServices::PROCESS_MITIGATION_DEP_POLICY_0;
use windows_sys::Win32::System::SystemServices::PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY;
use windows_sys::Win32::System::SystemServices::PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY_0;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::GetProcessMitigationPolicy;
use windows_sys::Win32::System::Threading::PROCESS_DEP_ENABLE;
use windows_sys::Win32::System::Threading::ProcessDEPPolicy;
use windows_sys::Win32::System::Threading::ProcessExtensionPointDisablePolicy;
use windows_sys::Win32::System::Threading::SetProcessMitigationPolicy;

const DEP_POLICY_FLAGS: u32 = PROCESS_DEP_ENABLE;
const EXTENSION_POINT_DISABLE_POLICY_FLAGS: u32 = 1;

/// This is designed to be called pre-main() (using `#[ctor::ctor]`) to perform
/// Windows process hardening steps.
pub fn pre_main_hardening() {
    if let Err(error) = pre_main_hardening_windows() {
        eprintln!("failed to apply Windows process hardening: {error}");
        std::process::exit(1);
    }
}

pub(crate) fn pre_main_hardening_windows() -> io::Result<()> {
    configure_windows_hardening(enable_dep, disable_extension_points)
}

fn configure_windows_hardening(
    mut enable_dep: impl FnMut(u32) -> io::Result<()>,
    mut disable_extension_points: impl FnMut(u32) -> io::Result<()>,
) -> io::Result<()> {
    enable_dep(DEP_POLICY_FLAGS)?;
    disable_extension_points(EXTENSION_POINT_DISABLE_POLICY_FLAGS)
}

fn enable_dep(flags: u32) -> io::Result<()> {
    let mut current = PROCESS_MITIGATION_DEP_POLICY::default();
    // SAFETY: `current` is a writable buffer of the structure required for this
    // policy kind, and GetProcessMitigationPolicy does not retain it.
    if unsafe {
        GetProcessMitigationPolicy(
            GetCurrentProcess(),
            ProcessDEPPolicy,
            &raw mut current as *mut c_void,
            size_of_val(&current),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful query initialized the union's `Flags` field.
    if unsafe { current.Anonymous.Flags } & flags == flags && current.Permanent {
        return Ok(());
    }

    let policy = PROCESS_MITIGATION_DEP_POLICY {
        Anonymous: PROCESS_MITIGATION_DEP_POLICY_0 { Flags: flags },
        Permanent: true,
    };
    // SAFETY: `policy` has the structure and lifetime required for this policy kind,
    // and SetProcessMitigationPolicy does not retain the buffer.
    if unsafe {
        SetProcessMitigationPolicy(
            ProcessDEPPolicy,
            &raw const policy as *const c_void,
            size_of_val(&policy),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn disable_extension_points(flags: u32) -> io::Result<()> {
    let mut current = PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY::default();
    // SAFETY: `current` is a writable buffer of the structure required for this
    // policy kind, and GetProcessMitigationPolicy does not retain it.
    if unsafe {
        GetProcessMitigationPolicy(
            GetCurrentProcess(),
            ProcessExtensionPointDisablePolicy,
            &raw mut current as *mut c_void,
            size_of_val(&current),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful query initialized the union's `Flags` field.
    if unsafe { current.Anonymous.Flags } & flags == flags {
        return Ok(());
    }

    let policy = PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY {
        Anonymous: PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY_0 { Flags: flags },
    };
    // SAFETY: `policy` has the structure and lifetime required for this policy kind,
    // and SetProcessMitigationPolicy does not retain the buffer.
    if unsafe {
        SetProcessMitigationPolicy(
            ProcessExtensionPointDisablePolicy,
            &raw const policy as *const c_void,
            size_of_val(&policy),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn hardening_enables_dep_before_disabling_extension_points() {
        let calls = RefCell::new(Vec::new());

        configure_windows_hardening(
            |flags| {
                calls.borrow_mut().push(("dep", flags));
                Ok(())
            },
            |flags| {
                calls.borrow_mut().push(("extension-points", flags));
                Ok(())
            },
        )
        .expect("hardening succeeds");

        assert_eq!(
            calls.into_inner(),
            vec![
                ("dep", DEP_POLICY_FLAGS),
                ("extension-points", EXTENSION_POINT_DISABLE_POLICY_FLAGS),
            ]
        );
    }

    #[test]
    fn hardening_stops_after_the_first_failed_mitigation() {
        let extension_points_called = Cell::new(false);

        let error = configure_windows_hardening(
            |_| Err(io::Error::other("DEP unavailable")),
            |_| {
                extension_points_called.set(true);
                Ok(())
            },
        )
        .expect_err("a mitigation failure must fail hardening");

        assert_eq!(error.to_string(), "DEP unavailable");
        assert!(!extension_points_called.get());
    }

    #[test]
    fn hardening_applies_to_the_current_process_idempotently() {
        pre_main_hardening_windows().expect("apply Windows hardening");
        pre_main_hardening_windows().expect("already-applied hardening remains successful");
    }
}
