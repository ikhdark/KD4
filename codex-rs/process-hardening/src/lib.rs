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

/// Result of [`prefer_largest_last_level_cache`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LastLevelCachePreference {
    /// This process's default CPU Sets now name the logical processors that
    /// share the uniquely largest last-level cache.
    Applied {
        logical_processors: u32,
        cache_bytes: u64,
    },
    /// Nothing changed: a single or uniform last-level cache, an existing
    /// affinity or CPU Set restriction, an unsupported topology, or a
    /// non-Windows platform.
    NotApplicable,
}

/// On processors whose last-level caches differ in size (for example a Ryzen
/// X3D part with one stacked-cache CCD), prefers the logical processors that
/// share the largest one for this process's own threads. The harness runtime
/// hands work between many threads; keeping them on one cache domain avoids
/// cross-die cache-line transfers and gives them the larger cache.
///
/// Only the process-default CPU Sets change. They are not inherited, so child
/// processes (commands, builds, tools) keep the whole machine. An explicit
/// process affinity mask or existing CPU Set assignment is left untouched, as
/// is every system whose last-level caches are uniform. Other platforms are
/// unchanged.
pub fn prefer_largest_last_level_cache() -> std::io::Result<LastLevelCachePreference> {
    #[cfg(windows)]
    {
        windows_cache_preference::prefer_largest_last_level_cache()
    }
    #[cfg(not(windows))]
    {
        Ok(LastLevelCachePreference::NotApplicable)
    }
}

#[cfg(windows)]
mod windows_cache_preference {
    use super::LastLevelCachePreference;
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::System::SystemInformation::CpuSetInformation;
    use windows_sys::Win32::System::SystemInformation::GetLogicalProcessorInformationEx;
    use windows_sys::Win32::System::SystemInformation::GetSystemCpuSetInformation;
    use windows_sys::Win32::System::SystemInformation::RelationCache;
    use windows_sys::Win32::System::SystemInformation::SYSTEM_CPU_SET_INFORMATION;
    use windows_sys::Win32::System::SystemInformation::SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::System::Threading::GetProcessAffinityMask;
    use windows_sys::Win32::System::Threading::GetProcessDefaultCpuSets;
    use windows_sys::Win32::System::Threading::SetProcessDefaultCpuSets;

    // SYSTEM_CPU_SET_INFORMATION AllFlags bits.
    const CPU_SET_ALLOCATED: u8 = 0x2;
    const CPU_SET_ALLOCATED_TO_TARGET_PROCESS: u8 = 0x4;
    const CPU_SET_REAL_TIME: u8 = 0x8;

    struct LastLevelCache {
        bytes: u64,
        group: u16,
        mask: usize,
    }

