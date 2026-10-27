//! Opt-in, content-free observations at the actual V8 display boundary.
//!
//! Heap endpoints are not a peak measurement. Windows working-set peaks are
//! lifetime-to-observation high water, while thread cycles cover this conversion.
use std::sync::OnceLock;
use std::time::Instant;

pub(super) struct Observation {
    origin: &'static str,
    budget: usize,
    started: Instant,
    used_heap: usize,
    total_heap: usize,
    windows: Option<WindowsCounters>,
}

#[derive(Clone, Copy)]
struct WindowsCounters {
    thread_cycles: u64,
    working_set: usize,
    peak_working_set: usize,
    private_bytes: usize,
}

impl Observation {
    pub(super) fn start(
        scope: &mut v8::PinScope<'_, '_>,
        origin: &'static str,
        budget: usize,
    ) -> Option<Self> {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        if !*ENABLED.get_or_init(|| {
            std::env::var_os("CODEX_CODE_MODE_DISPLAY_METRICS").is_some_and(|value| value == "1")
        }) || scope.is_execution_terminating()
        {
            return None;
        }
        let heap = scope.get_heap_statistics();
        let windows = windows_counters();
        Some(Self {
            origin,
            budget,
            used_heap: heap.used_heap_size(),
            total_heap: heap.total_heap_size(),
            windows,
            started: Instant::now(),
        })
    }

    pub(super) fn finish(self, scope: &mut v8::PinScope<'_, '_>, result: &Result<String, String>) {
        let elapsed_ns = self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let windows = windows_counters();
        let heap_available = !scope.is_execution_terminating();
        let heap = heap_available.then(|| scope.get_heap_statistics());
        let cpu_available = self.windows.is_some() && windows.is_some();
        let before = self.windows.unwrap_or(WindowsCounters {
            thread_cycles: 0,
            working_set: 0,
            peak_working_set: 0,
            private_bytes: 0,
        });
        let after = windows.unwrap_or(before);
        tracing::info!(target: "codex_code_mode_runtime::display_metrics", {
            event.name = "codex.code_mode.display_metrics", version = 1u64,
            process_id = std::process::id(), origin = self.origin,
            budget_bytes = self.budget, elapsed_ns,
            result_ok = result.is_ok(), output_bytes = result.as_ref().map_or(0, String::len),
            heap_available, used_heap_before = self.used_heap,
            used_heap_after = heap.as_ref().map_or(0, v8::HeapStatistics::used_heap_size), total_heap_before = self.total_heap,
            total_heap_after = heap.as_ref().map_or(0, v8::HeapStatistics::total_heap_size), cpu_available,
            thread_cycles = after.thread_cycles.saturating_sub(before.thread_cycles),
            working_set_before = before.working_set, working_set_after = after.working_set,
            lifetime_peak_working_set_before = before.peak_working_set,
            lifetime_peak_working_set_after = after.peak_working_set,
            private_bytes_before = before.private_bytes, private_bytes_after = after.private_bytes,
        }, "content-free native display observation");
    }
}

#[cfg(windows)]
fn windows_counters() -> Option<WindowsCounters> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};
    use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;
    let mut cycles = 0;
    // SAFETY: the pseudo handles refer to this live process/thread; the aligned
    // counter buffer has the documented size and contains only integer fields.
    let mut memory: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    memory.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let observed = unsafe {
        QueryThreadCycleTime(GetCurrentThread(), &mut cycles) != 0
            && K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
                memory.cb,
            ) != 0
    };
    observed.then_some(WindowsCounters {
        thread_cycles: cycles,
        working_set: memory.WorkingSetSize,
        peak_working_set: memory.PeakWorkingSetSize,
        private_bytes: memory.PrivateUsage,
    })
}

#[cfg(not(windows))]
fn windows_counters() -> Option<WindowsCounters> {
    None
}
