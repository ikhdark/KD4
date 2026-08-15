use crate::git_workspace::SourceFreshnessRegistration;
use codex_file_search::source_search::SourceSearchHydrationCandidate;
use codex_file_search::source_search::SourceSearchHydrationCandidateKind;
use codex_file_search::task_locator::LocateTaskDecisionFacts;
use codex_file_search::task_locator::LocateTaskSourceSectionState;
use codex_file_search::task_locator::ManifestOwnerProjection;
use codex_file_search::task_locator::OwnerCandidateResolution;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::watch;

const MAX_ALTERNATIVES: usize = 8;
const MAX_INSTRUCTIONS: usize = 16;
const MAX_TARGETS: usize = 64;
const MAX_OBSERVED_PATHS: usize = 64;
const MAX_INTERVALS: usize = 256;
const MAX_SEARCH_RECEIPTS: usize = 32;
const MAX_UNRESOLVED: usize = 16;
pub(crate) const SOURCE_CLOSURE_SUMMARY_TARGET_BYTES: usize = 2 * 1024;
pub(crate) const SOURCE_CLOSURE_SUMMARY_MAX_BYTES: usize = 8 * 1024;

pub(crate) type SharedSourceClosureState = Arc<Mutex<SourceClosureState>>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceQuestionKind {
    UnknownCaller,
    UnknownContract,
    AmbiguousOwnership,
    IncompletePriorResult,
    SourceChanged,
    ValidationDependency,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceQuestion {
    pub(crate) kind: SourceQuestionKind,
    pub(crate) detail: String,
}

impl SourceQuestion {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.detail.trim().is_empty() {
            Err("source_question.detail must be nonempty".to_string())
        } else {
            Ok(())
        }
    }

    fn summary(&self) -> String {
        format!("{:?}: {}", self.kind, self.detail.trim())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceClosureDisposition {
    #[default]
    Gathering,
    Established,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceClosureTargetSummary {
    pub(crate) path: String,
    pub(crate) role: String,
    pub(crate) established: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceClosureSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) authoritative_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) primary_implementation: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) relevant_targets: Vec<SourceClosureTargetSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) pending_required_targets: Vec<String>,
    pub(crate) validation: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) unresolved_questions: Vec<String>,
    pub(crate) discovery: SourceClosureDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reopen_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct SourceMetadataToken {
    pub(crate) size: u64,
    pub(crate) created_at_ms: i64,
    pub(crate) modified_at_ms: i64,
    pub(crate) is_symlink: bool,
    pub(crate) repository_identity: String,
    pub(crate) environment_identity: String,
    pub(crate) mutation_revision: u64,
    pub(crate) watcher_generation: u64,
    pub(crate) host_mutation_generation: u64,
    pub(crate) stable_file_identity: String,
    pub(crate) freshness_identity: String,
}

