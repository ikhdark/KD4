//! Native observations on the existing turn clock, without a polling task.
//! Process accounting includes concurrent turns in this process, not children,
//! Desktop renderers, session-exclusive CPU, or evidence of contention.
use super::*;
use serde_json::{Value, json};

const LIMIT: usize = 2048;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Capture {
    baseline: Option<ResourceSample>,
    latest: Option<ResourceSample>,
    connections: Vec<Value>,
    transport_phases: Vec<Value>,
    dropped_connections: u64,
    dropped_transport_phases: u64,
    execution: Option<Value>,
    pub(super) cache_comparison_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResourceSample {
    at_ns: u128,
    lifetime: u64,
    cpu_100ns: u64,
    private_bytes: u64,
    read_bytes: u64,
    write_bytes: u64,
    processors: u32,
}

fn label(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty() && value.len() <= 256)
}

impl Capture {
    pub(super) fn start(&mut self) {
        self.baseline = sample_resources();
        self.latest = None;
        self.cache_comparison_key = label("CODEX_DIAGNOSTIC_CACHE_COMPARISON_KEY");
        if let (Some(cohort), Some(run), Some(state)) = (
            label("CODEX_DIAGNOSTIC_COHORT_ID"),
            label("CODEX_DIAGNOSTIC_RUN_ID"),
            label("CODEX_DIAGNOSTIC_PROCESS_STATE"),
        ) && matches!(
            state.as_str(),
            "fresh_process" | "initialized_process" | "reused_connection"
        ) {
            self.execution = Some(json!({"cohortId":cohort, "runId":run, "state":state,
                "basis":"explicit_runner_labels_not_inferred_from_timing"}));
        }
    }

    pub(super) fn finish(&mut self) {
        self.latest = sample_resources();
    }

    pub(super) fn snapshot(&self, timeline: &codex_protocol::timing::TurnTimingTimeline) -> Value {
        let origin = u128::from(timeline.started_at_ns);
        let mut resources = Vec::new();
        if let (Some(a), Some(b)) = (&self.baseline, &self.latest)
            && a.lifetime == b.lifetime
            && a.processors == b.processors
            && a.processors > 0
            && let Some(interval) = b.at_ns.checked_sub(a.at_ns).filter(|value| *value > 0)
            && let Some(at) = b
                .at_ns
                .checked_sub(origin)
                .filter(|value| *value <= u128::from(timeline.duration_ns))
            && interval <= at
            && let (Some(cpu), Some(read), Some(write)) = (
                b.cpu_100ns.checked_sub(a.cpu_100ns),
                b.read_bytes.checked_sub(a.read_bytes),
                b.write_bytes.checked_sub(a.write_bytes),
            )
        {
            resources.push(json!({"id":"native-process-interval", "processLifetime":format!("{}:{}", std::process::id(), b.lifetime),
                "intervalMs":interval as f64 / 1e6, "atMs":at as f64 / 1e6,
                "cpuPercent":cpu as f64 * 100.0 * 100.0 / interval as f64 / f64::from(a.processors),
                "privateBytes":b.private_bytes, "readBytes":read, "writeBytes":write}));
        }
        json!({"schemaVersion":1, "clockDomainId":timeline.clock_domain_id,
            "observedThroughMs":timeline.duration_ns as f64 / 1e6,
            "connections":self.connections, "transportPhases":self.transport_phases,
            "resources":resources, "execution":self.execution,
            "capture":{"droppedConnections":self.dropped_connections,
                "droppedTransportPhases":self.dropped_transport_phases,
                "resourcesScope":"current_process_including_concurrent_turns_excluding_children_and_desktop",
                "resourceSampleAvailable":!resources.is_empty(),
                "complete":false,
                "scope":"instrumented_connection_and_transport_boundaries; no socket/provider/client inference"}})
    }
}

impl TurnTimingState {
    /// The physical attempt identity comes from the same transport callback as
    /// dispatch. Preserve unavailable phases too; never fill them by residuals.
    pub fn record_transport_phase(
        &self,
        request: &str,
        attempt: &str,
        phase: &str,
        duration: Option<Duration>,
        wire_bytes: Option<u64>,
        provenance: &str,
        unavailable: Option<&str>,
    ) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() {
            return;
        }
        let capture = &mut state.measurements.performance;
        if capture.transport_phases.len() >= LIMIT {
            capture.dropped_transport_phases = capture.dropped_transport_phases.saturating_add(1);
            return;
        }
        capture.transport_phases.push(json!({"samplingRequestId":request, "physicalAttemptId":attempt,
            "phase":phase, "durationNs":duration.and_then(|duration| u64::try_from(duration.as_nanos()).ok()),
            "wireBytes":wire_bytes, "provenance":provenance, "unavailableReason":unavailable}));
    }

    pub fn record_connection_observation(
        &self,
        request: &str,
        attempt: &str,
        connection: &str,
        kind: &str,
        started: std::time::Instant,
    ) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() {
            return;
        }
        let start = codex_protocol::timing::process_monotonic_ns_at(started)
            .filter(|_| {
                self.clock.domain_id() == Some(codex_protocol::timing::process_clock_domain_id())
            })
            .and_then(|at| at.checked_sub(state.started_sample?.time.monotonic_ns));
        let end = state.started_sample.and_then(|start| {
            self.clock
                .sample()
                .time
                .monotonic_ns
                .checked_sub(start.time.monotonic_ns)
        });
        let capture = &mut state.measurements.performance;
        if capture.connections.len() >= LIMIT {
            capture.dropped_connections = capture.dropped_connections.saturating_add(1);
            return;
        }
        capture.connections.push(
            json!({"id":format!("connection-{}", capture.connections.len()),
            "samplingRequestId":request, "physicalAttemptId":attempt, "connectionId":connection,
            "kind":kind, "startedMs":start.map(|at| at as f64 / 1e6),
            "completedMs":end.map(|at| at as f64 / 1e6)}),
        );
    }
}

#[cfg(not(windows))]
fn sample_resources() -> Option<ResourceSample> {
    None
}

#[cfg(windows)]
fn sample_resources() -> Option<ResourceSample> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::{
        GetActiveProcessorCount, GetCurrentProcess, GetProcessIoCounters, GetProcessTimes, IO_COUNTERS,
    };
    fn ticks(time: FILETIME) -> u64 {
        (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
    }
    // All output buffers are correctly sized, initialized, and used only after
    // successful API returns. The current-process pseudo-handle is not closed.
    unsafe {
        let process = GetCurrentProcess();
        let mut created: FILETIME = std::mem::zeroed();
        let mut exited: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let mut io: IO_COUNTERS = std::mem::zeroed();
        let mut memory: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        memory.cb = std::mem::size_of_val(&memory) as u32;
        if GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) == 0
            || GetProcessIoCounters(process, &mut io) == 0
            || GetProcessMemoryInfo(
                process,
                (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
                memory.cb,
            ) == 0
        {
            return None;
        }
        Some(ResourceSample {
            at_ns: codex_protocol::timing::process_monotonic_ns(),
            lifetime: ticks(created),
            cpu_100ns: ticks(kernel).checked_add(ticks(user))?,
            private_bytes: memory.PrivateUsage as u64,
            read_bytes: io.ReadTransferCount,
            write_bytes: io.WriteTransferCount,
            processors: GetActiveProcessorCount(u16::MAX),
        })
    }
}