    pub(super) fn prefer_largest_last_level_cache() -> std::io::Result<LastLevelCachePreference> {
        let caches = last_level_caches()?;
        // A single processor group keeps affinity masks and CPU Set indexes
        // directly comparable; larger topologies are left to the scheduler.
        if caches.len() < 2 || caches.iter().any(|cache| cache.group != 0) {
            return Ok(LastLevelCachePreference::NotApplicable);
        }
        let largest = caches.iter().map(|cache| cache.bytes).max().unwrap_or(0);
        let mut largest_caches = caches.iter().filter(|cache| cache.bytes == largest);
        let (Some(chosen), None) = (largest_caches.next(), largest_caches.next()) else {
            return Ok(LastLevelCachePreference::NotApplicable);
        };

        // SAFETY: The pseudo-handle refers to this process.
        let process = unsafe { GetCurrentProcess() };
        let mut process_mask = 0usize;
        let mut system_mask = 0usize;
        // SAFETY: Both outputs are valid, writable usize values.
        if unsafe { GetProcessAffinityMask(process, &mut process_mask, &mut system_mask) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if process_mask != system_mask || chosen.mask & system_mask == system_mask {
            return Ok(LastLevelCachePreference::NotApplicable);
        }
        let mut existing = 0u32;
        // SAFETY: A zero-length query only reports the required count.
        if unsafe { GetProcessDefaultCpuSets(process, std::ptr::null_mut(), 0, &mut existing) } == 0
        {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_INSUFFICIENT_BUFFER as i32) {
                return Ok(LastLevelCachePreference::NotApplicable);
            }
            return Err(error);
        }
        if existing != 0 {
            return Ok(LastLevelCachePreference::NotApplicable);
        }

        let ids = cpu_set_ids_for(chosen)?;
        if ids.is_empty() {
            return Ok(LastLevelCachePreference::NotApplicable);
        }
        let count = u32::try_from(ids.len()).map_err(std::io::Error::other)?;
        // SAFETY: `ids` holds `count` CPU Set IDs and outlives the call.
        if unsafe { SetProcessDefaultCpuSets(process, ids.as_ptr(), count) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(LastLevelCachePreference::Applied {
            logical_processors: count,
            cache_bytes: chosen.bytes,
        })
    }

    fn last_level_caches() -> std::io::Result<Vec<LastLevelCache>> {
        let mut length = 0u32;
        // SAFETY: A null buffer only reports the required length.
        unsafe { GetLogicalProcessorInformationEx(RelationCache, std::ptr::null_mut(), &mut length) };
        let buffer = query_buffer(length, |buffer, length| {
            // SAFETY: `buffer` is writable for `length` bytes and 8-byte aligned.
            unsafe { GetLogicalProcessorInformationEx(RelationCache, buffer.cast(), length) }
        })?;
        let bytes = buffer.len() * std::mem::size_of::<u64>();
        let base = buffer.as_ptr().cast::<u8>();
        let mut caches = Vec::new();
        let mut offset = 0usize;
        while offset + std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() <= bytes {
            // SAFETY: The record starts inside the returned buffer; read it
            // without assuming alignment.
            let record = unsafe {
                std::ptr::read_unaligned(
                    base.add(offset).cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
                )
            };
            if record.Size == 0 {
                break;
            }
            if record.Relationship == RelationCache {
                // SAFETY: RelationCache records hold the cache variant.
                let cache = unsafe { record.Anonymous.Cache };
                if cache.Level == 3 {
                    // A cache spanning several groups is outside the supported
                    // single-group topology; report it so the caller declines.
                    let group_mask = if cache.GroupCount <= 1 {
                        // SAFETY: Zero or one group mask occupies GroupMask.
                        unsafe { cache.Anonymous.GroupMask }
                    } else {
                        return Ok(Vec::new());
                    };
                    caches.push(LastLevelCache {
                        bytes: u64::from(cache.CacheSize),
                        group: group_mask.Group,
                        mask: group_mask.Mask,
                    });
                }
            }
            offset += record.Size as usize;
        }
        Ok(caches)
    }

    fn cpu_set_ids_for(cache: &LastLevelCache) -> std::io::Result<Vec<u32>> {
        let mut length = 0u32;
        // SAFETY: A null buffer only reports the required length.
        unsafe {
            GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut length, std::ptr::null_mut(), 0)
        };
        let buffer = query_buffer(length, |buffer, length| {
            // SAFETY: `buffer` is writable for `*length` bytes and 8-byte aligned.
            unsafe {
                GetSystemCpuSetInformation(buffer.cast(), *length, length, std::ptr::null_mut(), 0)
            }
        })?;
        let bytes = buffer.len() * std::mem::size_of::<u64>();
        let base = buffer.as_ptr().cast::<u8>();
        let mut ids = Vec::new();
        let mut offset = 0usize;
        while offset + std::mem::size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes {
            // SAFETY: The record starts inside the returned buffer.
            let record = unsafe {
                std::ptr::read_unaligned(base.add(offset).cast::<SYSTEM_CPU_SET_INFORMATION>())
            };
            if record.Size == 0 {
                break;
            }
            if record.Type == CpuSetInformation {
                // SAFETY: CpuSetInformation records hold the CpuSet variant.
                let set = unsafe { record.Anonymous.CpuSet };
                // SAFETY: AllFlags is the plain-byte view of the flag union.
                let flags = unsafe { set.Anonymous1.AllFlags };
                let reserved_elsewhere = flags & CPU_SET_ALLOCATED != 0
                    && flags & CPU_SET_ALLOCATED_TO_TARGET_PROCESS == 0;
                let index = u32::from(set.LogicalProcessorIndex);
                if set.Group == cache.group
                    && index < usize::BITS
                    && cache.mask & (1usize << index) != 0
                    && !reserved_elsewhere
                    && flags & CPU_SET_REAL_TIME == 0
                {
                    ids.push(set.Id);
                }
            }
            offset += record.Size as usize;
        }
        Ok(ids)
    }

    /// Calls `query` with an 8-byte aligned buffer of at least `length` bytes.
    fn query_buffer(
        length: u32,
        query: impl FnOnce(*mut u64, &mut u32) -> windows_sys::core::BOOL,
    ) -> std::io::Result<Vec<u64>> {
        if length == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut length = length;
        let mut buffer = vec![0u64; (length as usize).div_ceil(std::mem::size_of::<u64>())];
        if query(buffer.as_mut_ptr(), &mut length) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        buffer.truncate((length as usize).div_ceil(std::mem::size_of::<u64>()));
        Ok(buffer)
    }
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

