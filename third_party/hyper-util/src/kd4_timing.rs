//! Bounded, optional transport facts. No addresses, headers or payloads.
//! Context uses weak observers: pooled sockets and executor tasks must not keep
//! a request, turn or telemetry owner alive. Socket rows are connection-scoped,
//! never an attribution of multiplexed traffic to the initiating request.
#![allow(missing_docs)]
use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct Event {
    pub phase: &'static str,
    pub started_at: Instant,
    pub completed_at: Instant,
    pub wire_bytes: Option<u64>,
    pub connection_id: u64,
    pub connection_scoped: bool,
    pub outcome: &'static str,
}
pub type Observer = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Clone)]
pub struct Context {
    observer: Weak<dyn Fn(Event) + Send + Sync>,
    connection_id: u64,
}
impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportContext")
            .field("connection_id", &self.connection_id)
            .finish()
    }
}
tokio::task_local! { static CURRENT: Option<Context>; }
pub fn current() -> Option<Context> {
    CURRENT.try_with(Clone::clone).ok().flatten()
}
pub async fn scope<F: std::future::Future>(context: Option<Context>, future: F) -> F::Output {
    CURRENT.scope(context, future).await
}
pub fn request(observer: &Observer) -> Context {
    Context {
        observer: Arc::downgrade(observer),
        connection_id: 0,
    }
}
pub fn connection() -> Option<Context> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut context = current()?;
    // Exhaustion fails closed; never wrap into a reused connection identity.
    let mut value = NEXT.load(Ordering::Relaxed);
    loop {
        let next = value.checked_add(1)?;
        match NEXT.compare_exchange_weak(value, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => {
                context.connection_id = value;
                break;
            }
            Err(observed) => value = observed,
        }
    }
    Some(context)
}
pub struct Span {
    context: Option<Context>,
    phase: &'static str,
    started: Option<Instant>,
}
impl Span {
    pub fn start(phase: &'static str) -> Self {
        let context = current().filter(|context| context.observer.strong_count() != 0);
        let started = context.as_ref().map(|_| Instant::now());
        Self {
            context,
            phase,
            started,
        }
    }
    pub fn finish(mut self, success: bool) {
        self.close(if success { "success" } else { "failed" });
    }
    fn close(&mut self, outcome: &'static str) {
        if let (Some(context), Some(start)) = (&self.context, self.started.take()) {
            context.emit(self.phase, start, None, false, outcome);
        }
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        self.close("abandoned");
    }
}
impl Context {
    fn emit(
        &self,
        phase: &'static str,
        start: Instant,
        bytes: Option<u64>,
        socket: bool,
        outcome: &'static str,
    ) {
        if let Some(observer) = self.observer.upgrade() {
            observer(Event {
                phase,
                started_at: start,
                completed_at: Instant::now(),
                wire_bytes: bytes,
                connection_id: self.connection_id,
                connection_scoped: socket,
                outcome,
            });
        }
    }
}

/// This adapter is installed only directly around the raw TCP socket.
#[derive(Debug)]
pub struct Wire {
    context: Context,
    read_started: Option<Instant>,
    write_started: Option<Instant>,
    emitted: u16,
}
impl Wire {
    pub fn new() -> Option<Self> {
        Some(Self {
            context: current()?,
            read_started: None,
            write_started: None,
            emitted: 0,
        })
    }
    pub fn begin(&mut self, read: bool) {
        if self.emitted > 64 || self.context.observer.strong_count() == 0 {
            return;
        }
        let slot = if read {
            &mut self.read_started
        } else {
            &mut self.write_started
        };
        slot.get_or_insert_with(Instant::now);
    }
    pub fn end(&mut self, read: bool, result: &std::task::Poll<std::io::Result<usize>>) {
        let std::task::Poll::Ready(result) = result else {
            return;
        };
        let start = if read {
            self.read_started.take()
        } else {
            self.write_started.take()
        };
        if let Some(start) = start {
            if self.emitted == 64 {
                self.context.emit(
                    "socket_capture_overflow",
                    start,
                    None,
                    true,
                    "retention_limit",
                );
            } else {
                self.context.emit(
                    if read { "socket_read" } else { "socket_write" },
                    start,
                    result.as_ref().ok().map(|count| *count as u64),
                    true,
                    if result.is_ok() { "success" } else { "failed" },
                );
            }
            self.emitted += 1;
        }
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        for (phase, start) in [
            ("socket_read", self.read_started.take()),
            ("socket_write", self.write_started.take()),
        ] {
            if let Some(start) = start {
                self.context.emit(phase, start, None, true, "abandoned");
            }
        }
    }
}
