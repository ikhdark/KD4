//! Ordinary completion reuses authenticated focused execution, never a canonical certificate.
//! All records below live inside the existing MAC-protected runtime state. There is no
//! public receipt importer and a caller cannot create these records by writing JSON.

use super::*;

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
    mutation_epoch: u64,
    pub(super) input_contract: ValidationInputContract,
    pub(super) input_snapshot: ValidationInputSnapshotV1,
    pub(super) report: ValidationAttemptReport,
    #[serde(default)]
    pub(super) inputs: super::test_quality::QualityExecutionInputs,
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
        self.cover_with_quality_evidence(
            repository_root,
            bundle_sha256,
            workspace_fingerprint,
            &mut uncovered,
        )
        .await?;
        for (id, pass) in &self.passes {
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
            uncovered.retain(|path| !patterns.iter().any(|pattern| pattern.matches(path)));
            outstanding_failures.remove(id);
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
            mutation_epoch: pending.start_mutation_epoch,
            input_contract: contract.clone(),
            input_snapshot,
            report: validation.clone(),
            inputs,
        };
        if validation.classification == ValidationClassification::ConfirmedValidationFailure {
            state
                .persistent
                .focused_completion
                .revoke_failed_quality(validation);

            state
                .persistent
                .focused_completion
                .failures
                .insert(validation.id.clone(), execution);
        } else {
            state
                .persistent
                .focused_completion
                .passes
                .insert(validation.id.clone(), execution);
        }
        drop(state);
        self.persist().await
    }
}
