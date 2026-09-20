//! System-wide monotonic clock shared by a code-mode runtime and its host.
//!
//! A nested tool call carries an absolute deadline. When the runtime and the
//! host are separate processes on the same machine, the deadline must survive
//! the crossing without either side re-basing it: a wall-clock adjustment must
//! not move it, and the receiver must not gain observation time relative to the
//! sender's wrapper. Both peers therefore read the same OS monotonic source and
//! exchange a reading on that timeline, not a duration.
//!
//! The reading is only meaningful between processes on one machine and only
//! within one boot. Callers must treat `None` as "no shared timeline available"
//! and fall back to a policy that does not claim to preserve the budget.

/// A reading of the machine's monotonic timeline, in nanoseconds.
///
/// The zero point is platform defined (typically boot). Only differences
/// between readings are meaningful, and only within one boot on one machine.
pub type SharedMonotonicNanos = u64;

/// Reads the machine's monotonic timeline, or `None` when this platform exposes
/// no source both peers can read.
pub fn shared_monotonic_now() -> Option<SharedMonotonicNanos> {
    platform::now_nanos()
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::System::Performance::QueryPerformanceCounter;
    use windows_sys::Win32::System::Performance::QueryPerformanceFrequency;

    /// `QueryPerformanceCounter` is system wide: every process on the machine
    /// reads one counter, and it is unaffected by wall-clock changes.
    pub(super) fn now_nanos() -> Option<u64> {
        let mut frequency = 0i64;
        let mut counter = 0i64;
        // SAFETY: both calls write one i64 through a valid exclusive pointer.
        let read = unsafe {
            QueryPerformanceFrequency(&mut frequency) != 0
                && QueryPerformanceCounter(&mut counter) != 0
        };
        if !read || frequency <= 0 || counter < 0 {
            return None;
        }
        let counter = u128::try_from(counter).ok()?;
        let frequency = u128::try_from(frequency).ok()?;
        u64::try_from(counter.checked_mul(1_000_000_000)? / frequency).ok()
    }
}

#[cfg(unix)]
mod platform {
    /// `CLOCK_MONOTONIC` is a machine-wide timeline unaffected by settimeofday.
    pub(super) fn now_nanos() -> Option<u64> {
        let mut timespec = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: writes one timespec through a valid exclusive pointer.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timespec) } != 0 {
            return None;
        }
        let seconds = u64::try_from(timespec.tv_sec).ok()?;
        let nanos = u64::try_from(timespec.tv_nsec).ok()?;
        seconds.checked_mul(1_000_000_000)?.checked_add(nanos)
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    pub(super) fn now_nanos() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::shared_monotonic_now;

    #[test]
    fn shared_monotonic_clock_is_available_and_advances() {
        let first = shared_monotonic_now()
            .expect("a supported platform exposes a shared monotonic source");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = shared_monotonic_now().expect("second reading");

        assert!(
            second > first,
            "the shared monotonic clock must advance: {first} then {second}"
        );
        let elapsed_ms = (second - first) / 1_000_000;
        assert!(
            (1..=5_000).contains(&elapsed_ms),
            "a 5ms sleep should register as a plausible elapsed time, got {elapsed_ms}ms"
        );
    }
}