impl SourceMetadataToken {
    pub(crate) fn permits_reuse(&self, current: &Self) -> bool {
        !self.is_symlink
            && !current.is_symlink
            && self.created_at_ms > 0
            && self.modified_at_ms > 0
            && !self.stable_file_identity.is_empty()
            && self.size == current.size
            && self.created_at_ms == current.created_at_ms
            && self.modified_at_ms == current.modified_at_ms
            && self.repository_identity == current.repository_identity
            && self.environment_identity == current.environment_identity
            && self.mutation_revision == current.mutation_revision
            && self.stable_file_identity == current.stable_file_identity
            && self.freshness_identity == current.freshness_identity
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReadReceipt {
    pub(crate) path: String,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) metadata: SourceMetadataToken,
    pub(crate) content_hash: String,
    pub(crate) artifact_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchReceipt {
    pub(crate) key: String,
    pub(crate) scope_revision: String,
    pub(crate) artifact_id: String,
    pub(crate) capped_zero: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct GitObservationReceipt {
    pub(crate) freshness_key: String,
    pub(crate) identity: String,
    pub(crate) artifact_id: String,
    pub(crate) observed_paths: Vec<String>,
    pub(crate) freshness_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub(crate) struct SourceEvidenceRange {
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct AuthoritativeSourceBasis {
    pub(crate) path: String,
    pub(crate) file_content_hash: String,
    pub(crate) source_snapshot_identity: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ObligationIdentity {
    pub(crate) locator_request_identity: String,
    pub(crate) owner_id: String,
    pub(crate) task_contract_epoch: String,
    pub(crate) obligation_kind: String,
    pub(crate) required_ranges: Vec<SourceEvidenceRange>,
    pub(crate) reset_generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AdmittedSourceEvidenceBasis {
    pub(crate) source_basis: AuthoritativeSourceBasis,
    pub(crate) obligation_identity: ObligationIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthoritativeSourceIdentity {
    file_content_hash: String,
    obligation_kind: String,
    required_ranges: Vec<SourceEvidenceRange>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BoundSourceBasisSignature {
    source_snapshot_identity: String,
    locator_request_identity: String,
    owner_id: String,
    task_contract_epoch: String,
    source_identities: BTreeMap<String, AuthoritativeSourceIdentity>,
}

#[derive(Clone, Debug)]
struct ClosureTarget {
    path: String,
    role: String,
    required: bool,
    observed: bool,
}

#[derive(Debug)]
pub(crate) struct SourceReservation;

#[derive(Debug, Default)]
pub(crate) struct SourceClosureState {
    // A mention/read only enters `candidates`. `owner` is populated solely by
    // a validated manifest projection, including when locator facts are applied.
    candidates: Vec<String>,
    owner: Option<ManifestOwnerProjection>,
    alternatives: Vec<String>,
    instructions: BTreeMap<String, bool>,
    targets: BTreeMap<String, ClosureTarget>,
    observed_paths: BTreeSet<String>,
    read_receipts: VecDeque<ReadReceipt>,
    search_receipts: VecDeque<SearchReceipt>,
    unresolved: Vec<String>,
    locator_unresolved: BTreeSet<String>,
    capacity_loss: bool,
    validation_known: bool,
    validation_explicitly_unresolved: bool,
    disposition: SourceClosureDisposition,
    reopen_reason: Option<String>,
    pub(crate) locator_attempted: bool,
    locator_source_revision: u64,
    pub(crate) source_revision: u64,
    pub(crate) repository_identity: Option<String>,
    pub(crate) source_snapshot_identity: Option<String>,
    pub(crate) manifest_revision: Option<String>,
    pub(crate) locator_artifact_id: Option<String>,
    pub(crate) git_observation: Option<GitObservationReceipt>,
    authoritative_source_identities: BTreeMap<String, AuthoritativeSourceIdentity>,
    locator_request_identity: Option<String>,
    task_contract_epoch: Option<String>,
    evidence_basis_generation: u64,
    bound_source_basis: Option<BoundSourceBasisSignature>,
    source_basis_bound: bool,
    read_reservations: BTreeMap<String, watch::Sender<bool>>,
    search_reservations: BTreeMap<String, watch::Sender<bool>>,
    hydration_candidates: Vec<SourceSearchHydrationCandidate>,
    source_watch_registrations: BTreeMap<String, SourceFreshnessRegistration>,
}

impl SourceClosureState {
    pub(crate) fn shared() -> SharedSourceClosureState {
        Arc::new(Mutex::new(Self::default()))
    }

    pub(crate) fn add_candidates(&mut self, candidates: impl IntoIterator<Item = String>) {
        for candidate in candidates {
            let candidate = normalize_path(&candidate);
            if !candidate.is_empty() && !self.candidates.contains(&candidate) {
                self.candidates.push(candidate);
            }
        }
        if self.candidates.len() > MAX_OBSERVED_PATHS {
            self.candidates.truncate(MAX_OBSERVED_PATHS);
            self.capacity_loss = true;
            self.extend_unresolved(["source_closure_candidate_capacity".to_string()]);
        }
    }

    pub(crate) fn apply_candidate_resolution(&mut self, resolution: OwnerCandidateResolution) {
        self.manifest_revision = Some(resolution.manifest_hash);
        self.alternatives = resolution
            .alternative_owners
            .iter()
            .map(|owner| owner.id.clone())
            .take(MAX_ALTERNATIVES)
            .collect();
        if resolution.alternative_owners.len() > MAX_ALTERNATIVES {
            self.capacity_loss = true;
            self.extend_unresolved(["source_closure_owner_capacity".to_string()]);
        }
        if let Some(owner) = resolution.authoritative_owner {
            self.set_owner(owner);
        } else {
            self.extend_unresolved(resolution.unresolved);
        }
        self.recompute_disposition();
    }

    pub(crate) fn apply_locator(
        &mut self,
        facts: &LocateTaskDecisionFacts,
        owner: Option<ManifestOwnerProjection>,
    ) {
        self.locator_attempted = true;
        self.source_basis_bound = false;
        self.unresolved
            .retain(|item| !self.locator_unresolved.contains(item));
        self.locator_unresolved.clear();
        self.repository_identity = Some(facts.repository_identity.clone());
        self.source_snapshot_identity = Some(facts.source_snapshot_identity.clone());
        self.manifest_revision = Some(facts.owner_manifest_revision.clone());
        self.hydration_candidates.clear();
        self.alternatives = facts
            .owner_candidates
            .iter()
            .map(|candidate| candidate.owner_id.clone())
            .take(MAX_ALTERNATIVES)
            .collect();
        if facts.owner_candidates.len() > MAX_ALTERNATIVES {
            self.capacity_loss = true;
            self.extend_unresolved(["source_closure_owner_capacity".to_string()]);
        }
        if let Some(owner) = owner {
            self.set_owner(owner);
        }
        self.authoritative_source_identities = authoritative_source_identities(facts);
        for instruction in &facts.captured_instruction_sources {
            let path = normalize_path(&instruction.path);
            if let Some(observed) = self.instructions.get_mut(&path) {
                *observed = true;
            } else if self.instructions.len() < MAX_INSTRUCTIONS {
                self.instructions.insert(path, true);
            } else {
                self.capacity_loss = true;
                self.extend_unresolved(["source_closure_instruction_capacity".to_string()]);
            }
        }
        for relationship in &facts.source_relationships {
            self.add_target(&relationship.path, "caller", true);
        }
        for contract in &facts.located_contracts {
            self.add_target(&contract.path, "contract", true);
        }
        for test in &facts.located_tests {
            self.add_target(&test.path, "test", true);
        }
        for section in &facts.captured_source_sections {
            if let Some(span) = &section.span {
                if self.hydration_candidates.len() < MAX_TARGETS {
                    self.hydration_candidates.push(SourceSearchHydrationCandidate {
                        path: section.path.clone(),
                        start_line: span.start_line,
                        end_line: span.end_line,
                        kind: if section.kind
                            == codex_file_search::task_locator::LocateTaskSourceSectionKind::PrimaryImplementation
                        {
                            SourceSearchHydrationCandidateKind::AuthoritativeDefinition
                        } else {
                            SourceSearchHydrationCandidateKind::StructuredContext
                        },
                    });
                } else {
                    self.capacity_loss = true;
                    self.extend_unresolved(["source_closure_hydration_capacity".to_string()]);
                }
            }
            if section.state == LocateTaskSourceSectionState::Materialized {
                self.mark_observed(&section.path);
            }
        }
        self.validation_known =
            self.validation_known || !facts.candidate_validation_routes.is_empty();
        self.validation_explicitly_unresolved = !self.validation_known
            && facts
                .source_gaps
                .iter()
                .any(|gap| gap.to_ascii_lowercase().contains("validation"));
        self.locator_unresolved
            .extend(facts.source_gaps.iter().cloned());
        self.locator_unresolved
            .extend(facts.unresolved_source_ambiguity.iter().cloned());
        self.extend_unresolved(self.locator_unresolved.clone());
        self.locator_source_revision = self.source_revision;
        self.recompute_disposition();
    }

    pub(crate) fn bind_authoritative_source_basis(
        &mut self,
        locator_request_identity: String,
        task_contract_epoch: String,
    ) {
        let Some(source_snapshot_identity) = self.source_snapshot_identity.clone() else {
            self.source_basis_bound = false;
            return;
        };
        let Some(owner_id) = self.owner.as_ref().map(|owner| owner.id.clone()) else {
            self.source_basis_bound = false;
            return;
        };
        if source_snapshot_identity.trim().is_empty()
            || locator_request_identity.trim().is_empty()
            || owner_id.trim().is_empty()
            || task_contract_epoch.trim().is_empty()
        {
            self.source_basis_bound = false;
            return;
        }
        let signature = BoundSourceBasisSignature {
            source_snapshot_identity,
            locator_request_identity: locator_request_identity.clone(),
            owner_id,
            task_contract_epoch: task_contract_epoch.clone(),
            source_identities: self.authoritative_source_identities.clone(),
        };
        if self.bound_source_basis.as_ref() != Some(&signature) {
            self.evidence_basis_generation = self.evidence_basis_generation.saturating_add(1);
        }
        self.locator_request_identity = Some(locator_request_identity);
        self.task_contract_epoch = Some(task_contract_epoch);
        self.bound_source_basis = Some(signature);
        self.source_basis_bound = true;
    }

    pub(crate) fn admitted_source_evidence_basis(
        &self,
        path: &str,
    ) -> Option<AdmittedSourceEvidenceBasis> {
        if !self.source_basis_bound {
            return None;
        }
        let path = normalize_path(path);
        let identity = self.authoritative_source_identities.get(&path)?;
        Some(AdmittedSourceEvidenceBasis {
            source_basis: AuthoritativeSourceBasis {
                path,
                file_content_hash: identity.file_content_hash.clone(),
                source_snapshot_identity: self.source_snapshot_identity.clone()?,
            },
            obligation_identity: ObligationIdentity {
                locator_request_identity: self.locator_request_identity.clone()?,
                owner_id: self.owner.as_ref()?.id.clone(),
                task_contract_epoch: self.task_contract_epoch.clone()?,
                obligation_kind: identity.obligation_kind.clone(),
                required_ranges: identity.required_ranges.clone(),
                reset_generation: self.evidence_basis_generation,
            },
        })
    }

    pub(crate) fn hydration_candidates(&self) -> Vec<SourceSearchHydrationCandidate> {
        self.hydration_candidates.clone()
    }

    fn set_owner(&mut self, owner: ManifestOwnerProjection) {
        let replacing = self
            .owner
            .as_ref()
            .is_some_and(|prior| prior.id != owner.id);
        if replacing {
            self.source_basis_bound = false;
            self.authoritative_source_identities.clear();
            self.evidence_basis_generation = self.evidence_basis_generation.saturating_add(1);
            self.reopen("authoritative owner changed");
        }
        let prior_instructions = std::mem::take(&mut self.instructions);
        let prior_targets = std::mem::take(&mut self.targets);
        self.instructions = owner
            .instructions
            .iter()
            .take(MAX_INSTRUCTIONS)
            .map(|path| {
                let path = normalize_path(path);
                let observed =
                    !replacing && prior_instructions.get(&path).copied().unwrap_or(false);
                (path, observed)
            })
            .collect();
        if owner.instructions.len() > MAX_INSTRUCTIONS {
            self.capacity_loss = true;
            self.extend_unresolved(["source_closure_instruction_capacity".to_string()]);
        }
        for entry in &owner.primary_entries {
            self.add_target(&entry.path, "primary_implementation", true);
        }
        for path in &owner.consumers {
            self.add_target(path, "caller", true);
        }
        for path in &owner.contracts {
            self.add_target(path, "contract", true);
        }
        for path in &owner.generated_mirrors {
            self.add_target(path, "generated_contract", true);
        }
        for path in &owner.tests {
            self.add_target(path, "test", true);
        }
        if !replacing {
            for (path, prior) in prior_targets {
                if prior.observed
                    && let Some(target) = self.targets.get_mut(&path)
                {
                    target.observed = true;
                }
            }
        }
        self.validation_known = !owner.validation.is_empty();
        self.validation_explicitly_unresolved = false;
        self.owner = Some(owner);
    }

    fn add_target(&mut self, path: &str, role: &str, required: bool) {
        let path = normalize_path(path);
        if self.targets.contains_key(&path) {
            return;
        }
        if self.targets.len() >= MAX_TARGETS {
            self.capacity_loss = true;
            self.extend_unresolved(["source_closure_target_capacity".to_string()]);
            return;
        }
        self.targets.entry(path.clone()).or_insert(ClosureTarget {
            path,
            role: role.to_string(),
            required,
            observed: false,
        });
    }

    pub(crate) fn mark_observed(&mut self, path: &str) {
        let path = normalize_path(path);
        if self.observed_paths.len() < MAX_OBSERVED_PATHS {
            self.observed_paths.insert(path.clone());
        } else if !self.observed_paths.contains(&path) {
            // The target/instruction bit below remains authoritative, but loss
            // of the general observation ledger must not permit closure.
            self.capacity_loss = true;
            self.extend_unresolved(["source_closure_observation_capacity".to_string()]);
        }
        if let Some(target) = self.targets.get_mut(&path) {
            target.observed = true;
        }
        if let Some(observed) = self.instructions.get_mut(&path) {
            *observed = true;
        }
        self.recompute_disposition();
    }

    pub(crate) fn record_discovered_target(&mut self, path: &str, role: &str) {
        self.add_target(path, role, true);
        self.mark_observed(path);
    }

    pub(crate) fn has_stale_read(
        &self,
        path: &str,
        start_line: usize,
        end_line: usize,
        metadata: &SourceMetadataToken,
    ) -> bool {
        let path = normalize_path(path);
        self.read_receipts.iter().any(|receipt| {
            receipt.path == path
                && receipt.start_line <= end_line
                && receipt.end_line >= start_line
                && !receipt.metadata.permits_reuse(metadata)
        })
    }

    pub(crate) fn search_scope_changed(&self, key: &str, scope_revision: &str) -> bool {
        self.search_receipts
            .iter()
            .any(|receipt| receipt.key == key && receipt.scope_revision != scope_revision)
    }

    pub(crate) fn reopen_for_source_change(&mut self, detail: impl Into<String>) {
        self.source_basis_bound = false;
        self.authoritative_source_identities.clear();
        self.evidence_basis_generation = self.evidence_basis_generation.saturating_add(1);
        self.reopen(format!("source changed: {}", detail.into()));
    }

    pub(crate) fn is_established(&self) -> bool {
        self.disposition == SourceClosureDisposition::Established
    }

    pub(crate) fn path_is_inside_closure(&self, path: &str) -> bool {
        let path = normalize_path(path);
        self.targets.contains_key(&path)
            || self.instructions.contains_key(&path)
            || self.owner.as_ref().is_some_and(|owner| {
                owner
                    .roots
                    .iter()
                    .any(|root| path_within(&path, &normalize_path(root)))
            })
    }

    pub(crate) fn search_is_inside_closure(&self, roots: &[String]) -> bool {
        !roots.is_empty() && roots.iter().all(|root| self.path_is_inside_closure(root))
    }

    pub(crate) fn reopen_for_question(&mut self, question: &SourceQuestion) {
        self.source_basis_bound = false;
        self.evidence_basis_generation = self.evidence_basis_generation.saturating_add(1);
        self.extend_unresolved([question.summary()]);
        self.reopen("new source question");
    }

    pub(crate) fn resolve_question(&mut self, question: &SourceQuestion) {
        let summary = question.summary();
        let detail = question.detail.trim();
        self.unresolved
            .retain(|item| item != &summary && item.trim() != detail);
        self.locator_unresolved.retain(|item| item.trim() != detail);
        self.recompute_disposition();
    }

    pub(crate) fn locator_evidence_is_current(&self) -> bool {
        self.locator_attempted && self.locator_source_revision == self.source_revision
    }

    pub(crate) fn reopen(&mut self, reason: impl Into<String>) {
        self.disposition = SourceClosureDisposition::Gathering;
        self.reopen_reason = Some(reason.into());
        self.source_revision = self.source_revision.saturating_add(1);
    }

    pub(crate) fn summary(&self) -> SourceClosureSummary {
        let mut summary = SourceClosureSummary {
            authoritative_owner: self.owner.as_ref().map(|owner| owner.id.clone()),
            primary_implementation: self
                .owner
                .iter()
                .flat_map(|owner| owner.primary_entries.iter().map(|entry| entry.path.clone()))
                .collect(),
            relevant_targets: self
                .targets
                .values()
                .map(|target| SourceClosureTargetSummary {
                    path: target.path.clone(),
                    role: target.role.clone(),
                    established: target.observed,
                })
                .collect(),
            pending_required_targets: self
                .targets
                .values()
                .filter(|target| target.required && !target.observed)
                .map(|target| target.path.clone())
                .collect(),
            validation: if self.validation_known {
                "known"
            } else if self.validation_explicitly_unresolved {
                "explicitly_unresolved"
            } else {
                "pending"
            }
            .to_string(),
            unresolved_questions: self.unresolved.clone(),
            discovery: self.disposition,
            reopen_reason: self.reopen_reason.clone(),
        };
        bound_summary(&mut summary);
        summary
    }

    pub(crate) fn summary_json(&self) -> String {
        serde_json::to_string(&self.summary()).unwrap_or_else(|_| {
            "{\"discovery\":\"gathering\",\"validation\":\"pending\"}".to_string()
        })
    }

    /// Canonical identity for only the source-closure facts rendered into the
    /// model context. Private read/search cache traffic does not affect it.
    pub(crate) fn dependency_identity(&self) -> Option<String> {
        if !self.locator_attempted
            && self.owner.is_none()
            && self.repository_identity.is_none()
            && self.manifest_revision.is_none()
        {
            return None;
        }
        Some(format!(
            "repository={:?};snapshot={:?};manifest={:?};git={:?};source_revision={};summary={}",
            self.repository_identity,
            self.source_snapshot_identity,
            self.manifest_revision,
            self.git_observation
                .as_ref()
                .map(|observation| observation.identity.as_str()),
            self.source_revision,
            self.summary_json(),
        ))
    }

    pub(crate) fn find_cached_read(
        &self,
        path: &str,
        metadata: &SourceMetadataToken,
    ) -> Option<ReadReceipt> {
        let path = normalize_path(path);
        self.read_receipts
            .iter()
            .rev()
            .find(|receipt| receipt.path == path && receipt.metadata.permits_reuse(metadata))
            .cloned()
    }

    pub(crate) fn record_read(&mut self, receipt: ReadReceipt) {
        self.read_receipts.push_back(receipt);
        while self.read_receipts.len() > MAX_INTERVALS {
            self.read_receipts.pop_front();
        }
    }

    pub(crate) fn retain_source_watch(
        &mut self,
        path: String,
        registration: SourceFreshnessRegistration,
    ) {
        self.source_watch_registrations.insert(path, registration);
        while self.source_watch_registrations.len() > MAX_OBSERVED_PATHS {
            let Some(first) = self.source_watch_registrations.keys().next().cloned() else {
                break;
            };
            self.source_watch_registrations.remove(&first);
        }
    }

    pub(crate) fn find_search(&self, key: &str, scope_revision: &str) -> Option<SearchReceipt> {
        self.search_receipts
            .iter()
            .rev()
            .find(|receipt| receipt.key == key && receipt.scope_revision == scope_revision)
            .cloned()
    }

    pub(crate) fn record_search(&mut self, receipt: SearchReceipt) {
        self.search_receipts
            .retain(|prior| prior.key != receipt.key);
        self.search_receipts.push_back(receipt);
        while self.search_receipts.len() > MAX_SEARCH_RECEIPTS {
            self.search_receipts.pop_front();
        }
    }

    pub(crate) fn reserve_read(
        &mut self,
        key: String,
    ) -> Result<SourceReservation, watch::Receiver<bool>> {
        if let Some((path, start_line, end_line)) = parse_read_reservation_key(&key)
            && let Some(producer) =
                self.read_reservations
                    .iter()
                    .find_map(|(existing_key, producer)| {
                        let (existing_path, existing_start, existing_end) =
                            parse_read_reservation_key(existing_key)?;
                        (path == existing_path
                            && start_line <= existing_end
                            && end_line >= existing_start)
                            .then_some(producer)
                    })
        {
            return Err(producer.subscribe());
        }
        reserve(&mut self.read_reservations, key)
    }

    pub(crate) fn finish_read_reservation(&mut self, key: &str) {
        finish_reservation(&mut self.read_reservations, key);
    }

    pub(crate) fn reserve_search(
        &mut self,
        key: String,
    ) -> Result<SourceReservation, watch::Receiver<bool>> {
        reserve(&mut self.search_reservations, key)
    }

    pub(crate) fn finish_search_reservation(&mut self, key: &str) {
        finish_reservation(&mut self.search_reservations, key);
    }

    fn extend_unresolved(&mut self, unresolved: impl IntoIterator<Item = String>) {
        for item in unresolved {
            if !item.trim().is_empty() && !self.unresolved.contains(&item) {
                self.unresolved.push(item);
            }
        }
        if self.unresolved.len() > MAX_UNRESOLVED {
            self.unresolved.truncate(MAX_UNRESOLVED);
            self.capacity_loss = true;
            if !self
                .unresolved
                .iter()
                .any(|item| item == "source_closure_unresolved_capacity")
            {
                self.unresolved[MAX_UNRESOLVED - 1] =
                    "source_closure_unresolved_capacity".to_string();
            }
        }
    }

    fn recompute_disposition(&mut self) {
        let owner_known = self.owner.is_some();
        // An authoritative owner with no applicable instruction paths has a
        // known (empty) instruction set. Closure still requires an observed
        // current primary span, not merely an observed caller or test.
        let instructions_known = self.instructions.values().all(|observed| *observed);
        let primary_observed = self
            .targets
            .values()
            .any(|target| target.role == "primary_implementation" && target.observed);
        let required_targets_known = self
            .targets
            .values()
            .filter(|target| target.required)
            .all(|target| target.observed);
        self.disposition = if owner_known
            && instructions_known
            && primary_observed
            && required_targets_known
            && (self.validation_known || self.validation_explicitly_unresolved)
            && !self.capacity_loss
        {
            self.reopen_reason = None;
            SourceClosureDisposition::Established
        } else {
            SourceClosureDisposition::Gathering
        };
    }
}

pub(crate) fn read_reservation_key(path: &str, start_line: usize, end_line: usize) -> String {
    format!("{}\u{1f}{start_line}\u{1f}{end_line}", normalize_path(path))
}

fn parse_read_reservation_key(key: &str) -> Option<(&str, usize, usize)> {
    let mut fields = key.rsplitn(3, '\u{1f}');
    let end_line = fields.next()?.parse().ok()?;
    let start_line = fields.next()?.parse().ok()?;
    let path = fields.next()?;
    Some((path, start_line, end_line))
}

fn reserve(
    reservations: &mut BTreeMap<String, watch::Sender<bool>>,
    key: String,
) -> Result<SourceReservation, watch::Receiver<bool>> {
    if let Some(sender) = reservations.get(&key) {
        return Err(sender.subscribe());
    }
    let (sender, _receiver) = watch::channel(false);
    reservations.insert(key, sender);
    Ok(SourceReservation)
}

fn finish_reservation(reservations: &mut BTreeMap<String, watch::Sender<bool>>, key: &str) {
    if let Some(sender) = reservations.remove(key) {
        let _ = sender.send(true);
    }
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

fn authoritative_source_identities(
    facts: &LocateTaskDecisionFacts,
) -> BTreeMap<String, AuthoritativeSourceIdentity> {
    let mut identities = BTreeMap::<String, AuthoritativeSourceIdentity>::new();
    let mut conflicting_paths = BTreeSet::new();
    for section in &facts.captured_source_sections {
        if section.source_snapshot_identity != facts.source_snapshot_identity {
            continue;
        }
        let (Some(file_content_hash), Some(span)) =
            (section.file_content_hash.as_ref(), section.span.as_ref())
        else {
            continue;
        };
        if file_content_hash.is_empty() || span.start_line == 0 || span.end_line < span.start_line {
            continue;
        }
        let path = normalize_path(&section.path);
        if path.is_empty() || conflicting_paths.contains(&path) {
            continue;
        }
        let range = SourceEvidenceRange {
            start_line: span.start_line,
            end_line: span.end_line,
        };
        let obligation_kind = match section.kind {
            codex_file_search::task_locator::LocateTaskSourceSectionKind::PrimaryImplementation => {
                "primary"
            }
            codex_file_search::task_locator::LocateTaskSourceSectionKind::Caller => "caller",
            codex_file_search::task_locator::LocateTaskSourceSectionKind::Test => "test",
            codex_file_search::task_locator::LocateTaskSourceSectionKind::Contract => "contract",
            codex_file_search::task_locator::LocateTaskSourceSectionKind::Generated => "generated",
            codex_file_search::task_locator::LocateTaskSourceSectionKind::OtherSourceContext => {
                "other_source_context"
            }
        };
        match identities.get_mut(&path) {
            Some(identity)
                if identity.file_content_hash.as_str() == file_content_hash.as_str()
                    && identity.obligation_kind == obligation_kind =>
            {
                identity.required_ranges.push(range);
            }
            Some(_) => {
                identities.remove(&path);
                conflicting_paths.insert(path);
            }
            None => {
                identities.insert(
                    path,
                    AuthoritativeSourceIdentity {
                        file_content_hash: file_content_hash.clone(),
                        obligation_kind: obligation_kind.to_string(),
                        required_ranges: vec![range],
                    },
                );
            }
        }
    }
    for identity in identities.values_mut() {
        canonicalize_evidence_ranges(&mut identity.required_ranges);
    }
    identities
}

fn canonicalize_evidence_ranges(ranges: &mut Vec<SourceEvidenceRange>) {
    ranges.sort();
    let mut merged: Vec<SourceEvidenceRange> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if let Some(previous) = merged.last_mut()
            && range.start_line <= previous.end_line.saturating_add(1)
        {
            previous.end_line = previous.end_line.max(range.end_line);
        } else {
            merged.push(range);
        }
    }
    *ranges = merged;
}

fn path_within(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|tail| tail.starts_with('/'))
}

fn bound_summary(summary: &mut SourceClosureSummary) {
    while serde_json::to_vec(summary).map_or(0, |bytes| bytes.len())
        > SOURCE_CLOSURE_SUMMARY_TARGET_BYTES
        && summary.relevant_targets.len() > 8
    {
        summary.relevant_targets.pop();
    }
    while serde_json::to_vec(summary).map_or(0, |bytes| bytes.len())
        > SOURCE_CLOSURE_SUMMARY_MAX_BYTES
        && !summary.unresolved_questions.is_empty()
    {
        summary.unresolved_questions.pop();
    }
    while serde_json::to_vec(summary).map_or(0, |bytes| bytes.len())
        > SOURCE_CLOSURE_SUMMARY_MAX_BYTES
        && !summary.pending_required_targets.is_empty()
    {
        summary.pending_required_targets.pop();
    }
    while serde_json::to_vec(summary).map_or(0, |bytes| bytes.len())
        > SOURCE_CLOSURE_SUMMARY_MAX_BYTES
        && !summary.relevant_targets.is_empty()
    {
        summary.relevant_targets.pop();
    }
    while serde_json::to_vec(summary).map_or(0, |bytes| bytes.len())
        > SOURCE_CLOSURE_SUMMARY_MAX_BYTES
        && summary.primary_implementation.len() > 1
    {
        summary.primary_implementation.pop();
    }
    if serde_json::to_vec(summary).map_or(0, |bytes| bytes.len()) > SOURCE_CLOSURE_SUMMARY_MAX_BYTES
    {
        summary.authoritative_owner = summary
            .authoritative_owner
            .take()
            .map(|value| truncate_utf8(value, 1024));
        for value in &mut summary.primary_implementation {
            *value = truncate_utf8(std::mem::take(value), 1024);
        }
        summary.reopen_reason = summary
            .reopen_reason
            .take()
            .map(|value| truncate_utf8(value, 1024));
    }
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_file_search::task_locator::ExactSpan;
    use codex_file_search::task_locator::LocateTaskLocatedPath;
    use codex_file_search::task_locator::LocateTaskSourceSection;
    use codex_file_search::task_locator::LocateTaskSourceSectionKind;
    use codex_file_search::task_locator::LocateTaskValidationRoute;

    fn owner(instructions: Vec<String>) -> ManifestOwnerProjection {
        ManifestOwnerProjection {
            id: "owner".to_string(),
            roots: vec!["src".to_string()],
            primary_entries: vec![LocateTaskLocatedPath {
                path: "src/lib.rs".to_string(),
                role: "primary".to_string(),
            }],
            instructions,
            consumers: Vec::new(),
            contracts: Vec::new(),
            generated_mirrors: Vec::new(),
            tests: Vec::new(),
            validation: vec![LocateTaskValidationRoute {
                id: "focused".to_string(),
                cwd: ".".to_string(),
                argv: vec!["cargo".to_string(), "test".to_string()],
                role: "test".to_string(),
            }],
        }
    }

    fn locator_facts(
        authoritative_owner: ManifestOwnerProjection,
        source_gaps: Vec<String>,
    ) -> LocateTaskDecisionFacts {
        LocateTaskDecisionFacts {
            repository_identity: "repo".to_string(),
            source_snapshot_identity: "snapshot".to_string(),
            owner_manifest_revision: "manifest".to_string(),
            closure_contract_revision: "source_closure_v2".to_string(),
            completeness: "complete".to_string(),
            selected_owner: Some(authoritative_owner.id.clone()),
            authoritative_owner: Some(authoritative_owner),
            owner_candidates: Vec::new(),
            primary_path: Some("src/lib.rs".to_string()),
            primary_symbol: None,
            primary_span: None,
            source_relationships: Vec::new(),
            located_contracts: Vec::new(),
            located_tests: Vec::new(),
            captured_instruction_sources: Vec::new(),
            captured_source_sections: Vec::new(),
            candidate_validation_routes: Vec::new(),
            source_gaps,
            unresolved_source_ambiguity: Vec::new(),
            truncated: false,
        }
    }

    #[test]
    fn compact_projection_excludes_private_cache_metadata() {
        let mut state = SourceClosureState::default();
        state.record_read(ReadReceipt {
            path: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 10,
            metadata: SourceMetadataToken {
                size: 100,
                created_at_ms: 1,
                modified_at_ms: 2,
                is_symlink: false,
                repository_identity: "repo".to_string(),
                environment_identity: "local".to_string(),
                mutation_revision: 0,
                watcher_generation: 0,
                host_mutation_generation: 0,
                stable_file_identity: "file".to_string(),
                freshness_identity: "freshness".to_string(),
            },
            content_hash: "secret-hash".to_string(),
            artifact_id: "secret-artifact".to_string(),
        });
        let visible = state.summary_json();
        assert!(!visible.contains("secret-hash"));
        assert!(!visible.contains("secret-artifact"));
        assert!(visible.len() <= SOURCE_CLOSURE_SUMMARY_MAX_BYTES);
    }

    #[tokio::test]
    async fn failed_producer_can_wake_joiners() {
        let mut state = SourceClosureState::default();
        let _producer = state.reserve_read("key".to_string()).expect("producer");
        let mut waiter = state.reserve_read("key".to_string()).expect_err("joiner");
        state.finish_read_reservation("key");
        let _ = tokio::time::timeout(std::time::Duration::from_millis(50), waiter.changed())
            .await
            .expect("joiner woke");
    }

    #[tokio::test]
    async fn source_tools_overlapping_read_reservations_join_the_producer() {
        let mut state = SourceClosureState::default();
        let producer_key = read_reservation_key("src/lib.rs", 10, 20);
        let joiner_key = read_reservation_key("src/lib.rs", 15, 25);
        let _producer = state.reserve_read(producer_key.clone()).expect("producer");
        let mut waiter = state.reserve_read(joiner_key).expect_err("overlap joins");
        state.finish_read_reservation(&producer_key);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(50), waiter.changed())
            .await
            .expect("joiner woke");
    }

    #[test]
    fn source_tools_empty_instruction_set_can_establish_after_primary_observation() {
        let mut state = SourceClosureState::default();
        state.apply_candidate_resolution(OwnerCandidateResolution {
            manifest_hash: "manifest".to_string(),
            authoritative_owner: Some(owner(Vec::new())),
            alternative_owners: Vec::new(),
            matched_candidates: vec!["tests/lib.rs".to_string()],
            unmatched_candidates: Vec::new(),
            unresolved: Vec::new(),
        });
        assert!(!state.is_established());
        state.mark_observed("src/lib.rs");
        assert!(state.is_established());
    }

    #[test]
    fn source_tools_capacity_loss_prevents_false_closure() {
        let mut state = SourceClosureState::default();
        let mut authoritative = owner(Vec::new());
        authoritative
            .primary_entries
            .extend((0..MAX_TARGETS).map(|index| LocateTaskLocatedPath {
                path: format!("src/generated_{index}.rs"),
                role: "primary".to_string(),
            }));
        state.apply_candidate_resolution(OwnerCandidateResolution {
            manifest_hash: "manifest".to_string(),
            authoritative_owner: Some(authoritative),
            alternative_owners: Vec::new(),
            matched_candidates: Vec::new(),
            unmatched_candidates: Vec::new(),
            unresolved: Vec::new(),
        });
        for index in 0..MAX_TARGETS {
            state.mark_observed(&format!("src/generated_{index}.rs"));
        }
        state.mark_observed("src/lib.rs");
        assert!(!state.is_established());
        assert!(
            state
                .summary()
                .unresolved_questions
                .iter()
                .any(|question| question.contains("capacity"))
        );
    }

    #[test]
    fn source_tools_explicitly_unresolved_validation_can_close_without_becoming_known() {
        let mut authoritative = owner(Vec::new());
        authoritative.validation.clear();
        let facts = locator_facts(
            authoritative.clone(),
            vec!["validation route explicitly unresolved".to_string()],
        );
        let mut state = SourceClosureState::default();
        state.apply_locator(&facts, Some(authoritative));
        state.mark_observed("src/lib.rs");
        assert!(state.is_established());
        assert_eq!(state.summary().validation, "explicitly_unresolved");
    }

    #[test]
    fn source_tools_changed_search_revision_reopens_closure() {
        let mut state = SourceClosureState::default();
        state.record_search(SearchReceipt {
            key: "query".to_string(),
            scope_revision: "before".to_string(),
            artifact_id: "artifact".to_string(),
            capped_zero: false,
        });
        assert!(state.search_scope_changed("query", "after"));
        state.reopen_for_source_change("search scope");
        assert_eq!(
            state.summary().discovery,
            SourceClosureDisposition::Gathering
        );
        assert_eq!(
            state.summary().reopen_reason.as_deref(),
            Some("source changed: search scope")
        );
    }

    #[test]
    fn locator_refresh_is_admitted_after_source_revision_changes() {
        let authoritative = owner(Vec::new());
        let facts = locator_facts(authoritative.clone(), Vec::new());
        let mut state = SourceClosureState::default();
        state.apply_locator(&facts, Some(authoritative));
        assert!(state.locator_evidence_is_current());

        state.reopen_for_source_change("src/lib.rs");

        assert!(!state.locator_evidence_is_current());
    }

    #[test]
    fn authoritative_basis_uses_existing_owner_identity_and_ignores_progress_revision() {
        let authoritative = owner(Vec::new());
        let mut facts = locator_facts(authoritative.clone(), Vec::new());
        facts
            .captured_source_sections
            .push(LocateTaskSourceSection {
                section_id: "caller-section".to_string(),
                kind: LocateTaskSourceSectionKind::Caller,
                state: LocateTaskSourceSectionState::NotMaterialized,
                path: "src/caller.rs".to_string(),
                span: Some(ExactSpan {
                    start_line: 1,
                    end_line: 100,
                    start_byte: 0,
                    end_byte: 1000,
                }),
                content_hash: None,
                file_content_hash: Some("hash-a".to_string()),
                source_snapshot_identity: "snapshot".to_string(),
                text: None,
                provenance: "locator".to_string(),
            });
        let mut state = SourceClosureState::default();
        state.apply_locator(&facts, Some(authoritative));
        state.bind_authoritative_source_basis("locator-v1".to_string(), "contract-1".to_string());
        let admitted = state
            .admitted_source_evidence_basis("./src\\caller.rs")
            .expect("owner-provided identity");
        assert_eq!(admitted.source_basis.file_content_hash, "hash-a");
        assert_eq!(admitted.source_basis.source_snapshot_identity, "snapshot");
        assert_eq!(admitted.obligation_identity.obligation_kind, "caller");
        assert_eq!(
            admitted.obligation_identity.required_ranges,
            vec![SourceEvidenceRange {
                start_line: 1,
                end_line: 100,
            }]
        );

        state.mark_observed("src/lib.rs");
        state.bind_authoritative_source_basis("locator-v1".to_string(), "contract-1".to_string());
        assert_eq!(
            state.admitted_source_evidence_basis("src/caller.rs"),
            Some(admitted),
            "progress-only source revisions must not split the stable evidence basis"
        );
    }

    #[test]
    fn authoritative_basis_is_unavailable_or_invalidated_fail_closed() {
        let authoritative = owner(Vec::new());
        let mut facts = locator_facts(authoritative.clone(), Vec::new());
        facts
            .captured_source_sections
            .push(LocateTaskSourceSection {
                section_id: "caller-section".to_string(),
                kind: LocateTaskSourceSectionKind::Caller,
                state: LocateTaskSourceSectionState::NotMaterialized,
                path: "src/caller.rs".to_string(),
                span: Some(ExactSpan {
                    start_line: 1,
                    end_line: 100,
                    start_byte: 0,
                    end_byte: 1000,
                }),
                content_hash: None,
                file_content_hash: None,
                source_snapshot_identity: "snapshot".to_string(),
                text: None,
                provenance: "locator".to_string(),
            });
        let mut state = SourceClosureState::default();
        state.apply_locator(&facts, Some(authoritative));
        state.bind_authoritative_source_basis("locator-v1".to_string(), "contract-1".to_string());
        assert!(
            state
                .admitted_source_evidence_basis("src/caller.rs")
                .is_none()
        );

        facts.captured_source_sections[0].file_content_hash = Some("hash-a".to_string());
        state.apply_locator(&facts, facts.authoritative_owner.clone());
        state.bind_authoritative_source_basis("locator-v1".to_string(), "contract-1".to_string());
        assert!(
            state
                .admitted_source_evidence_basis("src/caller.rs")
                .is_some()
        );

        state.reopen_for_question(&SourceQuestion {
            kind: SourceQuestionKind::IncompletePriorResult,
            detail: "explicitly rebind the source obligation".to_string(),
        });
        assert!(
            state
                .admitted_source_evidence_basis("src/caller.rs")
                .is_none()
        );
    }

    #[test]
    fn focused_question_resolution_retires_locator_and_question_obligations() {
        let detail = "find the unknown caller";
        let authoritative = owner(Vec::new());
        let facts = locator_facts(authoritative.clone(), vec![detail.to_string()]);
        let mut state = SourceClosureState::default();
        state.apply_locator(&facts, Some(authoritative));
        let question = SourceQuestion {
            kind: SourceQuestionKind::UnknownCaller,
            detail: detail.to_string(),
        };
        state.reopen_for_question(&question);

        state.resolve_question(&question);

        assert!(state.summary().unresolved_questions.is_empty());
    }

    #[test]
    fn source_tools_metadata_revision_uses_scoped_freshness_and_replacement() {
        let base = SourceMetadataToken {
            size: 10,
            created_at_ms: 1,
            modified_at_ms: 2,
            is_symlink: false,
            repository_identity: "repo".to_string(),
            environment_identity: "local".to_string(),
            mutation_revision: 0,
            watcher_generation: 3,
            host_mutation_generation: 4,
            stable_file_identity: "file-a".to_string(),
            freshness_identity: "freshness-a".to_string(),
        };
        assert!(base.permits_reuse(&base));
        let mut changed = base.clone();
        changed.watcher_generation += 1;
        assert!(base.permits_reuse(&changed));
        changed.freshness_identity = "freshness-b".to_string();
        assert!(!base.permits_reuse(&changed));
        changed = base.clone();
        changed.mutation_revision += 1;
        assert!(!base.permits_reuse(&changed));
        changed = base.clone();
        changed.stable_file_identity = "file-b".to_string();
        assert!(!base.permits_reuse(&changed));

        let mut state = SourceClosureState::default();
        state.record_read(ReadReceipt {
            path: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 10,
            metadata: base,
            content_hash: "hash".to_string(),
            artifact_id: "artifact".to_string(),
        });
        assert!(state.has_stale_read("src/lib.rs", 5, 6, &changed));
    }
}
