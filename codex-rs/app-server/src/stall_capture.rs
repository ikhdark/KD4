//! One content-free diagnostic sample for the standalone debug app-server.
//! The native observer reads atomics; it never submits work to the runtime
//! whose scheduler or blocking pool it might be observing.

use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use tokio::task::JoinHandle;

const CHECK_INTERVAL: Duration = Duration::from_millis(100);
const STALL_MS: u64 = 2_000;
const CAPTURE_WINDOW: Duration = Duration::from_secs(90);
const RETIRE_GRACE: Duration = Duration::from_millis(250);
const OUTPUT_FILE: &str = "stall-capture.jsonl";

// Local owner identity, not an executable attestation. No source paths or
// arbitrary build-supplied strings are exported in the sample.
const OWNER_SOURCE_FNV1A64: u64 = owner_source_fingerprint(&[
    include_bytes!("lib.rs"),
    include_bytes!("main.rs"),
    include_bytes!("stall_capture.rs"),
]);

const fn owner_source_fingerprint(sources: &[&[u8]]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    let mut file_index = 0;
    while file_index < sources.len() {
        let mut byte_index = 0;
        while byte_index < sources[file_index].len() {
            hash ^= sources[file_index][byte_index] as u64;
            hash = hash.wrapping_mul(0x100000001b3);
            byte_index += 1;
        }
        file_index += 1;
    }
    hash
}

#[derive(Clone, Copy)]
pub(crate) enum Phase {
    SqliteAdmission = 1,
    InstallationIdentity = 2,
}

pub(crate) struct Capture {
    state: Arc<State>,
    stop: Option<mpsc::Sender<()>>,
    retired: Mutex<mpsc::Receiver<io::Result<()>>>,
    scheduler: JoinHandle<()>,
    blocking: JoinHandle<()>,
}

struct State {
    origin: Instant,
    done: AtomicBool,
    // Milliseconds since origin and phase are published together.
    phase: AtomicU64,
    scheduler_ms: AtomicU64,
    scheduler_ticks: AtomicU64,
    // Zero means no pending probe; a queued timestamp is offset by one.
    blocking_queued: AtomicU64,
    blocking_completed: AtomicU64,
}

impl State {
    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
}

pub(crate) struct Wait<'a>(&'a State);

impl Drop for Wait<'_> {
    fn drop(&mut self) {
        self.0.phase.store(0, Relaxed);
    }
}

impl Capture {
    pub(crate) fn start(home: &Path) -> io::Result<Self> {
        let home = home.canonicalize()?;
        let executable = std::env::current_exe()?.canonicalize()?;
        let cwd = std::env::current_dir()?.canonicalize()?;
        if std::env::var_os("CODEX_HOME").is_none_or(|value| value.is_empty())
            || home.parent() != executable.parent()
            || home != cwd
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "dev stall capture requires explicit CODEX_HOME and working directory in a private child directory beside the standalone executable",
            ));
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(home.join(OUTPUT_FILE))?;
        let state = Arc::new(State {
            origin: Instant::now(),
            done: AtomicBool::new(false),
            phase: AtomicU64::new(0),
            scheduler_ms: AtomicU64::new(0),
            scheduler_ticks: AtomicU64::new(0),
            blocking_queued: AtomicU64::new(0),
            blocking_completed: AtomicU64::new(0),
        });
        let (stop, stopped) = mpsc::channel();
        let (retire, retired) = mpsc::channel();
        let observer_state = Arc::clone(&state);
        thread::Builder::new()
            .name("app-server-stall-capture".into())
            .spawn(move || {
                let result = observe(&observer_state, &stopped, file);
                observer_state.done.store(true, Relaxed);
                let _ = retire.send(result);
            })?;
        let scheduler = tokio::spawn(scheduler_probe(Arc::clone(&state)));
        let blocking = tokio::spawn(blocking_probe(Arc::clone(&state)));
        Ok(Self {
            state,
            stop: Some(stop),
            retired: Mutex::new(retired),
            scheduler,
            blocking,
        })
    }

    // Only the startup supervisor marks phases; they never nest or overlap.
    pub(crate) fn enter(&self, phase: Phase) -> Wait<'_> {
        self.state
            .phase
            .store((self.state.now_ms() << 8) | phase as u64, Relaxed);
        Wait(&self.state)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.state.done.store(true, Relaxed);
        drop(self.stop.take());
        self.scheduler.abort();
        self.blocking.abort();
        // The observer drops its file before acknowledging retirement. Do not
        // let a stalled diagnostic write hold up the isolated app-server exit.
        let retired = self
            .retired
            .get_mut()
            .is_ok_and(|retired| matches!(retired.recv_timeout(RETIRE_GRACE), Ok(Ok(()))));
        if !retired {
            tracing::warn!("dev stall capture did not complete");
        }
    }
}

