//! Opt-in, content-free observations at the actual V8 display boundary.
//!
//! Heap endpoints are not a peak measurement. Windows working-set peaks are
//! lifetime-to-observation high water, while thread cycles cover this conversion.
use std::cell::Cell;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Instant;

pub(super) struct Observation {
    origin: &'static str,
    budget: usize,
    started: Instant,
    used_heap: usize,
    total_heap: usize,
    windows: Option<WindowsCounters>,
    journal_scope: Option<JournalScope>,
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
        let journal_scope = JournalScope::enter();
        let heap = scope.get_heap_statistics();
        let windows = windows_counters();
        Some(Self {
            origin,
            budget,
            used_heap: heap.used_heap_size(),
            total_heap: heap.total_heap_size(),
            windows,
            journal_scope,
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
        if self.journal_scope.as_ref().is_some_and(|scope| scope.outermost) {
            // All native timing/heap/CPU snapshots above precede journal I/O.
            // Only explicit diagnostics block here; no runtime/dispatch/queue
            // lock is acquired while the process-local journal lock is held.
            append_journal(serde_json::json!({
                "journal_kind": "metric", "journal_scope": "outermost_conversion",
                "event.name": "codex.code_mode.display_metrics", "version": 1,
                "process_id": std::process::id(), "origin": self.origin,
                "budget_bytes": self.budget, "elapsed_ns": elapsed_ns,
                "result_ok": result.is_ok(), "output_bytes": result.as_ref().map_or(0, String::len),
                "heap_available": heap_available, "used_heap_before": self.used_heap,
                "used_heap_after": heap.as_ref().map_or(0, v8::HeapStatistics::used_heap_size),
                "total_heap_before": self.total_heap,
                "total_heap_after": heap.as_ref().map_or(0, v8::HeapStatistics::total_heap_size),
                "cpu_available": cpu_available,
                "thread_cycles": after.thread_cycles.saturating_sub(before.thread_cycles),
                "working_set_before": before.working_set, "working_set_after": after.working_set,
                "lifetime_peak_working_set_before": before.peak_working_set,
                "lifetime_peak_working_set_after": after.peak_working_set,
                "private_bytes_before": before.private_bytes, "private_bytes_after": after.private_bytes,
            }));
        }
    }
}

const JOURNAL_RECORD_LIMIT: usize = 256;
const JOURNAL_BYTE_LIMIT: usize = 256 * 1024;
const JOURNAL_FAILURE_RESERVE: usize = 512;

fn journal_directory() -> Option<&'static Path> {
    static DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIRECTORY
        .get_or_init(|| {
            if !std::env::var_os("CODEX_CODE_MODE_DISPLAY_METRICS")
                .is_some_and(|value| value == "1")
            {
                return None;
            }
            std::env::var_os("CODEX_CODE_MODE_DISPLAY_METRICS_DIR").map(PathBuf::from)
        })
        .as_deref()
}

// console(Error-with-evidence) calls the instrumented text converter internally.
// Keep its legacy log, but journal only the outer conversion: writing the inner
// journal would otherwise add disk I/O inside the outer component's timer.
thread_local! {
    static JOURNAL_DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct JournalScope {
    outermost: bool,
}

impl JournalScope {
    fn enter() -> Option<Self> {
        journal_directory()?;
        Some(JOURNAL_DEPTH.with(|depth| {
            let previous = depth.get();
            depth.set(previous.saturating_add(1));
            Self { outermost: previous == 0 }
        }))
    }
}

impl Drop for JournalScope {
    fn drop(&mut self) {
        JOURNAL_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

struct DisplayJournal {
    file: File,
    records: usize,
    charged_bytes: usize,
    next_sequence: u64,
    failed: bool,
}

impl DisplayJournal {
    fn open(directory: &Path) -> std::io::Result<Self> {
        if !directory.is_absolute() {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        }
        for ancestor in directory.ancestors() {
            let metadata = std::fs::symlink_metadata(ancestor)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
                }
            }
        }
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(directory.join(format!("display-metrics-{}.jsonl", std::process::id())))?;
        Ok(Self { file, records: 0, charged_bytes: 0, next_sequence: 1, failed: false })
    }

    fn fail(&mut self, reason: &'static str) {
        if self.failed {
            return;
        }
        self.failed = true;
        let marker = serde_json::json!({
            "journal_kind": "failure", "version": 1,
            "process_id": std::process::id(), "sequence": self.next_sequence,
            "reason": reason,
        });
        if let Ok(mut bytes) = serde_json::to_vec(&marker) {
            bytes.push(b'\n');
            if bytes.len() <= JOURNAL_FAILURE_RESERVE
                && self.records < JOURNAL_RECORD_LIMIT
                && self.charged_bytes.saturating_add(bytes.len()) <= JOURNAL_BYTE_LIMIT
            {
                self.records += 1;
                self.charged_bytes += bytes.len();
                // A failed/partial marker is still rejected by the required
                // parser's strict LF/JSON/sequence/exact-call-count gates.
                let _ = self.file.write_all(&bytes);
            }
        }
    }

    fn append(&mut self, mut metric: serde_json::Value) {
        if self.failed {
            return;
        }
        metric["sequence"] = self.next_sequence.into();
        self.next_sequence = self.next_sequence.saturating_add(1);
        let Ok(mut bytes) = serde_json::to_vec(&metric) else {
            self.fail("serialization");
            return;
        };
        bytes.push(b'\n');
        if self.records >= JOURNAL_RECORD_LIMIT - 1 {
            self.fail("record_limit");
            return;
        }
        if self.charged_bytes.saturating_add(bytes.len())
            > JOURNAL_BYTE_LIMIT - JOURNAL_FAILURE_RESERVE
        {
            self.fail("byte_limit");
            return;
        }
        // Charge the whole attempted write: partial failures cannot exceed the
        // bound, and the reserved failure marker always fits.
        self.records += 1;
        self.charged_bytes += bytes.len();
        if self.file.write_all(&bytes).is_err() {
            self.fail("write_error");
        }
    }
}

fn append_journal(metric: serde_json::Value) {
    static JOURNAL: OnceLock<Option<Mutex<DisplayJournal>>> = OnceLock::new();
    let Some(journal) = JOURNAL
        .get_or_init(|| {
            // Failure leaves a missing/empty journal, never a successful metric.
            // The required owning capture must reject it; do not fall back to logs.
            DisplayJournal::open(journal_directory()?).ok().map(Mutex::new)
        })
        .as_ref()
    else {
        return;
    };
    match journal.lock() {
        Ok(mut journal) => journal.append(metric),
        Err(poisoned) => poisoned.into_inner().fail("lock_poisoned"),
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
