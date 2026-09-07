//! Ordinary completion reuses authenticated focused execution, never a canonical certificate.
//! All records below live inside the existing MAC-protected runtime state. There is no
//! public receipt importer and a caller cannot create these records by writing JSON.

use super::*;

// The approved prospective quality workflow entered KD4 at s12. This is a
// fixed source-history boundary, never a caller-selected or current-HEAD reset.
const KD4_QUALITY_INTRODUCTION: &str = "5c242c418a66f3aa5d5e187ae4d1ee639c8c0918";

pub(super) async fn adopt_kd4_quality_boundary(
    root: &Path,
    state: &mut PersistentCompletionProofState,
    observation: &WorkspaceObservationSnapshot,
) {
    adopt_legacy_quality_boundary(root, state, KD4_QUALITY_INTRODUCTION, Some(observation)).await;
}

async fn adopt_legacy_quality_boundary(
    root: &Path,
    state: &mut PersistentCompletionProofState,
    introduction: &str,
    observation: Option<&WorkspaceObservationSnapshot>,
) {
    let Some(observation) = observation else {
        return;
    };
    if state.last_observed_fingerprint.as_deref() != Some(observation.fingerprint.as_str())
        || state.last_observed_head_identity != observation.head_identity
        || state.last_observed_path_fingerprints != observation.path_fingerprints
    {
        return;
    }
    let focused = &state.focused_completion;
    let (Some(previous), Some(current)) = (
        focused.baseline_head.as_deref(),
        state.last_observed_head_identity.as_deref(),
    ) else {
        return;
    };
    if previous == introduction
        || !focused.passes.is_empty()
        || !focused.failures.is_empty()
        || !focused.quality.is_empty()
    {
        return;
    }
    // Missing history, a divergent branch, or a post-introduction baseline
    // cannot authorize removing any pending obligation.
    for (ancestor, descendant) in [(previous, introduction), (introduction, current)] {
        if super::test_quality::quality_git(
            root,
            &["merge-base", "--is-ancestor", ancestor, descendant],
        )
        .await
        .is_err()
        {
            return;
        }
    }
    let Some(committed) =
        changed_paths_between_heads(root, Some(introduction), Some(current)).await
    else {
        return;
    };
    let paths = committed
        .into_iter()
        .chain(observation.path_fingerprints.keys().cloned())
        .filter(|path| !is_documentation(path))
        .collect();
    state.focused_completion.baseline_head = Some(introduction.to_owned());
    state.focused_completion.pending_paths = paths;
    // Legacy files predate coverage_known. A fresh complete observation and the
    // fixed introduction-to-HEAD diff reconstruct the new scope without guessing.
    state.focused_completion.coverage_known = true;
    // All other state, particularly confirmed-failure poison, is preserved.
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FocusedCompletionState {
    #[serde(default)]
    pub(super) pending_paths: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) baseline_head: Option<String>,
    #[serde(default)]
    pub(super) coverage_known: bool,
    #[serde(default)]
    pub(super) passes: BTreeMap<String, ScopedFocusedPass>,
    #[serde(default)]
    pub(super) certification_requests: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) failures: BTreeMap<String, ScopedFocusedPass>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) quality: Vec<super::test_quality::TestQualityEvidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ScopedFocusedPass {
    pub(super) attempt_id: String,
    invocation_nonce: String,
    session_lineage_id: String,
    pub(super) policy_runner_bundle_sha256: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) trusted_bundle_hashes: BTreeMap<String, String>,
    pub(super) mutation_epoch: u64,
    pub(super) input_contract: ValidationInputContract,
    pub(super) input_snapshot: ValidationInputSnapshotV1,
    pub(super) report: ValidationAttemptReport,
    #[serde(default)]
    pub(super) inputs: super::test_quality::QualityExecutionInputs,
    // An exact subset proves execution, not the configured group's entire
    // product scope. Only independently reviewed dependencies can supply that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) narrow_selection: Option<Vec<String>>,
}

impl FocusedCompletionState {
    // Preserve the MAC payload of states written before scoped completion existed.
    pub(super) fn is_legacy_default(&self) -> bool {
        !self.coverage_known
            && self.pending_paths.is_empty()
            && self.baseline_head.is_none()
            && self.passes.is_empty()
            && self.certification_requests.is_empty()
            && self.failures.is_empty()
            && self.quality.is_empty()
    }

    pub(super) fn new() -> Self {
        Self {
            coverage_known: true,
            ..Self::default()
        }
    }

    pub(super) fn observe_changes(&mut self, paths: Option<&[String]>) {
        match paths {
            Some(paths) => self
                .pending_paths
                .extend(paths.iter().filter(|path| !is_documentation(path)).cloned()),
            None => self.coverage_known = false,
        }
    }