async fn scheduler_probe(state: Arc<State>) {
    while !state.done.load(Relaxed) {
        state.scheduler_ms.store(state.now_ms(), Relaxed);
        state.scheduler_ticks.fetch_add(1, Relaxed);
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

async fn blocking_probe(state: Arc<State>) {
    while !state.done.load(Relaxed) {
        state.blocking_queued.store(state.now_ms() + 1, Relaxed);
        let sample = Arc::clone(&state);
        // At most one queued closure, with no I/O or application state.
        let result = tokio::task::spawn_blocking(move || {
            sample.blocking_completed.fetch_add(1, Relaxed);
            sample.blocking_queued.store(0, Relaxed);
        })
        .await;
        if result.is_err() {
            break;
        }
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

fn observe(state: &State, stop: &mpsc::Receiver<()>, mut file: File) -> io::Result<()> {
    while let Some(remaining) = CAPTURE_WINDOW.checked_sub(state.origin.elapsed()) {
        if stop.recv_timeout(CHECK_INTERVAL.min(remaining)) != Err(mpsc::RecvTimeoutError::Timeout)
            || state.origin.elapsed() >= CAPTURE_WINDOW
        {
            return Ok(());
        }
        let now_ms = state.now_ms();
        let phase = state.phase.load(Relaxed);
        let phase_age_ms = now_ms.saturating_sub(phase >> 8);
        let scheduler_ms = now_ms.saturating_sub(state.scheduler_ms.load(Relaxed));
        let blocking_ms = state
            .blocking_queued
            .load(Relaxed)
            .checked_sub(1)
            .map(|queued| now_ms.saturating_sub(queued));
        let (boundary, pending_ms, owner_chain) = match (phase & 0xff, phase_age_ms >= STALL_MS) {
            (1, true) => (
                "startup.sqlite_admission",
                phase_age_ms,
                &["run_main_owned", "init_sqlite_state_db_with_fresh_start_on_corruption"][..],
            ),
            (2, true) => (
                "startup.installation_identity",
                phase_age_ms,
                &["run_main_owned", "codex_core::resolve_installation_id"][..],
            ),
            _ if scheduler_ms >= STALL_MS => (
                "runtime.scheduler",
                scheduler_ms,
                &["stall_capture::scheduler_probe"][..],
            ),
            _ if blocking_ms.is_some_and(|ms| ms >= STALL_MS) => (
                "runtime.blocking_pool",
                blocking_ms.unwrap_or_default(),
                &["stall_capture::blocking_probe", "tokio::task::spawn_blocking"][..],
            ),
            _ => continue,
        };
        let row = serde_json::json!({
            "schema": 1,
            "owner": "codex_app_server",
            "ownerSourceFnv1a64": format!("{OWNER_SOURCE_FNV1A64:016x}"),
            "boundary": boundary,
            "logicalOwnerChain": owner_chain,
            "elapsedMs": now_ms,
            "boundaryPendingMs": pending_ms,
            "schedulerHeartbeatAgeMs": scheduler_ms,
            "schedulerTicks": state.scheduler_ticks.load(Relaxed),
            "blockingPendingMs": blocking_ms,
            "blockingCompleted": state.blocking_completed.load(Relaxed),
        });
        // One bounded record; no content, method names, or native stacks.
        writeln!(file, "{row}")?;
        return Ok(());
    }
    Ok(())
}
