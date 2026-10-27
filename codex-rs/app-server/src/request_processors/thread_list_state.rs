use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Instant;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::OwnedRwLockReadGuard;
use tokio::sync::OwnedRwLockWriteGuard;
use tokio::sync::RwLock;
use tokio::sync::RwLockWriteGuard;

/// Independent lifecycle publication shares admission, while destructive and
/// metadata mutations remain exclusive. Equal resume identities stay ordered.
#[derive(Default)]
pub(crate) struct ThreadListState {
    state: Arc<RwLock<()>>,
    resumes: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

pub(super) struct ThreadResumePermit {
    state: Arc<RwLock<()>>,
    _state: Option<OwnedRwLockReadGuard<()>>,
    _thread: OwnedMutexGuard<()>,
}

impl ThreadResumePermit {
    pub(super) fn release_state(&mut self) {
        self._state.take();
    }

    pub(super) async fn reacquire_state(&mut self) {
        let started = wait_start();
        self._state = Some(Arc::clone(&self.state).read_owned().await);
        record_wait("resume_revalidate", started);
    }
}

fn wait_start() -> Option<Instant> {
    tracing::enabled!(target: "codex_app_server::desktop_benchmark", tracing::Level::DEBUG)
        .then(Instant::now)
}

fn record_wait(gate: &'static str, started: Option<Instant>) {
    if let Some(started) = started {
        tracing::debug!(target: "codex_app_server::desktop_benchmark",
            kdr_desktop_event = "gate_wait", gate,
            wait_us = started.elapsed().as_micros() as u64);
    }
}

impl ThreadListState {
    pub(super) async fn acquire_publication(&self) -> OwnedRwLockReadGuard<()> {
        let started = wait_start();
        let guard = Arc::clone(&self.state).read_owned().await;
        record_wait("publication", started);
        guard
    }

    pub(crate) async fn acquire(&self) -> Result<RwLockWriteGuard<'_, ()>, Infallible> {
        let started = wait_start();
        let guard = self.state.write().await;
        record_wait("mutation", started);
        Ok(guard)
    }

    pub(crate) async fn acquire_owned(
        self: Arc<Self>,
    ) -> Result<OwnedRwLockWriteGuard<()>, Infallible> {
        let started = wait_start();
        let guard = Arc::clone(&self.state).write_owned().await;
        record_wait("owned_mutation", started);
        Ok(guard)
    }

    pub(super) async fn acquire_resume(&self, identity: String) -> ThreadResumePermit {
        let gate = {
            let mut gates = self
                .resumes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            gates.retain(|_, gate| gate.strong_count() > 0);
            if let Some(gate) = gates.get(&identity).and_then(Weak::upgrade) {
                gate
            } else {
                let gate = Arc::new(tokio::sync::Mutex::new(()));
                gates.insert(identity, Arc::downgrade(&gate));
                gate
            }
        };
        // Never wait on the same-thread queue while holding shared admission:
        // a queued exclusive mutation must be able to settle first.
        let started = wait_start();
        let thread = gate.lock_owned().await;
        record_wait("resume_identity", started);
        ThreadResumePermit {
            state: Arc::clone(&self.state),
            _state: Some(self.acquire_publication().await),
            _thread: thread,
        }
    }
}
