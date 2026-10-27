//! Root-turn validation launch ownership.

use std::collections::HashMap;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;


/// Shared launch ownership and duplicate suppression for one root turn.
#[derive(Debug, Default)]
pub struct FullSuiteBudget(
    Mutex<HashMap<String, SuiteReservation>>,
    tokio::sync::Mutex<()>,
    Mutex<CompilationChanges>,
);

#[derive(Debug, Default)]
struct CompilationChanges {
    revision: u64,
    paths: BTreeMap<(String, PathBuf), u64>,
}

/// Positive input evidence, deliberately independent of observation epochs.
/// Kept inside the existing opaque checkpoint revision string.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CompilationInputs {
    compilation_inputs_v1: String,
    edit_revision: u64,
    fingerprints: BTreeMap<PathBuf, String>,
    #[serde(default)]
    closed_inputs: bool,
}

fn compilation_inputs_changed(previous: &str, current: &str) -> bool {
    let Ok(current) = serde_json::from_str::<CompilationInputs>(current) else { return false; };
    let Ok(previous) = serde_json::from_str::<CompilationInputs>(previous) else {
        // A legacy epoch-only checkpoint has no source baseline. Only a new
        // exact edit observed by this budget can establish repair inputs.
        return current.edit_revision > 0;
    };
    if current.compilation_inputs_v1 == previous.compilation_inputs_v1
        && current.closed_inputs && previous.closed_inputs {
        return current.fingerprints != previous.fingerprints;
    }
    if current.compilation_inputs_v1 == previous.compilation_inputs_v1
        && !current.fingerprints.is_empty() && current.fingerprints == previous.fingerprints {
        return false;
    }
    current.compilation_inputs_v1 != previous.compilation_inputs_v1
        || current.edit_revision > previous.edit_revision
        || current.fingerprints.iter().any(|(path, fingerprint)|
            previous.fingerprints.get(path).is_some_and(|before| before != fingerprint))
}

#[derive(Debug)]
struct SuiteReservation {
    generation: Arc<()>,
    started: bool,
    policy: Option<(String, String)>,
    compilation_only: bool,
}

/// Compilation kind travels inside the opaque selection, preserving the
/// existing (selection, revision) checkpoint wire shape. Legacy entries remain
/// non-compilation identities; compilation repairs use stricter freshness checks.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedCompilationSelection {
    compilation_selection_v1: String,
}

/// Dropping a prelaunch reservation releases that call's reservation only.
pub struct FullSuitePermit {
    budget: Arc<FullSuiteBudget>,
    owner: String,
    generation: Arc<()>,
}

const DUPLICATE_CALL: &str = "Full-suite launch blocked: this tool call already reserved or crossed the possible-start boundary; resume its live run instead of launching it twice.";

impl FullSuiteBudget {
    /// Exact patch effects only; unknown writers and command completion are
    /// not evidence that a compilation input changed. No child work holds this
    /// lock, and no input-evidence lock is nested with the reservation lock.
    pub fn record_compilation_input_changes(&self, environment: &str, paths: &[PathBuf]) {
        if paths.is_empty() { return; }
        let mut changes = self.2.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        changes.revision = changes.revision.saturating_add(1);
        let revision = changes.revision;
        for path in paths {
            changes.paths.insert((environment.to_owned(), path.clone()), revision);
        }
    }

    pub fn compilation_revision(
        &self, scope: &str, environment: &str, relevant_inputs: &[PathBuf],
        closed_inputs: bool, fingerprints: BTreeMap<PathBuf, String>,
    ) -> String {
        let changes = self.2.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let edit_revision = changes.paths.iter().filter(|((owner, path), _)|
            owner == environment && relevant_inputs.contains(path))
            .map(|(_, revision)| *revision).max().unwrap_or(0);
        serde_json::to_string(&CompilationInputs {
            compilation_inputs_v1: scope.to_owned(), edit_revision,
            closed_inputs: closed_inputs && relevant_inputs.len() == fingerprints.len(),
            fingerprints,
        }).unwrap_or_default()
    }

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
    /// release their identity; only the runtime owner can mark possible execution.
    /// There is no attempt cap. Compilation repairs require changed inputs.
    pub fn reserve_policy(
        self: &Arc<Self>,
        owner: String,
        selection: String,
        source_revision: String,
        compilation_only: bool,
    ) -> Result<FullSuitePermit, String> {
        let mut state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.contains_key(&owner) {
            return Err(DUPLICATE_CALL.into());
        }
        let attempts = state.values().filter_map(|entry|
            entry.policy.as_ref().map(|attempt| (entry.compilation_only, attempt))
        ).collect::<Vec<_>>();
        if attempts.iter().any(|(previous_compilation, (previous_selection, revision))|
            *previous_compilation == compilation_only
                && if compilation_only { !compilation_inputs_changed(revision, &source_revision) } else { revision == &source_revision }
                && (compilation_only || previous_selection == &selection)) {
            return Err("validation already admitted and no relevant input change was established; retain its result or resume its live process. Unsupported or unavailable input evidence is not proof of unchanged source.".into());
        }
        let generation = Arc::new(());
        state.insert(owner.clone(), SuiteReservation {
            generation: Arc::clone(&generation), started: false,
            policy: Some((selection, source_revision)),
            compilation_only,
        });
        Ok(FullSuitePermit { budget: Arc::clone(self), owner, generation })
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
        // Seed the exact-edit clock before any new repair. Restored paths are
        // fingerprint candidates, never fresh change events by themselves.
        let restored_revision = attempts.iter().filter_map(|(_, revision)|
            serde_json::from_str::<CompilationInputs>(revision).ok().map(|inputs| inputs.edit_revision))
            .max().unwrap_or(0);
        {
            let mut changes = self.2.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            changes.revision = changes.revision.max(restored_revision);
        }
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


