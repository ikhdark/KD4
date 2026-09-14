use std::ffi::c_void;
use std::io;
use std::mem::size_of_val;

use windows_sys::Win32::System::SystemServices::PROCESS_MITIGATION_DEP_POLICY;
use windows_sys::Win32::System::SystemServices::PROCESS_MITIGATION_DEP_POLICY_0;
use windows_sys::Win32::System::SystemServices::PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY;
use windows_sys::Win32::System::SystemServices::PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY_0;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::GetProcessMitigationPolicy;
use windows_sys::Win32::System::Threading::PROCESS_DEP_DISABLE_ATL_THUNK_EMULATION;
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
    let current_flags = unsafe { current.Anonymous.Flags };
    if current_flags & flags == flags && current.Permanent {
        return Ok(());
    }

    let policy = hardened_dep_policy(current_flags, flags);
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

fn hardened_dep_policy(current_flags: u32, flags: u32) -> PROCESS_MITIGATION_DEP_POLICY {
    // Preserve existing documented settings without copying reserved bits.
    let known_flags = PROCESS_DEP_ENABLE | PROCESS_DEP_DISABLE_ATL_THUNK_EMULATION;
    PROCESS_MITIGATION_DEP_POLICY {
        Anonymous: PROCESS_MITIGATION_DEP_POLICY_0 {
            Flags: (current_flags & known_flags) | flags,
        },
        Permanent: true,
    }
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
            vec![("dep", 1), ("extension-points", 1)]
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
    fn dep_policy_preserves_documented_settings_and_drops_reserved_bits() {
        for (current, expected) in [(0, 1), (1, 1), (2, 3), (3, 3), (u32::MAX, 3)] {
            let policy = hardened_dep_policy(current, DEP_POLICY_FLAGS);
            // SAFETY: hardened_dep_policy initializes the union's Flags field.
            assert_eq!(unsafe { policy.Anonymous.Flags }, expected);
            assert!(policy.Permanent);
        }
    }

    #[test]
    fn hardening_applies_to_the_current_process_idempotently() {
        pre_main_hardening_windows().expect("apply Windows hardening");
        pre_main_hardening_windows().expect("already-applied hardening remains successful");

        let mut dep = PROCESS_MITIGATION_DEP_POLICY::default();
        let mut extension_points = PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY::default();
        // SAFETY: both queries receive writable buffers of the required policy
        // structure and size; neither API call retains the buffer.
        unsafe {
            assert_ne!(
                GetProcessMitigationPolicy(
                    GetCurrentProcess(),
                    ProcessDEPPolicy,
                    &raw mut dep as *mut c_void,
                    size_of_val(&dep),
                ),
                0,
                "query DEP policy: {}",
                io::Error::last_os_error()
            );
            assert_ne!(
                GetProcessMitigationPolicy(
                    GetCurrentProcess(),
                    ProcessExtensionPointDisablePolicy,
                    &raw mut extension_points as *mut c_void,
                    size_of_val(&extension_points),
                ),
                0,
                "query extension-point policy: {}",
                io::Error::last_os_error()
            );
            assert_eq!(dep.Anonymous.Flags & 1, 1, "DEP must be enabled");
            assert!(dep.Permanent, "DEP must be permanent");
            assert_eq!(
                extension_points.Anonymous.Flags & 1,
                1,
                "extension points must be disabled"
            );
        }
    }
}
