use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use tokio::sync::watch;

/// Coordinates user elicitations that pause tool-result delivery for a session.
///
/// Registrations are counted so concurrent elicitations keep the session paused until all of them
/// finish. Consumers can subscribe to pause timeout progress or wait before returning an already
/// captured result.
#[derive(Clone)]
pub(crate) struct ElicitationService {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<ServiceState>,
    paused: watch::Sender<bool>,
}

#[derive(Default)]
struct ServiceState {
    outstanding: i64,
}

pub(crate) struct ElicitationRegistration {
    service: ElicitationService,
}

/// Exact identity of one app-server-owned out-of-band elicitation lease.
///
/// The owner is the stable transport connection id and `lease_id` is the server-issued token for
/// the lease. The target thread is implicit in the owning [`OutOfBandElicitationLeases`] instance.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OutOfBandElicitationLeaseId {
    owner_id: u64,
    lease_id: String,
}

impl OutOfBandElicitationLeaseId {
    pub fn new(owner_id: u64, lease_id: String) -> Self {
        Self { owner_id, lease_id }
    }
}

pub(crate) struct OutOfBandElicitationLeases {
    service: ElicitationService,
    state: Mutex<OutOfBandLeaseState>,
}

struct OutOfBandLeaseState {
    accepting: bool,
    registrations: HashMap<OutOfBandElicitationLeaseId, ElicitationRegistration>,
}

impl Default for OutOfBandLeaseState {
    fn default() -> Self {
        Self {
            accepting: true,
            registrations: HashMap::new(),
        }
    }
}

impl ElicitationService {
    pub(crate) fn new() -> Self {
        let (paused, _paused_rx) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(ServiceState::default()),
                paused,
            }),
        }
    }

    pub(crate) fn register(&self) -> ElicitationRegistration {
        self.increment();
        ElicitationRegistration {
            service: self.clone(),
        }
    }

    fn increment(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was_clear = state.outstanding == 0;
        assert_ne!(
            state.outstanding,
            i64::MAX,
            "outstanding elicitation count overflowed"
        );
        state.outstanding += 1;
        if was_clear {
            self.inner.paused.send_replace(true);
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<bool> {
        self.inner.paused.subscribe()
    }

    pub(crate) async fn wait_until_clear(&self) {
        let mut paused = self.subscribe();
        let _ = paused.wait_for(|paused| !*paused).await;
    }

    fn decrement(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            state.outstanding > 0,
            "elicitation registration count underflowed"
        );
        state.outstanding -= 1;
        if state.outstanding == 0 {
            self.inner.paused.send_replace(false);
        }
    }
}

impl OutOfBandElicitationLeases {
    pub(crate) fn new(service: ElicitationService) -> Self {
        Self {
            service,
            state: Mutex::new(OutOfBandLeaseState::default()),
        }
    }

    pub(crate) fn acquire(&self, lease_id: OutOfBandElicitationLeaseId) -> CodexResult<i64> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting {
            return Err(CodexErr::InvalidRequest(
                "thread is shutting down and cannot accept an elicitation lease".to_string(),
            ));
        }
        if state.registrations.contains_key(&lease_id) {
            return Err(CodexErr::InvalidRequest(
                "out-of-band elicitation lease already exists".to_string(),
            ));
        }

        state
            .registrations
            .insert(lease_id, self.service.register());
        Ok(lease_count(state.registrations.len()))
    }

    pub(crate) fn release(&self, lease_id: &OutOfBandElicitationLeaseId) -> i64 {
        let (registration, count) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let registration = state.registrations.remove(lease_id);
            let count = lease_count(state.registrations.len());
            (registration, count)
        };
        drop(registration);
        count
    }

    pub(crate) fn active_count(&self) -> i64 {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lease_count(state.registrations.len())
    }

    pub(crate) fn close(&self) {
        let registrations = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.accepting = false;
            std::mem::take(&mut state.registrations)
        };
        drop(registrations);
    }
}

fn lease_count(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

impl Drop for ElicitationRegistration {
    fn drop(&mut self) {
        self.service.decrement();
    }
}

#[cfg(test)]
#[path = "elicitation_tests.rs"]
mod tests;
