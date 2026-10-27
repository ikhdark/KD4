use std::ops::Deref;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::SendError;

/// Largest individual stdin payload, including direct in-process callers.
pub const MAX_STDIN_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const PROCESS_STDIN_BYTES: usize = 8 * 1024 * 1024;
const GLOBAL_STDIN_BYTES: usize = 32 * 1024 * 1024;
const DIAGNOSTIC_TARGET: &str = "codex_utils_pty::stdin_custody";
static DIAGNOSTIC_GLOBAL_BYTES: AtomicUsize = AtomicUsize::new(0);
static NEXT_DIAGNOSTIC_CHANNEL: AtomicU64 = AtomicU64::new(1);

struct CustodyDiagnostics {
    channel: u64,
    resident: AtomicUsize,
    sampled: AtomicBool,
}

impl CustodyDiagnostics {
    fn record(&self, bytes: usize, release: bool, wait_micros: u128) {
        let previous = if release {
            self.resident.fetch_sub(bytes, Ordering::Relaxed)
        } else {
            self.resident.fetch_add(bytes, Ordering::Relaxed)
        };
        let (resident, global) = if release {
            (previous - bytes, DIAGNOSTIC_GLOBAL_BYTES.fetch_sub(bytes, Ordering::Relaxed) - bytes)
        } else {
            (previous + bytes, DIAGNOSTIC_GLOBAL_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes)
        };
        // Coalesce small writes by MiB boundary; no payload, command, path or
        // credentials are logged. Counters are diagnostic, not admission state.
        if resident >= 1024 * 1024 {
            self.sampled.store(true, Ordering::Relaxed);
        }
        if (resident == 0 && self.sampled.load(Ordering::Relaxed))
            || resident / (1024 * 1024) != previous / (1024 * 1024) {
            log::debug!(target: DIAGNOSTIC_TARGET,
                "stdin_custody channel={} phase={} bytes={} resident={} global={} wait_us={}",
                self.channel, if release { "released" } else { "reserved" }, bytes,
                resident, global, wait_micros);
        }
    }
}

/// Accepted input retains its byte custody until the actual writer consumes or
/// discards it, not merely until an RPC acknowledges channel admission.
pub struct StdinPayload {
    bytes: Vec<u8>,
    _process: OwnedSemaphorePermit,
    _global: OwnedSemaphorePermit,
    diagnostics: Option<Arc<CustodyDiagnostics>>,
}

impl Drop for StdinPayload {
    fn drop(&mut self) {
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics.record(self.bytes.len(), true, 0);
        }
    }
}

impl Deref for StdinPayload {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone)]
pub struct StdinSender {
    sender: mpsc::Sender<StdinPayload>,
    budget: Arc<Semaphore>,
    diagnostics: Option<Arc<CustodyDiagnostics>>,
}

pub struct StdinPermit<'a> {
    slot: mpsc::Permit<'a, StdinPayload>,
    payload: StdinPayload,
}

impl StdinPermit<'_> {
    /// Synchronous commit for callers that atomically record write identities.
    pub fn send(self) {
        self.slot.send(self.payload);
    }
}

pub fn stdin_channel(capacity: usize) -> (StdinSender, mpsc::Receiver<StdinPayload>) {
    let (sender, receiver) = mpsc::channel(capacity);
    (
        StdinSender {
            sender,
            budget: Arc::new(Semaphore::new(PROCESS_STDIN_BYTES)),
            diagnostics: log::log_enabled!(target: DIAGNOSTIC_TARGET, log::Level::Debug).then(|| {
                Arc::new(CustodyDiagnostics {
                    channel: NEXT_DIAGNOSTIC_CHANNEL.fetch_add(1, Ordering::Relaxed),
                    resident: AtomicUsize::new(0),
                    sampled: AtomicBool::new(false),
                })
            }),
        },
        receiver,
    )
}

impl StdinSender {
    pub async fn reserve(&self, bytes: Vec<u8>) -> Result<StdinPermit<'_>, SendError<Vec<u8>>> {
        if bytes.len() > MAX_STDIN_CHUNK_BYTES || self.sender.is_closed() {
            return Err(SendError(bytes));
        }
        // Do not retain uncharged spare capacity supplied by a typed caller.
        let bytes = bytes.into_boxed_slice().into_vec();
        let length = bytes.len() as u32;
        let diagnostic_started = self.diagnostics.as_ref().map(|_| std::time::Instant::now());
        // Per-process admission comes first: one blocked child must not reserve
        // the entire global allowance while waiting for its own capacity.
        let process = match tokio::select! {
            biased;
            _ = self.sender.closed() => return Err(SendError(bytes)),
            permit = self.budget.clone().acquire_many_owned(length) => permit,
        } {
            Ok(permit) => permit,
            Err(_) => return Err(SendError(bytes)),
        };
        static GLOBAL: OnceLock<Arc<Semaphore>> = OnceLock::new();
        let global_budget = GLOBAL
            .get_or_init(|| Arc::new(Semaphore::new(GLOBAL_STDIN_BYTES)))
            .clone();
        let global = match tokio::select! {
            biased;
            _ = self.sender.closed() => return Err(SendError(bytes)),
            permit = global_budget.acquire_many_owned(length) => permit,
        } {
            Ok(permit) => permit,
            Err(_) => return Err(SendError(bytes)),
        };
        let slot = match self.sender.reserve().await {
            Ok(slot) => slot,
            Err(_) => return Err(SendError(bytes)),
        };
        if let (Some(diagnostics), Some(started)) = (&self.diagnostics, diagnostic_started) {
            diagnostics.record(bytes.len(), false, started.elapsed().as_micros());
        }
        Ok(StdinPermit {
            slot,
            payload: StdinPayload {
                bytes,
                _process: process,
                _global: global,
                diagnostics: self.diagnostics.clone(),
            },
        })
    }

    pub async fn send(&self, bytes: Vec<u8>) -> Result<(), SendError<Vec<u8>>> {
        self.reserve(bytes).await?.send();
        Ok(())
    }
}