    pub(super) fn certification_requested(&self, lineage: Option<&str>) -> bool {
        lineage.is_some_and(|lineage| self.certification_requests.contains(lineage))
    }

    pub(super) fn completed(&mut self, lineage: Option<&str>) {
        self.pending_paths.clear();
        self.baseline_head = None;
        self.coverage_known = true;
        if let Some(lineage) = lineage {
            self.certification_requests.remove(lineage);
        }
    }

    /// A successful check covers only the inputs in its admitted contract. A pass in
    /// an unrelated directory, a listing, or migration bookkeeping cannot cover code.
    pub(super) async fn check(
        &self,
        repository_root: &Path,
        config: &RepositoryCompletionProofConfig,
        bundle_sha256: &str,
        workspace_fingerprint: &str,
        poisoned: &BTreeMap<String, PoisonedValidation>,
    ) -> Result<(), String> {
        if !self.coverage_known || self.pending_paths.is_empty() {
            return Err("the runtime has no complete scoped mutation record".to_owned());
        }
        let mut uncovered = self.pending_paths.clone();
        let mut outstanding_failures = poisoned.keys().cloned().collect::<BTreeSet<_>>();
        let mut outstanding_native = poisoned
            .iter()
            .map(|(id, failure)| (id.clone(), failure.failed_test_ids.clone()))
            .collect::<BTreeMap<_, _>>();
        self.cover_with_quality_evidence(
            repository_root,
            bundle_sha256,
            workspace_fingerprint,
            &mut uncovered,
        )
        .await?;
        for pass in self.passes.values() {
            let id = &pass.report.id;
            let Some(contract) = config.validation_path_patterns.get(id) else {
                continue;
            };
            let Some(evidence_contract) = config.validation_evidence_contracts.get(id) else {
                continue;
            };
            if pass.policy_runner_bundle_sha256 != bundle_sha256
                || pass.input_contract != *contract
                || !validation_evidence_contract_matches(&pass.report, evidence_contract)
                || pass.report.classification != ValidationClassification::ConfirmedPass
                || !matches!(
                    pass.report.evidence_kind,
                    ValidationEvidenceKind::StructuredTest | ValidationEvidenceKind::TypedNonTest
                )
                || poisoned
                    .get(id)
                    .is_some_and(|failure| pass.mutation_epoch <= failure.failed_at_mutation_epoch)
            {
                continue;
            }
            let patterns = contract
                .all_declared_patterns()
                .iter()
                .map(|pattern| glob::Pattern::new(&normalized_validation_pattern(pattern)))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("invalid focused input pattern: {error}"))?;
            if !uncovered
                .iter()
                .any(|path| patterns.iter().any(|pattern| pattern.matches(path)))
                && !outstanding_failures.contains(id)
            {
                continue;
            }
            let (observation, snapshot) =
                stable_validation_input_snapshot(repository_root, contract).await;
            if observation.as_ref().map(|value| value.fingerprint.as_str())
                != Some(workspace_fingerprint)
            {
                return Err("the workspace changed while checking focused evidence".to_owned());
            }
            if snapshot.as_ref() != Some(&pass.input_snapshot) {
                continue;
            }
            if pass.narrow_selection.is_none() {
                uncovered.retain(|path| !patterns.iter().any(|pattern| pattern.matches(path)));
            }
            if let Some(remaining) = outstanding_native.get_mut(id) {
                if poisoned[id].failed_test_ids.is_empty() {
                    if pass.narrow_selection.is_none() {
                        outstanding_failures.remove(id);
                    }
                } else {
                    remaining.retain(|test| !pass.report.executed_ids.contains(test));
                    if remaining.is_empty() {
                        outstanding_failures.remove(id);
                    }
                }
            }
        }
        if !outstanding_failures.is_empty() {
            return Err(format!(
                "confirmed failures still need a relevant correction and fresh focused pass: {}",
                outstanding_failures
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !uncovered.is_empty() {
            return Err(format!(
                "changed inputs still need current focused validation: {}",
                uncovered
                    .iter()
                    .take(12)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(())
    }
}

impl CompletionProofLedger {
    /// Called only after the existing exact-command, private-nonce, OS-process,
    /// nonzero-result, workspace-stability, and failure-poison checks have passed.
    pub(super) async fn record_scoped_focused_pass(
        &self,
        pending: &PendingAttempt,
        validation: &ValidationAttemptReport,
        narrow_selection: bool,
    ) -> Result<(), String> {
        if !matches!(
            validation.evidence_kind,
            ValidationEvidenceKind::StructuredTest | ValidationEvidenceKind::TypedNonTest
        ) {
            return Ok(());
        }
        let contract = pending
            .validation_path_patterns
            .get(&validation.id)
            .ok_or_else(|| "focused validation has no admitted input contract".to_owned())?;
        let authority = self.verified_authority().await?;
        if authority.policy_runner_bundle_sha256 != pending.policy_runner_bundle_sha256 {
            return Err("focused authority changed before retaining execution".to_owned());
        }
        let (observation, snapshot) = stable_validation_input_snapshots(
            &self.repository_root,
            &BTreeMap::from([(validation.id.clone(), contract.clone())]),
        )
        .await;
        if observation.as_ref().map(|value| value.fingerprint.as_str())
            != Some(pending.start_fingerprint.as_str())
        {
            return Err("focused inputs changed before their pass could be retained".to_owned());
        }
        let input_snapshot = snapshot
            .and_then(|mut values| values.remove(&validation.id))
            .ok_or_else(|| "focused inputs could not be captured stably".to_owned())?;
        let lineage = self
            .session_lineage_id
            .as_ref()
            .ok_or_else(|| "focused execution has no runtime session lineage".to_owned())?;
        let focused = self
            .state
            .lock()
            .await
            .persistent
            .focused_completion
            .clone();
        let quality_paths = focused
            .required_quality_paths(&self.repository_root)
            .await?;
        let mut paths = focused.pending_paths.clone();
        paths.extend(quality_paths.iter().cloned());
        paths.extend(
            authority
                .trusted_bundle_hashes
                .keys()
                .filter(|path| path.ends_with(".py"))
                .cloned(),
        );
        let inputs =
            super::test_quality::capture_execution_inputs(&self.repository_root, contract, &paths)
                .await?;
        if workspace_observation(&self.repository_root)
            .await
            .as_ref()
            .map(|value| value.fingerprint.as_str())
            != Some(pending.start_fingerprint.as_str())
        {
            return Err("focused inputs changed while capturing quality evidence".to_owned());
        }
        let mut state = self.state.lock().await;
        if state.persistent.mutation_epoch != pending.start_mutation_epoch {
            return Err("the mutation epoch changed before retaining focused execution".to_owned());
        }
        // Keep established tests in scope when a later product regression
        // revokes their review, even if their source bytes did not change.
        state
            .persistent
            .focused_completion
            .pending_paths
            .extend(quality_paths);
        let execution = ScopedFocusedPass {
            attempt_id: pending.attempt_id.clone(),
            invocation_nonce: pending.nonce.clone(),
            session_lineage_id: lineage.clone(),
            policy_runner_bundle_sha256: pending.policy_runner_bundle_sha256.clone(),
            trusted_bundle_hashes: authority.trusted_bundle_hashes,
            mutation_epoch: pending.start_mutation_epoch,
            input_contract: contract.clone(),
            input_snapshot,
            report: validation.clone(),
            inputs,
            narrow_selection: narrow_selection.then(|| validation.selected_ids.clone()),
        };
        if validation.classification == ValidationClassification::ConfirmedValidationFailure {
            state
                .persistent
                .focused_completion
                .revoke_failed_quality(validation);

            retain_execution(&mut state.persistent.focused_completion.failures, execution);
        } else {
            retain_execution(&mut state.persistent.focused_completion.passes, execution);
        }
        drop(state);
        self.persist().await
    }
}

fn retain_execution(
    executions: &mut BTreeMap<String, ScopedFocusedPass>,
    execution: ScopedFocusedPass,
) {
    // Keep the latest validation alias for old diagnostics, and retain every
    // previous private attempt for independently reviewed native subsets.
    if let Some(previous) = executions.insert(execution.report.id.clone(), execution) {
        executions.insert(format!("attempt:{}", previous.attempt_id), previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(root)
            .output()
            .expect("git fixture command");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git output")
            .trim()
            .to_owned()
    }

    fn commit(root: &Path, message: &str) -> String {
        git(root, &["add", "."]);
        git(root, &["commit", "--quiet", "-m", message]);
        git(root, &["rev-parse", "HEAD"])
    }

    #[tokio::test]
    async fn initial_quality_adoption_keeps_post_boundary_work_and_failure_poison() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let root = directory.path();
        git(root, &["init", "--quiet"]);
        std::fs::write(
            root.join("test_legacy.py"),
            "def test_old(): assert 1 == 1\n",
        )
        .expect("old source");
        let original = commit(root, "old runtime");
        let mut state = PersistentCompletionProofState::new(root);
        reconcile_external_workspace_change(root, &mut state, workspace_observation(root).await)
            .await;
        assert_eq!(
            state.focused_completion.baseline_head.as_deref(),
            Some(original.as_str())
        );
        std::fs::write(
            root.join("test_legacy.py"),
            "def test_old(): assert 2 == 2\n",
        )
        .expect("completed legacy work");
        std::fs::write(
            root.join("quality_runtime.rs"),
            "// initial quality implementation\n",
        )
        .expect("introduction source");
        let introduction = commit(root, "quality introduction");
        std::fs::write(
            root.join("test_committed_later.py"),
            "def test_new(): assert True\n",
        )
        .expect("new committed test");
        commit(root, "later work must remain in scope");
        std::fs::write(
            root.join("test_untracked.py"),
            "def test_dirty(): assert True\n",
        )
        .expect("new untracked test");
        reconcile_external_workspace_change(root, &mut state, workspace_observation(root).await)
            .await;
        assert!(
            state
                .focused_completion
                .pending_paths
                .contains("test_legacy.py")
        );
        state.poisoned_validations.insert(
            "legacy.failure".to_owned(),
            PoisonedValidation {
                validation_id: "legacy.failure".to_owned(),
                failed_test_ids: BTreeSet::from(["legacy::test_failure".to_owned()]),
                failed_at_mutation_epoch: 1,
                relevant_path_patterns: BTreeSet::from(["test_legacy.py".to_owned()]),
                relevant_input_contract: None,
                failure_input_snapshot: None,
            },
        );
        let poison = serde_json::to_value(&state.poisoned_validations).expect("poison snapshot");
        let epoch = state.mutation_epoch;
        state.focused_completion.coverage_known = false; // Actual pre-system serialized default.
        let observation = workspace_observation(root)
            .await
            .expect("fresh adoption snapshot");
        adopt_legacy_quality_boundary(root, &mut state, &introduction, Some(&observation)).await;
        assert!(state.focused_completion.coverage_known);
        assert_eq!(
            state.focused_completion.baseline_head.as_deref(),
            Some(introduction.as_str())
        );
        assert_eq!(
            state.focused_completion.pending_paths,
            BTreeSet::from([
                "test_committed_later.py".to_owned(),
                "test_untracked.py".to_owned(),
            ])
        );
        assert_eq!(
            serde_json::to_value(&state.poisoned_validations).expect("preserved poison"),
            poison
        );
        assert_eq!(state.mutation_epoch, epoch);
        state
            .focused_completion
            .pending_paths
            .insert("later-retained-obligation".to_owned());
        let once = serde_json::to_value(&state).expect("state after adoption");
        adopt_legacy_quality_boundary(root, &mut state, &introduction, Some(&observation)).await;
        assert_eq!(
            serde_json::to_value(&state).expect("state after second load"),
            once
        );
    }

    #[tokio::test]
    async fn initial_quality_adoption_rejects_unknown_history_and_incomplete_observation() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let root = directory.path();
        git(root, &["init", "--quiet"]);
        std::fs::write(
            root.join("test_existing.py"),
            "def test_existing(): assert True\n",
        )
        .expect("existing source");
        let original = commit(root, "original");
        std::fs::write(root.join("runtime.rs"), "// quality introduction\n").expect("new source");
        let introduction = commit(root, "introduction");
        let mut state = PersistentCompletionProofState::new(root);
        let observation = workspace_observation(root)
            .await
            .expect("fresh repository observation");
        reconcile_external_workspace_change(root, &mut state, Some(observation.clone())).await;
        state.focused_completion.baseline_head = Some(original);
        state
            .focused_completion
            .pending_paths
            .insert("test_existing.py".to_owned());
        state.last_observed_head_identity = Some(introduction.clone());
        let before = serde_json::to_value(&state).expect("original state");
        adopt_legacy_quality_boundary(root, &mut state, &"1".repeat(40), Some(&observation)).await;
        assert_eq!(
            serde_json::to_value(&state).expect("unknown boundary state"),
            before
        );
        state.focused_completion.coverage_known = false;
        let before = serde_json::to_value(&state).expect("incomplete observation state");
        adopt_legacy_quality_boundary(root, &mut state, &introduction, None).await;
        assert_eq!(
            serde_json::to_value(&state).expect("preserved incomplete state"),
            before
        );
        // A new checkout with no recorded baseline cannot choose a historical
        // introduction as a way to bless its pre-existing dirty files.
        state.focused_completion.coverage_known = true;
        state.focused_completion.baseline_head = None;
        let before = serde_json::to_value(&state).expect("no baseline state");
        adopt_legacy_quality_boundary(root, &mut state, &introduction, Some(&observation)).await;
        assert_eq!(
            serde_json::to_value(&state).expect("preserved unadmitted state"),
            before
        );
    }
}
