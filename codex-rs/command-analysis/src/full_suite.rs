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
    compilation_only: bool,
}

/// Compilation kind travels inside the opaque selection, preserving the
/// existing (selection, revision) checkpoint wire shape. Legacy entries remain
/// budgeted; new compilation repairs never consume runtime/tooling attempts.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedCompilationSelection {
    compilation_selection_v1: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ValidationPolicySnapshot {
    /// Started, budgeted runtime/tooling attempts; excludes compilation.
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
                compilation_only: false,
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
    /// Compilation repairs have no count limit, but require changed inputs.
    pub fn reserve_policy(
        self: &Arc<Self>,
        owner: String,
        selection: String,
        source_revision: String,
        limit: usize,
        compilation_only: bool,
    ) -> Result<FullSuitePermit, String> {
        let mut state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.contains_key(&owner) {
            return Err(DUPLICATE_CALL.into());
        }
        let attempts = state.values().filter_map(|entry|
            entry.policy.as_ref().map(|attempt| (entry.compilation_only, attempt))
        ).collect::<Vec<_>>();
        if !compilation_only && attempts.iter().filter(|attempt| !attempt.0).count() >= limit {
            return Err("runtime/tooling validation execution budget exhausted; no further budgeted validation may run; authorized compilation repairs remain available after relevant input changes".into());
        }
        if attempts.iter().any(|(previous_compilation, (previous_selection, revision))|
            *previous_compilation == compilation_only && revision == &source_revision
                && (compilation_only || previous_selection == &selection)) {
            return Err("validation already admitted against unchanged inputs; retain its result or resume its live process, not another launch".into());
        }
        let generation = Arc::new(());
        state.insert(owner.clone(), SuiteReservation {
            generation: Arc::clone(&generation), started: false,
            policy: Some((selection, source_revision)),
            compilation_only,
        });
        Ok(FullSuitePermit { budget: Arc::clone(self), owner, generation })
    }

    pub fn policy_snapshot(&self, limit: usize) -> ValidationPolicySnapshot {
        let state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let admitted = state.values().filter(|entry|
            entry.started && entry.policy.is_some() && !entry.compilation_only
        ).count();
        ValidationPolicySnapshot { admitted, exhausted: admitted >= limit }
    }

    /// These are admission facts, never passing validation evidence.
    pub fn persisted_attempts(&self) -> Vec<(String, String)> {
        let mut attempts = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .values().filter(|entry| entry.started).filter_map(|entry| {
                let (selection, revision) = entry.policy.as_ref()?;
                let selection = if entry.compilation_only {
                    serde_json::json!({"compilation_selection_v1": selection}).to_string()
                } else {
                    selection.clone()
                };
                Some((selection, revision.clone()))
            })
            .collect::<Vec<_>>();
        attempts.sort();
        attempts
    }

    pub fn restore_policy_attempts(&self, attempts: &[(String, String)]) {
        let mut state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for attempt in attempts {
            let compilation = serde_json::from_str::<PersistedCompilationSelection>(&attempt.0).ok();
            let compilation_only = compilation.is_some();
            let policy = (
                compilation.map(|value| value.compilation_selection_v1).unwrap_or_else(|| attempt.0.clone()),
                attempt.1.clone(),
            );
            if state.values().any(|entry|
                entry.compilation_only == compilation_only && entry.policy.as_ref() == Some(&policy)
            ) { continue; }
            state.insert(format!("recovered-policy:{}", serde_json::json!(attempt)), SuiteReservation {
                generation: Arc::new(()), started: true,
                policy: Some(policy), compilation_only,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn admit_started(budget: &Arc<FullSuiteBudget>, owner: &str, revision: &str, compilation: bool, limit: usize) {
        let selection = if compilation { "cargo check -p example" } else { "cargo bench -p example" };
        let permit = budget.reserve_policy(owner.into(), selection.into(), revision.into(), limit, compilation).unwrap();
        budget.start(owner, None).unwrap();
        drop(permit);
    }

    #[test]
    fn compilation_repairs_are_uncapped_but_runtime_attempts_remain_bounded() {
        let budget = Arc::new(FullSuiteBudget::default());
        for index in 0..5 {
            admit_started(&budget, &format!("compile-{index}"), &format!("source-{index}"), true, 0);
        }
        assert_eq!(budget.policy_snapshot(2).admitted, 0);
        assert!(!budget.policy_snapshot(2).exhausted);
        // A compile at this revision must not block the first runtime check.
        admit_started(&budget, "runtime-one", "source-4", false, 2);
        admit_started(&budget, "runtime-two", "source-5", false, 2);
        assert!(budget.reserve_policy("runtime-three".into(), "another benchmark".into(), "source-6".into(), 2, false).is_err());
        admit_started(&budget, "compile-after-runtime-exhaustion", "source-6", true, 0);
        assert!(budget.reserve_policy("unchanged".into(), "another compile selection".into(), "source-6".into(), 0, true).is_err());
        assert_eq!(budget.policy_snapshot(2).admitted, 2);
        assert!(budget.policy_snapshot(2).exhausted);
        assert_eq!(budget.persisted_attempts().len(), 8);
    }

    #[test]
    fn checkpoint_round_trip_preserves_uncapped_compilation_and_legacy_runtime_budgeting() {
        let budget = Arc::new(FullSuiteBudget::default());
        admit_started(&budget, "compile", "source-0", true, 0);
        admit_started(&budget, "runtime", "source-0", false, 2);
        let checkpoint: Vec<(String, String)> = serde_json::from_value(
            serde_json::to_value(budget.persisted_attempts()).unwrap(),
        ).unwrap();
        let restored = Arc::new(FullSuiteBudget::default());
        restored.restore_policy_attempts(&checkpoint);
        restored.restore_policy_attempts(&checkpoint);
        assert_eq!(restored.persisted_attempts(), checkpoint);
        assert_eq!(restored.policy_snapshot(2).admitted, 1);
        assert!(restored.reserve_policy("unchanged".into(), "different compile".into(), "source-0".into(), 0, true).is_err());
        admit_started(&restored, "repaired", "source-1", true, 0);
        let legacy = ("legacy-selection".into(), "legacy-revision".into());
        restored.restore_policy_attempts(&[legacy]);
        restored.restore_policy_attempts(&checkpoint);
        assert_eq!(restored.policy_snapshot(2).admitted, 2);
        assert_eq!(restored.persisted_attempts().len(), 4);
    }

    #[test]
    fn unstarted_compilation_refunds_without_allowing_duplicate_live_owners() {
        let budget = Arc::new(FullSuiteBudget::default());
        let permit = budget.reserve_policy("compile".into(), "check".into(), "source".into(), 0, true).unwrap();
        assert!(budget.reserve_policy("compile".into(), "check".into(), "source".into(), 0, true).is_err());
        drop(permit);
        assert!(budget.persisted_attempts().is_empty());
        admit_started(&budget, "retry", "source", true, 0);
        assert_eq!(budget.persisted_attempts().len(), 1);
        assert_eq!(budget.policy_snapshot(2).admitted, 0);
    }
}

