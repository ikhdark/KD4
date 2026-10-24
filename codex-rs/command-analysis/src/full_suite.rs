//! Root-turn validation mode budgets.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;


/// Shared launch ownership and validation allowance for one root turn.
#[derive(Debug, Default)]
pub struct FullSuiteBudget(
    Mutex<HashMap<String, SuiteReservation>>,
    tokio::sync::Mutex<()>,
);

#[derive(Debug)]
struct SuiteReservation {
    generation: Arc<()>,
    started: bool,
    policy: Option<(String, String)>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ValidationPolicySnapshot {
    pub admitted: usize,
    pub exhausted: bool,
}

/// Dropping a prelaunch reservation releases that call's reservation only.
pub struct FullSuitePermit {
    budget: Arc<FullSuiteBudget>,
    owner: String,
    generation: Arc<()>,
}

const DUPLICATE_CALL: &str = "Full-suite launch blocked: this tool call already reserved or crossed the possible-start boundary; resume its live run instead of launching it twice.";

impl FullSuiteBudget {
    /// Serialize only admission checkpoint publication, never the child run.
    pub async fn checkpoint_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.1.lock().await
    }

    pub fn reserve(
        self: &Arc<Self>,
        full_suite: bool,
        owner: String,
    ) -> Result<Option<FullSuitePermit>, String> {
        if !full_suite {
            return Ok(None);
        }
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.contains_key(&owner) {
            return Err(DUPLICATE_CALL.into());
        }
        let generation = Arc::new(());
        state.insert(
            owner.clone(),
            SuiteReservation {
                generation: Arc::clone(&generation),
                started: false,
                policy: None,
            },
        );
        Ok(Some(FullSuitePermit {
            budget: Arc::clone(self),
            owner,
            generation,
        }))
    }

    /// Admission is atomic with in-flight reservations. Dropped prelaunch calls
    /// refund their slot; only the runtime owner can mark possible execution.
    pub fn reserve_policy(
        self: &Arc<Self>,
        owner: String,
        selection: String,
        source_revision: String,
        limit: usize,
        require_changed_revision: bool,
    ) -> Result<FullSuitePermit, String> {
        let mut state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.contains_key(&owner) {
            return Err(DUPLICATE_CALL.into());
        }
        let attempts = state.values().filter_map(|entry| entry.policy.as_ref()).collect::<Vec<_>>();
        if attempts.len() >= limit {
            return Err("validation execution budget exhausted; report remaining diagnostics without another validation run".into());
        }
        if attempts.iter().any(|(previous_selection, revision)|
            revision == &source_revision && (require_changed_revision || previous_selection == &selection)) {
            return Err("validation already admitted against unchanged inputs; retain its result or resume its live process, not another launch".into());
        }
        let generation = Arc::new(());
        state.insert(owner.clone(), SuiteReservation {
            generation: Arc::clone(&generation), started: false,
            policy: Some((selection, source_revision)),
        });
        Ok(FullSuitePermit { budget: Arc::clone(self), owner, generation })
    }

    pub fn policy_snapshot(&self, limit: usize) -> ValidationPolicySnapshot {
        let state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let admitted = state.values().filter(|entry| entry.started && entry.policy.is_some()).count();
        ValidationPolicySnapshot { admitted, exhausted: admitted >= limit }
    }

    /// These are admission facts, never passing validation evidence.
    pub fn persisted_attempts(&self) -> Vec<(String, String)> {
        let mut attempts = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .values().filter(|entry| entry.started).filter_map(|entry| entry.policy.clone())
            .collect::<Vec<_>>();
        attempts.sort();
        attempts
    }

    pub fn restore_policy_attempts(&self, attempts: &[(String, String)]) {
        let mut state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for (index, attempt) in attempts.iter().enumerate() {
            if state.values().any(|entry| entry.policy.as_ref() == Some(attempt)) { continue; }
            state.insert(format!("recovered-policy:{index}:{}", attempt.0), SuiteReservation {
                generation: Arc::new(()), started: true,
                policy: Some(attempt.clone()),
            });
        }
    }


    pub fn reserved_for(&self, owner: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(owner)
            .is_some_and(|state| !state.started)
    }

    pub fn started_for(&self, owner: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(owner)
            .is_some_and(|state| state.started)
    }

    /// Only the launch owner may call this after proving no process was created.
    pub fn acknowledge_not_spawned(&self, owner: &str) {
        if let Some(entry) = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(owner) {
            // Keep the reservation while its handler is alive. A sandbox retry
            // must cross start again, never fall through an absent reservation.
            entry.started = false;
        }
    }

    pub fn start(
        &self,
        owner: &str,
        _acknowledgement: Option<(PathBuf, String)>,
    ) -> Result<(), String> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = state.get_mut(owner).filter(|state| !state.started) else {
            return Err(DUPLICATE_CALL.into());
        };
        state.started = true;
        Ok(())
    }
}

impl Drop for FullSuitePermit {
    fn drop(&mut self) {
        let mut state = self
            .budget
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.get(&self.owner).is_some_and(|state| {
            !state.started && Arc::ptr_eq(&state.generation, &self.generation)
        }) {
            state.remove(&self.owner);
        }
    }
}

