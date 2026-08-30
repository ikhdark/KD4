use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_tools::ToolPayload;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::git_workspace::GitWorkspaceCache;
use crate::git_workspace::SourcePathChangeObservation;
use crate::git_workspace::WorkspaceEvidenceIdentity;
use crate::tools::command_output_artifact::reconcile_active_tool_history_artifact_protection;
use crate::tools::command_output_artifact::remint_tool_history_artifact_for_thread;

const RECEIPT_VERSION: u8 = 2;
const LEGACY_RECEIPT_VERSION: u8 = 1;
const TOOL_SEARCH_RECEIPT_VERSION: u8 = 1;
const RECEIPT_MAX_TOKENS: usize = 256;
const RECEIPT_DIGEST_TARGET_TOKENS: usize = 96;
const MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET: usize = 10_000;
const MINIMUM_RAW_TOKENS: u64 = 256;
const MINIMUM_SAVED_TOKENS: u64 = 64;
const MINIMUM_RELATIVE_SAVINGS_PERCENT: u64 = 25;
const LEDGER_VERSION: u8 = 1;
const JOURNAL_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ModelGenerationId {
    pub(crate) turn_id: String,
    pub(crate) ordinal: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ToolHistoryReceiptV1 {
    version: u8,
    receipt_id: String,
    call_id: String,
    tool_identity: String,
    semantic_class: String,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    source_dependencies_current: bool,
    digest: String,
    artifact: ReceiptArtifact,
    original: ReceiptOriginalSize,
    retrieval: ReceiptRetrieval,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ToolHistoryReceiptV2 {
    version: u8,
    receipt_id: String,
    call_id: String,
    tool_identity: String,
    semantic_class: String,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    source_dependencies_current: bool,
    digest: String,
    artifact_id: String,
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
enum ToolHistoryReceipt {
    V2(ToolHistoryReceiptV2),
    V1(ToolHistoryReceiptV1),
}

impl ToolHistoryReceipt {
    fn receipt_id(&self) -> &str {
        match self {
            Self::V2(receipt) => &receipt.receipt_id,
            Self::V1(receipt) => &receipt.receipt_id,
        }
    }

    fn is_valid_for_call(&self, call_id: &str) -> bool {
        match self {
            Self::V2(receipt) => {
                receipt.version == RECEIPT_VERSION
                    && receipt.call_id == call_id
                    && receipt.receipt_id
                        == receipt_id_for(
                            call_id,
                            &receipt.sha256,
                            &receipt.tool_identity,
                            &receipt.semantic_class,
                            receipt.bytes,
                        )
                    && receipt.bytes > 0
                    && !receipt.artifact_id.is_empty()
                    && is_sha256_hex(&receipt.sha256)
                    && !receipt.digest.is_empty()
            }
            Self::V1(receipt) => {
                receipt.version == LEGACY_RECEIPT_VERSION
                    && receipt.call_id == call_id
                    && receipt.receipt_id
                        == receipt_id_for(
                            call_id,
                            &receipt.artifact.sha256,
                            &receipt.tool_identity,
                            &receipt.semantic_class,
                            receipt.original.bytes,
                        )
                    && receipt.artifact.complete
                    && receipt.artifact.byte_start == 0
                    && receipt.artifact.byte_end > 0
                    && receipt.artifact.byte_end == receipt.original.bytes
                    && receipt.original.approximate_tokens > 0
                    && !receipt.artifact.artifact_id.is_empty()
                    && is_sha256_hex(&receipt.artifact.sha256)
                    && !receipt.digest.is_empty()
                    && receipt.retrieval.tool == "read_tool_output"
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ToolSearchReceiptV1 {
    version: u8,
    receipt_id: String,
    call_id: String,
    status: String,
    execution: String,
    arguments: serde_json::Value,
    result_set_sha256: String,
    result_count: usize,
    omitted_result_count: Option<usize>,
    complete: bool,
    ordered_tool_identities: Vec<String>,
    omitted_identity_count: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct SourceDependencyV1 {
    pub(crate) path: String,
    pub(crate) recursive: bool,
}

impl SourceDependencyV1 {
    pub(crate) fn new(path: &Path, recursive: bool) -> Self {
        Self {
            path: normalized_source_path(path),
            recursive,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ReceiptArtifact {
    artifact_id: String,
    byte_start: u64,
    byte_end: u64,
    sha256: String,
    complete: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ReceiptOriginalSize {
    bytes: u64,
    approximate_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ReceiptRetrieval {
    tool: String,
    instruction: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ToolHistoryArtifactPinV1 {
    version: u8,
    kind: String,
    artifact_id: String,
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ToolHistoryCandidate {
    pub(crate) call_id: String,
    pub(crate) tool_identity: String,
    pub(crate) semantic_class: String,
    #[serde(default)]
    pub(crate) source_dependencies: BTreeSet<SourceDependencyV1>,
    #[serde(default = "default_true")]
    pub(crate) source_dependencies_current: bool,
    pub(crate) artifact_id: String,
    pub(crate) artifact_bytes: u64,
    pub(crate) artifact_sha256: String,
    pub(crate) original_output_sha256: String,
    pub(crate) original_tokens: u64,
    #[serde(default)]
    pub(crate) preserved_non_text_tokens: u64,
    #[serde(rename = "bounded_digest")]
    pub(crate) bounded_model_output: String,
    pub(crate) complete: bool,
    pub(crate) projection_eligible: bool,
    pub(crate) proof_identity: Option<String>,
    pub(crate) supersession_identity: Option<String>,
    pub(crate) consumed_by_generation: Option<ModelGenerationId>,
    #[serde(skip)]
    pub(crate) derived: ToolHistoryCandidateDerived,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ToolHistoryCandidateDerived {
    receipt_id: String,
    bounded_model_output_sha256: String,
    bounded_model_output_tokens: u64,
    receipt: Option<String>,
    receipt_tokens: u64,
}

impl ToolHistoryCandidate {
    pub(crate) fn artifact_reference(&self) -> (u64, String) {
        (self.artifact_bytes, self.artifact_sha256.clone())
    }

    fn artifact_pin_value(&self) -> Option<serde_json::Value> {
        if !self.complete || !self.projection_eligible {
            return None;
        }
        Some(serde_json::json!({
            "version": 1,
            "kind": "tool_history_artifact_pin",
            "artifact_id": self.artifact_id,
            "bytes": self.artifact_bytes,
            "sha256": self.artifact_sha256,
            "retrieval": {
                "tool": "read_tool_output",
                "instruction": "Call read_tool_output with artifact_id to recover the exact output."
            }
        }))
    }

    fn artifact_pin(&self) -> Option<(String, usize)> {
        let rendered = serde_json::to_string(&self.artifact_pin_value()?).ok()?;
        let tokens = approx_token_count(&rendered);
        Some((rendered, tokens))
    }

    fn receipt(&self) -> Option<(&str, &str, u64)> {
        self.render_receipt(
            /*require_consumed*/ true, /*require_savings*/ true,
        )
    }

    fn admission_receipt(&self) -> Option<(&str, &str, u64)> {
        self.render_receipt(
            /*require_consumed*/ false, /*require_savings*/ false,
        )
    }

    fn render_receipt(
        &self,
        require_consumed: bool,
        require_savings: bool,
    ) -> Option<(&str, &str, u64)> {
        if !self.complete
            || !self.projection_eligible
            || (require_consumed && self.consumed_by_generation.is_none())
        {
            return None;
        }
        let bounded_tokens = self.derived.bounded_model_output_tokens;
        if require_savings && bounded_tokens < MINIMUM_RAW_TOKENS {
            return None;
        }
        let rendered = self.derived.receipt.as_deref()?;
        let receipt_tokens = self.derived.receipt_tokens;
        let saved = bounded_tokens.saturating_sub(receipt_tokens);
        let relative = saved
            .saturating_mul(100)
            .checked_div(bounded_tokens.max(1))
            .unwrap_or(0);
        if require_savings
            && (saved < MINIMUM_SAVED_TOKENS || relative < MINIMUM_RELATIVE_SAVINGS_PERCENT)
        {
            return None;
        }
        Some((self.derived.receipt_id.as_str(), rendered, receipt_tokens))
    }

    fn refresh_derived(&mut self) {
        let receipt_id = receipt_id_for(
            &self.call_id,
            &self.artifact_sha256,
            &self.tool_identity,
            &self.semantic_class,
            self.artifact_bytes,
        );
        let bounded_model_output_sha256 = sha256(self.bounded_model_output.as_bytes());
        let bounded_model_output_tokens =
            u64::try_from(approx_token_count(&self.bounded_model_output)).unwrap_or(u64::MAX);
        let (receipt, receipt_tokens) = self
            .fit_receipt(&receipt_id)
            .map_or((None, 0), |(receipt, tokens)| (Some(receipt), tokens));
        self.derived = ToolHistoryCandidateDerived {
            receipt_id,
            bounded_model_output_sha256,
            bounded_model_output_tokens,
            receipt,
            receipt_tokens,
        };
    }

    fn fit_receipt(&self, receipt_id: &str) -> Option<(String, u64)> {
        if !self.complete || !self.projection_eligible {
            return None;
        }
        let mut receipt = ToolHistoryReceiptV2 {
            version: RECEIPT_VERSION,
            receipt_id: receipt_id.to_string(),
            call_id: self.call_id.clone(),
            tool_identity: self.tool_identity.clone(),
            semantic_class: self.semantic_class.clone(),
            source_dependencies_current: self.source_dependencies_current,
            digest: String::new(),
            artifact_id: self.artifact_id.clone(),
            bytes: self.artifact_bytes,
            sha256: self.artifact_sha256.clone(),
        };
        if !self.source_dependencies_current {
            receipt.digest = "STALE: a source dependency changed after this result was produced; rerun the tool before relying on it."
                .to_string();
            let rendered = serde_json::to_string(&receipt).ok()?;
            let tokens = u64::try_from(approx_token_count(&rendered)).unwrap_or(u64::MAX);
            return (tokens <= RECEIPT_MAX_TOKENS as u64).then_some((rendered, tokens));
        }

        let mut digest_limit = RECEIPT_DIGEST_TARGET_TOKENS;
        loop {
            receipt.digest =
                truncate_text_to_token_ceiling(&self.bounded_model_output, digest_limit);
            let rendered = serde_json::to_string(&receipt).ok()?;
            let receipt_tokens = u64::try_from(approx_token_count(&rendered)).unwrap_or(u64::MAX);
            if receipt_tokens <= RECEIPT_MAX_TOKENS as u64 {
                return Some((rendered, receipt_tokens));
            }
            if digest_limit == 0 {
                return None;
            }
            // Preserve a useful digest at the 256-token envelope boundary.
            // A 32-token decrement could jump from an oversized 32-token
            // digest directly to an empty one even when a 16-token digest fit.
            digest_limit = digest_limit.saturating_sub(16);
        }
    }

    fn matches_parsed_receipt(&self, receipt: &ToolHistoryReceipt) -> bool {
        match receipt {
            ToolHistoryReceipt::V2(receipt) => {
                receipt.version == RECEIPT_VERSION
                    && receipt.call_id == self.call_id
                    && receipt.receipt_id == self.derived.receipt_id
                    && receipt.tool_identity == self.tool_identity
                    && receipt.semantic_class == self.semantic_class
                    && receipt.source_dependencies_current == self.source_dependencies_current
                    && receipt.artifact_id == self.artifact_id
                    && receipt.sha256 == self.artifact_sha256
                    && receipt.bytes == self.artifact_bytes
                    && self.complete
            }
            ToolHistoryReceipt::V1(receipt) => {
                receipt.version == LEGACY_RECEIPT_VERSION
                    && receipt.call_id == self.call_id
                    && receipt.receipt_id == self.derived.receipt_id
                    && receipt.tool_identity == self.tool_identity
                    && receipt.semantic_class == self.semantic_class
                    && receipt.source_dependencies_current == self.source_dependencies_current
                    && receipt.artifact.artifact_id == self.artifact_id
                    && receipt.artifact.sha256 == self.artifact_sha256
                    && receipt.artifact.byte_start == 0
                    && receipt.artifact.byte_end == self.artifact_bytes
                    && receipt.artifact.complete == self.complete
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolHistorySubstitution {
    pub(crate) item_index: usize,
    pub(crate) call_id: String,
    pub(crate) bounded_output_sha256: String,
    pub(crate) receipt_id: String,
    pub(crate) substituted_output_sha256: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ToolHistoryProjection {
    pub(crate) items: Arc<[ResponseItem]>,
    pub(crate) unreplaced_items: Arc<[ResponseItem]>,
    pub(crate) substitutions: Arc<[ToolHistorySubstitution]>,
}

#[derive(Clone, Debug)]
enum ProjectedResponseItems {
    Shared(Arc<[ResponseItem]>),
    Owned(Vec<ResponseItem>),
}

impl ProjectedResponseItems {
    fn make_owned(&mut self) -> &mut Vec<ResponseItem> {
        if let Self::Shared(items) = self {
            *self = Self::Owned(items.to_vec());
        }
        match self {
            Self::Shared(_) => unreachable!("shared projection should have been materialized"),
            Self::Owned(items) => items,
        }
    }

    fn retain(&mut self, mut keep: impl FnMut(&ResponseItem) -> bool) {
        if self.iter().any(|item| !keep(item)) {
            self.make_owned().retain(keep);
        }
    }

    fn into_shared(self) -> Arc<[ResponseItem]> {
        match self {
            Self::Shared(items) => items,
            Self::Owned(items) => items.into(),
        }
    }
}

impl Deref for ProjectedResponseItems {
    type Target = [ResponseItem];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Shared(items) => items,
            Self::Owned(items) => items,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ToolHistoryState {
    #[serde(default)]
    candidates: BTreeMap<String, ToolHistoryCandidate>,
    #[serde(default)]
    workspace_evidence: BTreeMap<String, WorkspaceEvidenceObservation>,
    /// Current runtimes record completed code-mode carriers that authoritatively
    /// contained no workspace-observing nested calls. Absence remains unknown
    /// for legacy ledgers and therefore fails closed.
    #[serde(default)]
    non_workspace_code_mode_calls: BTreeSet<String>,
    #[serde(skip)]
    artifact_call_ids: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct WorkspaceEvidenceObservation {
    call_id: String,
    output_sha256: String,
    #[serde(default)]
    revision: Option<WorkspaceEvidenceIdentity>,
    #[serde(default)]
    source_dependencies: BTreeSet<SourceDependencyV1>,
    #[serde(default)]
    source_path_observations: Vec<SourcePathChangeObservation>,
    #[serde(default = "default_true")]
    source_dependencies_current: bool,
}

impl WorkspaceEvidenceObservation {
    #[cfg(test)]
    pub(crate) fn from_response_item(
        revision: Option<WorkspaceEvidenceIdentity>,
        item: &ResponseItem,
        source_dependencies: BTreeSet<SourceDependencyV1>,
    ) -> Option<Self> {
        Self::from_response_item_with_freshness(
            revision,
            item,
            source_dependencies,
            /*source_dependencies_current*/ true,
        )
    }

    pub(crate) fn from_response_item_with_freshness(
        revision: Option<WorkspaceEvidenceIdentity>,
        item: &ResponseItem,
        source_dependencies: BTreeSet<SourceDependencyV1>,
        source_dependencies_current: bool,
    ) -> Option<Self> {
        let (call_id, output) = canonical_textual_output_identity(item)?;
        Some(Self {
            call_id: call_id.to_string(),
            output_sha256: sha256(output.as_bytes()),
            revision,
            source_dependencies,
            source_path_observations: Vec::new(),
            source_dependencies_current,
        })
    }

    pub(crate) fn with_source_path_observations(
        mut self,
        source_path_observations: Vec<SourcePathChangeObservation>,
    ) -> Self {
        self.source_path_observations = source_path_observations;
        self
    }

    fn source_paths_are_current(
        &self,
        workspace_identity: Option<&WorkspaceEvidenceIdentity>,
        git_workspace: Option<&GitWorkspaceCache>,
    ) -> bool {
        self.revision
            .as_ref()
            .and_then(|identity| identity.repository_root.as_ref())
            .is_some_and(|captured_root| {
                workspace_identity.and_then(|identity| identity.repository_root.as_ref())
                    == Some(captured_root)
            })
            && !self.source_dependencies.is_empty()
            && self.source_path_observations.len() == self.source_dependencies.len()
            && git_workspace.is_some_and(|cache| {
                self.source_path_observations
                    .iter()
                    .all(|observation| cache.source_path_change_observation_is_current(observation))
            })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ToolHistoryMutation {
    RegisterCandidate {
        candidate: ToolHistoryCandidate,
    },
    RegisterWorkspaceEvidence {
        observation: WorkspaceEvidenceObservation,
    },
    RegisterNonWorkspaceCodeModeCall {
        call_id: String,
    },
    InvalidateSourceDependencies {
        affected_paths: Option<BTreeSet<PathBuf>>,
        current_workspace_identity: Option<WorkspaceEvidenceIdentity>,
        excluded_call_ids: BTreeSet<String>,
    },
    MarkConsumed {
        call_ids: BTreeSet<String>,
        generation: ModelGenerationId,
    },
}

impl ToolHistoryMutation {
    pub(crate) fn apply(&self, state: &mut ToolHistoryState) -> bool {
        match self {
            Self::RegisterCandidate { candidate } => {
                state.register(candidate.clone());
                true
            }
            Self::RegisterWorkspaceEvidence { observation } => {
                state.register_workspace_evidence(observation.clone());
                true
            }
            Self::RegisterNonWorkspaceCodeModeCall { call_id } => {
                state.register_non_workspace_code_mode_call(call_id.clone());
                true
            }
            Self::InvalidateSourceDependencies {
                affected_paths,
                current_workspace_identity,
                excluded_call_ids,
            } => state.invalidate_source_dependencies_excluding_call_ids(
                affected_paths.as_ref(),
                current_workspace_identity.as_ref(),
                excluded_call_ids,
            ),
            Self::MarkConsumed {
                call_ids,
                generation,
            } => state.mark_call_ids_consumed(call_ids, generation),
        }
    }
}

impl ToolHistoryState {
    fn is_persisted_empty(&self) -> bool {
        self.candidates.is_empty()
            && self.workspace_evidence.is_empty()
            && self.non_workspace_code_mode_calls.is_empty()
    }

    pub(crate) fn register(&mut self, mut candidate: ToolHistoryCandidate) {
        candidate.refresh_derived();
        let call_id = candidate.call_id.clone();
        let artifact_id = candidate.artifact_id.clone();
        let replaced = self.candidates.insert(call_id.clone(), candidate);

        if let Some(replaced) = replaced
            && replaced.artifact_id != artifact_id
            && self.artifact_call_ids.get(&replaced.artifact_id) == Some(&call_id)
        {
            self.rebuild_artifact_mapping(&replaced.artifact_id);
        }

        self.artifact_call_ids
            .entry(artifact_id)
            .and_modify(|selected_call_id| {
                if call_id < *selected_call_id {
                    selected_call_id.clone_from(&call_id);
                }
            })
            .or_insert(call_id);
    }

    fn refresh_derived_and_indexes(&mut self) {
        for candidate in self.candidates.values_mut() {
            candidate.refresh_derived();
        }
        self.rebuild_artifact_index();
    }

    fn rebuild_artifact_index(&mut self) {
        self.artifact_call_ids.clear();
        for (call_id, candidate) in &self.candidates {
            self.artifact_call_ids
                .entry(candidate.artifact_id.clone())
                .or_insert_with(|| call_id.clone());
        }
    }

    fn rebuild_artifact_mapping(&mut self, artifact_id: &str) {
        self.artifact_call_ids.remove(artifact_id);
        if let Some((call_id, _)) = self
            .candidates
            .iter()
            .find(|(_, candidate)| candidate.artifact_id == artifact_id)
        {
            self.artifact_call_ids
                .insert(artifact_id.to_string(), call_id.clone());
        }
    }

    #[cfg(test)]
    pub(crate) fn invalidate_source_dependencies(
        &mut self,
        affected_paths: Option<&BTreeSet<PathBuf>>,
        current_workspace_identity: Option<&WorkspaceEvidenceIdentity>,
    ) -> bool {
        self.invalidate_source_dependencies_excluding_call_ids(
            affected_paths,
            current_workspace_identity,
            &BTreeSet::new(),
        )
    }

    pub(crate) fn invalidate_source_dependencies_excluding_call_ids(
        &mut self,
        affected_paths: Option<&BTreeSet<PathBuf>>,
        current_workspace_identity: Option<&WorkspaceEvidenceIdentity>,
        excluded_call_ids: &BTreeSet<String>,
    ) -> bool {
        let normalized_affected = affected_paths.map(|paths| {
            paths
                .iter()
                .map(|path| normalized_source_path(path))
                .collect::<BTreeSet<_>>()
        });
        let mut changed = false;
        for (call_id, candidate) in &mut self.candidates {
            if excluded_call_ids.contains(call_id) {
                continue;
            }
            if !candidate.source_dependencies_current {
                continue;
            }
            let affected = if candidate.source_dependencies.is_empty() {
                tool_observes_workspace(&candidate.tool_identity)
            } else {
                normalized_affected.as_ref().is_none_or(|paths| {
                    candidate
                        .source_dependencies
                        .iter()
                        .any(|dependency| affected_paths_overlap_dependency(paths, dependency))
                })
            };
            if affected {
                candidate.source_dependencies_current = false;
                candidate.refresh_derived();
                changed = true;
            }
        }
        for (call_id, observation) in &mut self.workspace_evidence {
            if excluded_call_ids.contains(call_id) {
                continue;
            }
            if !observation.source_dependencies_current {
                continue;
            }
            let affected = observation.source_dependencies.is_empty()
                || normalized_affected.as_ref().is_none_or(|paths| {
                    observation
                        .source_dependencies
                        .iter()
                        .any(|dependency| affected_paths_overlap_dependency(paths, dependency))
                });
            if affected {
                observation.source_dependencies_current = false;
                changed = true;
            } else if observation.revision.as_ref() != current_workspace_identity {
                // An exact, disjoint mutation is the proof that permits this
                // dependency-scoped result to advance to the new repository
                // identity. Unobserved external identity changes still fail closed.
                observation.revision = current_workspace_identity.cloned();
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn register_workspace_evidence(
        &mut self,
        observation: WorkspaceEvidenceObservation,
    ) {
        self.workspace_evidence
            .entry(observation.call_id.clone())
            .or_insert(observation);
    }

    #[cfg(test)]
    pub(crate) fn workspace_evidence_revision_for_test(
        &self,
        call_id: &str,
    ) -> Option<Option<WorkspaceEvidenceIdentity>> {
        self.workspace_evidence
            .get(call_id)
            .map(|observation| observation.revision.clone())
    }

    pub(crate) fn register_non_workspace_code_mode_call(&mut self, call_id: String) {
        self.workspace_evidence.remove(&call_id);
        self.non_workspace_code_mode_calls.insert(call_id);
    }

    #[cfg(test)]
    pub(crate) fn consumed_outputs_for_tool(&self, tool_identity: &str) -> Vec<(String, String)> {
        self.candidates
            .values()
            .filter(|candidate| {
                candidate.tool_identity == tool_identity
                    && candidate.consumed_by_generation.is_some()
            })
            .map(|candidate| {
                (
                    candidate.call_id.clone(),
                    candidate.bounded_model_output.clone(),
                )
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn mark_consumed(
        &mut self,
        input: &[ResponseItem],
        generation: ModelGenerationId,
    ) -> bool {
        !self.mark_consumed_with_delta(input, generation).is_empty()
    }

    pub(crate) fn mark_consumed_with_delta(
        &mut self,
        input: &[ResponseItem],
        generation: ModelGenerationId,
    ) -> BTreeSet<String> {
        struct ExposedOutputIdentity<'a> {
            text: Cow<'a, str>,
            output_sha256: String,
        }
        let exposed = input
            .iter()
            .filter_map(canonical_textual_output_identity)
            .map(|(call_id, text)| {
                let output_sha256 = sha256(text.as_bytes());
                (
                    call_id,
                    ExposedOutputIdentity {
                        text,
                        output_sha256,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut changed_call_ids = BTreeSet::new();
        for candidate in self.candidates.values_mut() {
            if candidate.consumed_by_generation.is_some() {
                continue;
            }
            if exposed
                .get(candidate.call_id.as_str())
                .is_some_and(|output| {
                    output.output_sha256 == candidate.derived.bounded_model_output_sha256
                        || serde_json::from_str::<ToolHistoryReceipt>(&output.text)
                            .is_ok_and(|receipt| candidate.matches_parsed_receipt(&receipt))
                })
            {
                candidate.consumed_by_generation = Some(generation.clone());
                changed_call_ids.insert(candidate.call_id.clone());
            }
        }
        changed_call_ids
    }

    fn mark_call_ids_consumed(
        &mut self,
        call_ids: &BTreeSet<String>,
        generation: &ModelGenerationId,
    ) -> bool {
        let mut changed = false;
        for call_id in call_ids {
            if let Some(candidate) = self.candidates.get_mut(call_id)
                && candidate.consumed_by_generation.is_none()
            {
                candidate.consumed_by_generation = Some(generation.clone());
                changed = true;
            }
        }
        changed
    }

    #[cfg(test)]
    pub(crate) fn project(&self, items: Arc<[ResponseItem]>) -> ToolHistoryProjection {
        self.project_inner(items, None, None)
    }

    pub(crate) fn project_with_workspace_identity(
        &self,
        items: Arc<[ResponseItem]>,
        workspace_identity: Option<&WorkspaceEvidenceIdentity>,
    ) -> ToolHistoryProjection {
        self.project_inner(items, Some(workspace_identity), None)
    }

    pub(crate) fn project_with_workspace_cache(
        &self,
        items: Arc<[ResponseItem]>,
        workspace_identity: Option<&WorkspaceEvidenceIdentity>,
        git_workspace: &GitWorkspaceCache,
    ) -> ToolHistoryProjection {
        self.project_inner(items, Some(workspace_identity), Some(git_workspace))
    }

    pub(crate) fn project_workspace_freshness_with_cache(
        &self,
        items: Arc<[ResponseItem]>,
        workspace_identity: Option<&WorkspaceEvidenceIdentity>,
        git_workspace: &GitWorkspaceCache,
    ) -> ToolHistoryProjection {
        let mut projected = ProjectedResponseItems::Shared(items);
        self.invalidate_stale_workspace_evidence(
            &mut projected,
            workspace_identity,
            Some(git_workspace),
        );
        let projected = projected.into_shared();
        ToolHistoryProjection {
            items: Arc::clone(&projected),
            unreplaced_items: projected,
            substitutions: Arc::from([]),
        }
    }

    pub(crate) fn requires_workspace_evidence_validation(&self, items: &[ResponseItem]) -> bool {
        !self.workspace_evidence_requirements(items).is_empty()
    }

    fn project_inner(
        &self,
        items: Arc<[ResponseItem]>,
        workspace_identity: Option<Option<&WorkspaceEvidenceIdentity>>,
        git_workspace: Option<&GitWorkspaceCache>,
    ) -> ToolHistoryProjection {
        let mut projected = ProjectedResponseItems::Shared(items);
        if let Some(workspace_identity) = workspace_identity {
            self.invalidate_stale_workspace_evidence(
                &mut projected,
                workspace_identity,
                git_workspace,
            );
        }
        let tool_search_arguments = projected
            .iter()
            .filter_map(|item| match item {
                ResponseItem::ToolSearchCall {
                    call_id: Some(call_id),
                    arguments,
                    ..
                } => Some((call_id.clone(), arguments.clone())),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        let exposed_output_sha256 = projected
            .iter()
            .filter_map(canonical_textual_output_identity)
            .map(|(call_id, output)| (call_id.to_string(), sha256(output.as_bytes())))
            .collect::<BTreeMap<_, _>>();
        let mut latest_supersession = BTreeMap::<String, String>::new();
        let mut superseded_call_ids = BTreeSet::new();
        for item in projected.iter() {
            let Some((call_id, _output)) = canonical_textual_output_identity(item) else {
                continue;
            };
            let Some(candidate) = self.candidates.get(call_id) else {
                continue;
            };
            if exposed_output_sha256.get(call_id)
                != Some(&candidate.derived.bounded_model_output_sha256)
            {
                continue;
            }
            let Some(identity) = candidate.supersession_identity.as_ref() else {
                continue;
            };
            if !action_bound_supersession_identity(identity) {
                continue;
            }
            if let Some(previous_call_id) =
                latest_supersession.insert(identity.clone(), call_id.to_string())
                && self
                    .candidates
                    .get(&previous_call_id)
                    .is_some_and(|previous| previous.consumed_by_generation.is_some())
            {
                superseded_call_ids.insert(previous_call_id);
            }
        }
        if !superseded_call_ids.is_empty() {
            projected.retain(|item| {
                item_call_id(item).is_none_or(|call_id| !superseded_call_ids.contains(call_id))
            });
        }

        #[derive(Debug)]
        struct AdmissionCandidate {
            priority: u8,
            item_index: std::cmp::Reverse<usize>,
            call_id: String,
            structured_tokens: Option<usize>,
        }

        let mut admission_candidates = projected
            .iter()
            .enumerate()
            .filter_map(|(item_index, item)| {
                let (call_id, output) = canonical_textual_output_identity(item)?;
                let candidate = self.candidates.get(call_id)?;
                (exposed_output_sha256.get(call_id)
                    == Some(&candidate.derived.bounded_model_output_sha256))
                .then(|| AdmissionCandidate {
                    priority: admission_priority(candidate, &output),
                    item_index: std::cmp::Reverse(item_index),
                    call_id: call_id.to_string(),
                    structured_tokens: None,
                })
            })
            .collect::<Vec<_>>();
        admission_candidates.extend(projected.iter().enumerate().filter_map(
            |(item_index, item)| {
                let ResponseItem::ToolSearchOutput {
                    call_id: Some(call_id),
                    status,
                    tools,
                    ..
                } = item
                else {
                    return None;
                };
                let serialized = serde_json::to_string(item).ok()?;
                Some(AdmissionCandidate {
                    priority: tool_search_admission_priority(status, tools),
                    item_index: std::cmp::Reverse(item_index),
                    call_id: call_id.clone(),
                    structured_tokens: Some(approx_token_count(&serialized)),
                })
            },
        ));
        admission_candidates.sort_unstable_by(|left, right| {
            (&left.priority, &left.item_index, &left.call_id).cmp(&(
                &right.priority,
                &right.item_index,
                &right.call_id,
            ))
        });

        #[derive(Debug)]
        enum AdmissionRepresentation {
            Raw,
            Receipt { receipt_id: String, text: String },
            ArtifactPin { text: String },
            StructuredReceipt { item: ResponseItem },
            Drop,
        }
        #[derive(Debug)]
        struct AdmissionDecision {
            representation: AdmissionRepresentation,
            retain_raw_fallback: bool,
        }

        // Protect the cheapest recoverable form of later candidates before a
        // higher-priority raw result spends the shared budget. This keeps Drop
        // as the fallback for genuine aggregate pressure, not single-result
        // monopolization.
        let cheapest_receiptable_representation_tokens =
            |admission_candidate: &AdmissionCandidate| -> usize {
                let item_index = admission_candidate.item_index.0;
                if let Some(raw_tokens) = admission_candidate.structured_tokens {
                    return projected
                        .get(item_index)
                        .and_then(|item| {
                            tool_search_receipt_item(
                                item,
                                tool_search_arguments.get(&admission_candidate.call_id),
                            )
                        })
                        .map(|(_, receipt_tokens)| raw_tokens.min(receipt_tokens))
                        .filter(|tokens| *tokens <= MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET)
                        .unwrap_or(0);
                }

                let Some((_, output)) = projected
                    .get(item_index)
                    .and_then(canonical_textual_output_identity)
                else {
                    return 0;
                };
                let Some(candidate) = self.candidates.get(&admission_candidate.call_id) else {
                    return 0;
                };
                let non_text_tokens =
                    usize::try_from(candidate.preserved_non_text_tokens).unwrap_or(usize::MAX);
                let raw_tokens = approx_token_count(&output).saturating_add(non_text_tokens);
                let receipt_tokens = candidate
                    .admission_receipt()
                    .map(|(_, _, receipt_tokens)| {
                        let receipt_tokens = usize::try_from(receipt_tokens)
                            .unwrap_or(usize::MAX)
                            .saturating_add(non_text_tokens);
                        raw_tokens.min(receipt_tokens)
                    })
                    .filter(|tokens| *tokens <= MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET);
                let pin_tokens = (non_text_tokens == 0)
                    .then(|| candidate.artifact_pin().map(|(_, tokens)| tokens))
                    .flatten()
                    .filter(|tokens| *tokens <= MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET);
                [Some(raw_tokens), receipt_tokens, pin_tokens]
                    .into_iter()
                    .flatten()
                    .min()
                    .unwrap_or(0)
            };
        let mut reserved_competing_tokens = admission_candidates
            .iter()
            .map(&cheapest_receiptable_representation_tokens)
            .fold(0usize, usize::saturating_add);
        let mut decisions = BTreeMap::<String, AdmissionDecision>::new();
        let mut remaining_tokens = MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET;
        let mut remaining_fallback_tokens = MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET;
        for admission_candidate in admission_candidates {
            reserved_competing_tokens = reserved_competing_tokens.saturating_sub(
                cheapest_receiptable_representation_tokens(&admission_candidate),
            );
            let remaining_raw_tokens = remaining_tokens.saturating_sub(reserved_competing_tokens);
            let item_index = admission_candidate.item_index.0;
            let call_id = admission_candidate.call_id;
            if let Some(raw_tokens) = admission_candidate.structured_tokens {
                let receipt = projected.get(item_index).and_then(|item| {
                    tool_search_receipt_item(item, tool_search_arguments.get(&call_id))
                });
                let (representation, retain_raw_fallback) = if raw_tokens <= remaining_raw_tokens {
                    remaining_tokens = remaining_tokens.saturating_sub(raw_tokens);
                    remaining_fallback_tokens =
                        remaining_fallback_tokens.saturating_sub(raw_tokens);
                    (AdmissionRepresentation::Raw, true)
                } else if let Some((item, receipt_tokens)) = receipt
                    && receipt_tokens <= remaining_tokens
                {
                    remaining_tokens = remaining_tokens.saturating_sub(receipt_tokens);
                    let retain_raw = raw_tokens <= remaining_fallback_tokens;
                    if retain_raw {
                        remaining_fallback_tokens =
                            remaining_fallback_tokens.saturating_sub(raw_tokens);
                    }
                    (
                        AdmissionRepresentation::StructuredReceipt { item },
                        retain_raw,
                    )
                } else {
                    (AdmissionRepresentation::Drop, false)
                };
                decisions.insert(
                    call_id,
                    AdmissionDecision {
                        representation,
                        retain_raw_fallback,
                    },
                );
                continue;
            }
            let Some((_, output)) = projected
                .get(item_index)
                .and_then(canonical_textual_output_identity)
            else {
                continue;
            };
            let Some(candidate) = self.candidates.get(&call_id) else {
                continue;
            };
            let non_text_tokens =
                usize::try_from(candidate.preserved_non_text_tokens).unwrap_or(usize::MAX);
            let raw_tokens = approx_token_count(&output).saturating_add(non_text_tokens);
            let savings_receipt = candidate
                .receipt()
                .map(|(receipt_id, text, receipt_tokens)| {
                    let tokens = usize::try_from(receipt_tokens)
                        .unwrap_or(usize::MAX)
                        .saturating_add(non_text_tokens);
                    (receipt_id, text, tokens)
                });
            let admission_receipt =
                candidate
                    .admission_receipt()
                    .map(|(receipt_id, text, receipt_tokens)| {
                        let tokens = usize::try_from(receipt_tokens)
                            .unwrap_or(usize::MAX)
                            .saturating_add(non_text_tokens);
                        (receipt_id, text, tokens)
                    });
            let artifact_pin = (non_text_tokens == 0)
                .then(|| candidate.artifact_pin())
                .flatten();

            let decision = if raw_tokens <= remaining_raw_tokens {
                if candidate.consumed_by_generation.is_some()
                    && let Some((receipt_id, text, receipt_tokens)) = savings_receipt
                    && receipt_tokens <= raw_tokens
                    && receipt_tokens <= remaining_tokens
                {
                    remaining_tokens = remaining_tokens.saturating_sub(receipt_tokens);
                    AdmissionDecision {
                        representation: AdmissionRepresentation::Receipt {
                            receipt_id: receipt_id.to_string(),
                            text: text.to_string(),
                        },
                        retain_raw_fallback: if raw_tokens <= remaining_fallback_tokens {
                            remaining_fallback_tokens =
                                remaining_fallback_tokens.saturating_sub(raw_tokens);
                            true
                        } else {
                            false
                        },
                    }
                } else {
                    remaining_tokens = remaining_tokens.saturating_sub(raw_tokens);
                    remaining_fallback_tokens =
                        remaining_fallback_tokens.saturating_sub(raw_tokens);
                    AdmissionDecision {
                        representation: AdmissionRepresentation::Raw,
                        retain_raw_fallback: true,
                    }
                }
            } else if let Some((receipt_id, text, receipt_tokens)) = admission_receipt
                && receipt_tokens <= remaining_tokens
            {
                remaining_tokens = remaining_tokens.saturating_sub(receipt_tokens);
                AdmissionDecision {
                    representation: AdmissionRepresentation::Receipt {
                        receipt_id: receipt_id.to_string(),
                        text: text.to_string(),
                    },
                    retain_raw_fallback: if raw_tokens <= remaining_fallback_tokens {
                        remaining_fallback_tokens =
                            remaining_fallback_tokens.saturating_sub(raw_tokens);
                        true
                    } else {
                        false
                    },
                }
            } else if let Some((text, pin_tokens)) = artifact_pin {
                // Exact recovery handles are the irreducible representation of a textual result.
                // Treat the aggregate budget as soft for this final form: dropping the handle
                // forces a later search or rerun and increases total turn cost.
                remaining_tokens = remaining_tokens.saturating_sub(pin_tokens);
                AdmissionDecision {
                    representation: AdmissionRepresentation::ArtifactPin { text },
                    retain_raw_fallback: false,
                }
            } else {
                AdmissionDecision {
                    representation: AdmissionRepresentation::Drop,
                    retain_raw_fallback: false,
                }
            };
            decisions.insert(call_id, decision);
        }

        let mut unreplaced_projected = projected.clone();
        unreplaced_projected.retain(|item| {
            item_call_id(item).is_none_or(|call_id| {
                decisions.get(call_id).is_none_or(|decision| {
                    decision.retain_raw_fallback
                        || matches!(
                            &decision.representation,
                            AdmissionRepresentation::Receipt { .. }
                                | AdmissionRepresentation::ArtifactPin { .. }
                        )
                })
            })
        });
        projected.retain(|item| {
            item_call_id(item).is_none_or(|call_id| {
                decisions.get(call_id).is_none_or(|decision| {
                    !matches!(&decision.representation, AdmissionRepresentation::Drop)
                })
            })
        });

        if decisions.values().any(|decision| {
            matches!(
                &decision.representation,
                AdmissionRepresentation::StructuredReceipt { .. }
            )
        }) {
            for item in projected.make_owned().iter_mut() {
                let call_id = match item {
                    ResponseItem::ToolSearchCall {
                        call_id: Some(call_id),
                        ..
                    }
                    | ResponseItem::ToolSearchOutput {
                        call_id: Some(call_id),
                        ..
                    } => call_id,
                    _ => continue,
                };
                let Some(AdmissionDecision {
                    representation:
                        AdmissionRepresentation::StructuredReceipt { item: receipt_item },
                    ..
                }) = decisions.get(call_id)
                else {
                    continue;
                };
                match item {
                    ResponseItem::ToolSearchOutput { .. } => *item = receipt_item.clone(),
                    ResponseItem::ToolSearchCall { arguments, .. } => {
                        let ResponseItem::ToolSearchOutput { tools, .. } = receipt_item else {
                            continue;
                        };
                        let Some(receipt_arguments) = tools
                            .first()
                            .and_then(|value| value.get("receipt"))
                            .and_then(|value| value.get("arguments"))
                        else {
                            continue;
                        };
                        *arguments = receipt_arguments.clone();
                    }
                    _ => {}
                }
            }
        }

        let mut substitutions = Vec::new();
        if decisions.values().any(|decision| {
            matches!(
                &decision.representation,
                AdmissionRepresentation::Receipt { .. }
                    | AdmissionRepresentation::ArtifactPin { .. }
            )
        }) {
            for (item_index, item) in projected.make_owned().iter_mut().enumerate() {
                let Some((call_id, body)) = textual_output_body_mut(item) else {
                    continue;
                };
                let Some(_output) = canonical_model_visible_output_text(body) else {
                    continue;
                };
                let Some(candidate) = self.candidates.get(call_id) else {
                    continue;
                };
                let bounded_output_sha256 = candidate.derived.bounded_model_output_sha256.clone();
                if exposed_output_sha256.get(call_id) != Some(&bounded_output_sha256) {
                    continue;
                }
                let Some(decision) = decisions.get(call_id) else {
                    continue;
                };
                let (text, receipt_id) = match &decision.representation {
                    AdmissionRepresentation::Receipt { receipt_id, text } => {
                        (text, Some(receipt_id))
                    }
                    AdmissionRepresentation::ArtifactPin { text } => (text, None),
                    _ => continue,
                };
                let substituted_output_sha256 = sha256(text.as_bytes());
                replace_model_visible_output_text(body, text.clone());
                if let Some(receipt_id) = receipt_id {
                    substitutions.push(ToolHistorySubstitution {
                        item_index,
                        call_id: call_id.to_string(),
                        bounded_output_sha256,
                        receipt_id: receipt_id.clone(),
                        substituted_output_sha256,
                    });
                }
            }
        }
        if decisions.values().any(|decision| {
            !decision.retain_raw_fallback
                && matches!(
                    &decision.representation,
                    AdmissionRepresentation::Receipt { .. }
                        | AdmissionRepresentation::ArtifactPin { .. }
                )
        }) {
            for item in unreplaced_projected.make_owned().iter_mut() {
                let Some((call_id, body)) = textual_output_body_mut(item) else {
                    continue;
                };
                let Some(candidate) = self.candidates.get(call_id) else {
                    continue;
                };
                if exposed_output_sha256.get(call_id)
                    != Some(&candidate.derived.bounded_model_output_sha256)
                {
                    continue;
                }
                let Some(decision) = decisions.get(call_id) else {
                    continue;
                };
                if decision.retain_raw_fallback
                    || !matches!(
                        &decision.representation,
                        AdmissionRepresentation::Receipt { .. }
                            | AdmissionRepresentation::ArtifactPin { .. }
                    )
                {
                    continue;
                }
                let Some((text, _)) = candidate.artifact_pin() else {
                    continue;
                };
                replace_model_visible_output_text(body, text);
            }
        }
        ToolHistoryProjection {
            items: projected.into_shared(),
            unreplaced_items: unreplaced_projected.into_shared(),
            substitutions: Arc::from(substitutions),
        }
    }

    fn invalidate_stale_workspace_evidence(
        &self,
        items: &mut ProjectedResponseItems,
        workspace_identity: Option<&WorkspaceEvidenceIdentity>,
        git_workspace: Option<&GitWorkspaceCache>,
    ) {
        let requirements = self.workspace_evidence_requirements(items);

        for item_index in 0..items.len() {
            let replacement = {
                let item = &items[item_index];
                let Some((call_id, output)) = canonical_textual_output_identity(item) else {
                    continue;
                };
                let Some(origin_call_id) = requirements.get(call_id) else {
                    continue;
                };
                let observation = self.workspace_evidence.get(origin_call_id);
                let revision_matches = observation.is_some_and(|observation| {
                    observation.source_dependencies_current
                        && ((observation.revision.as_ref() == workspace_identity)
                            || observation
                                .source_paths_are_current(workspace_identity, git_workspace))
                });
                let output_matches = origin_call_id != call_id
                    || observation.is_some_and(|observation| {
                        observation.output_sha256 == sha256(output.as_bytes())
                    });
                if revision_matches && output_matches {
                    continue;
                }
                let reason = if observation.is_some_and(|observation| {
                    !observation.source_dependencies.is_empty()
                        && !observation.source_dependencies_current
                }) {
                    "a source dependency changed after this tool result was captured; rerun the tool before relying on it"
                } else {
                    "the repository identity is unavailable or changed after this tool result was captured; rerun the tool before relying on it"
                };
                Some((call_id.to_string(), reason))
            };
            let Some((call_id, reason)) = replacement else {
                continue;
            };
            let Some((_call_id, body)) =
                textual_output_body_mut(&mut items.make_owned()[item_index])
            else {
                continue;
            };
            replace_model_visible_output_text(
                body,
                serde_json::json!({
                    "call_id": call_id,
                    "reason": reason,
                    "stale_workspace_evidence": true,
                })
                .to_string(),
            );
        }
    }

    fn workspace_evidence_requirements(&self, items: &[ResponseItem]) -> BTreeMap<String, String> {
        let mut requirements = BTreeMap::<String, String>::new();
        for item in items.iter() {
            let (name, arguments, call_id) = match item {
                ResponseItem::FunctionCall {
                    name,
                    arguments,
                    call_id,
                    ..
                } => (name, arguments, call_id),
                ResponseItem::CustomToolCall {
                    name,
                    input,
                    call_id,
                    ..
                } => (name, input, call_id),
                _ => continue,
            };
            // Code-mode's `functions.exec` carrier can contain repository
            // reads even though the carrier itself is not a host executable.
            // Explicitly registered evidence is authoritative for any other
            // tool whose current classifier no longer exposes that detail.
            let code_mode_workspace_state_unknown = name == "functions.exec"
                && !self.non_workspace_code_mode_calls.contains(call_id)
                && !self.workspace_evidence.contains_key(call_id);
            let call_observes_workspace = code_mode_workspace_state_unknown
                || tool_call_observes_workspace_parts(name, arguments);
            if call_observes_workspace || self.workspace_evidence.contains_key(call_id) {
                requirements.insert(call_id.clone(), call_id.clone());
                continue;
            }
            if name != "read_tool_output" {
                continue;
            }
            let Some(artifact_id) = read_tool_output_artifact_id(arguments) else {
                continue;
            };
            let origin = self
                .artifact_call_ids
                .get(&artifact_id)
                .and_then(|call_id| self.candidates.get(call_id));
            match origin {
                Some(candidate)
                    if (candidate.tool_identity == "functions.exec"
                        && !self
                            .non_workspace_code_mode_calls
                            .contains(&candidate.call_id))
                        || self.workspace_evidence.contains_key(&candidate.call_id) =>
                {
                    requirements.insert(call_id.clone(), candidate.call_id.clone());
                }
                Some(_) => {}
                None => {
                    // Legacy or missing provenance cannot safely establish that recovered
                    // output was independent of the workspace revision.
                    requirements.insert(call_id.clone(), call_id.clone());
                }
            };
        }
        requirements
    }

    pub(crate) fn retain_for_history(&mut self, items: &[ResponseItem]) {
        let mut live = items
            .iter()
            .filter_map(output_call_id)
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        for item in items {
            let (ResponseItem::FunctionCall {
                name, arguments, ..
            }
            | ResponseItem::CustomToolCall {
                name,
                input: arguments,
                ..
            }) = item
            else {
                continue;
            };
            if name != "read_tool_output" {
                continue;
            }
            let Some(artifact_id) = read_tool_output_artifact_id(arguments) else {
                continue;
            };
            if let Some(origin_call_id) = self.artifact_call_ids.get(&artifact_id) {
                live.insert(origin_call_id.clone());
            }
        }
        for candidate in self.candidates.values() {
            if items
                .iter()
                .any(|item| response_item_references_artifact(item, candidate))
            {
                live.insert(candidate.call_id.clone());
            }
        }
        self.candidates.retain(|call_id, _| live.contains(call_id));
        self.workspace_evidence
            .retain(|call_id, _| live.contains(call_id));
        self.non_workspace_code_mode_calls
            .retain(|call_id| live.contains(call_id));
        self.rebuild_artifact_index();
    }

    pub(crate) fn artifact_references(&self) -> BTreeMap<String, (u64, String)> {
        self.candidates
            .values()
            .map(|candidate| {
                (
                    candidate.artifact_id.clone(),
                    candidate.artifact_reference(),
                )
            })
            .collect()
    }

    /// Builds a deterministic, exact recovery sidecar for artifacts referenced by a projected
    /// prompt. Compaction appends this separately from model-authored prose so retention does not
    /// depend on the model copying a receipt byte-for-byte into its summary.
    pub(crate) fn artifact_pin_payload_for_items(&self, items: &[ResponseItem]) -> Option<String> {
        let pins = self
            .candidates
            .values()
            .filter(|candidate| {
                items
                    .iter()
                    .any(|item| response_item_references_artifact(item, candidate))
            })
            .filter_map(ToolHistoryCandidate::artifact_pin_value)
            .collect::<Vec<_>>();
        if pins.is_empty() {
            return None;
        }
        serde_json::to_string(&serde_json::json!({
            "version": 1,
            "kind": "tool_history_artifact_pins",
            "instruction": "Use read_tool_output with an artifact_id below to recover exact prior output.",
            "artifacts": pins,
        }))
        .ok()
    }

    fn retain_retrievable_artifacts(
        &mut self,
        expected: &BTreeMap<String, (u64, String)>,
        live: &BTreeSet<String>,
    ) {
        self.candidates.retain(|_, candidate| {
            let reference = candidate.artifact_reference();
            live.contains(&candidate.artifact_id)
                && expected.get(&candidate.artifact_id) == Some(&reference)
        });
        self.rebuild_artifact_index();
    }
}

fn read_tool_output_artifact_id(arguments: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| value.get("artifact_id")?.as_str().map(str::to_string))
}

fn action_bound_supersession_identity(identity: &str) -> bool {
    let mut parts = identity.rsplitn(3, ':');
    let Some(result_sha256) = parts.next() else {
        return false;
    };
    let Some(invocation_sha256) = parts.next() else {
        return false;
    };
    parts.next().is_some() && is_sha256_hex(invocation_sha256) && is_sha256_hex(result_sha256)
}

fn default_true() -> bool {
    true
}

// Serde's `skip_serializing_if` callback contract passes the field by reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(value: &bool) -> bool {
    *value
}

fn normalized_source_path(path: &Path) -> String {
    normalized_source_path_with_case_sensitivity(path, cfg!(not(windows)))
}

fn normalized_source_path_with_case_sensitivity(path: &Path, case_sensitive: bool) -> String {
    let mut lexical = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => match lexical.components().next_back() {
                Some(std::path::Component::Normal(_)) => {
                    lexical.pop();
                }
                Some(std::path::Component::ParentDir) | None if !path.is_absolute() => {
                    lexical.push(component.as_os_str());
                }
                _ => {}
            },
            _ => lexical.push(component.as_os_str()),
        }
    }
    let normalized = lexical.to_string_lossy().replace('\\', "/");

    let normalized = if case_sensitive {
        normalized
    } else {
        normalized.to_ascii_lowercase()
    };
    let trimmed = normalized.trim_end_matches('/');
    if trimmed.is_empty() {
        normalized
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
fn source_dependency_overlaps(dependency: &SourceDependencyV1, changed: &str) -> bool {
    changed == dependency.path
        || dependency.recursive
            && changed
                .strip_prefix(&dependency.path)
                .is_some_and(|suffix| suffix.starts_with('/'))
        || dependency
            .path
            .strip_prefix(changed)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn affected_paths_overlap_dependency(
    affected_paths: &BTreeSet<String>,
    dependency: &SourceDependencyV1,
) -> bool {
    if affected_paths.contains(&dependency.path) {
        return true;
    }
    if dependency.recursive {
        let descendant_prefix = format!("{}/", dependency.path);
        if affected_paths
            .range(descendant_prefix.clone()..)
            .next()
            .is_some_and(|path| path.starts_with(&descendant_prefix))
        {
            return true;
        }
    }

    let mut ancestor = dependency.path.as_str();
    while let Some((parent, _)) = ancestor.rsplit_once('/') {
        if affected_paths.contains(parent) {
            return true;
        }
        if parent.is_empty() {
            return false;
        }
        ancestor = parent;
    }
    false
}

#[derive(Deserialize, Serialize)]
struct ToolHistoryLedgerFile {
    version: u8,
    state: ToolHistoryState,
}

#[derive(Serialize)]
struct ToolHistoryLedgerRef<'a> {
    version: u8,
    state: &'a ToolHistoryState,
}

#[derive(Deserialize)]
struct ToolHistoryJournalRecord {
    version: u8,
    writer_id: String,
    sequence: u64,
    mutation: ToolHistoryMutation,
    checksum_sha256: String,
}

#[derive(Serialize)]
struct ToolHistoryJournalRecordRef<'a> {
    version: u8,
    writer_id: &'a str,
    sequence: u64,
    mutation: &'a ToolHistoryMutation,
    checksum_sha256: String,
}

#[derive(Serialize)]
struct ToolHistoryJournalChecksumRef<'a> {
    version: u8,
    writer_id: &'a str,
    sequence: u64,
    mutation: &'a ToolHistoryMutation,
}

enum ToolHistoryJournalLoadError {
    Corrupt(String),
    UnsupportedVersion(u8),
    Io(String),
}

#[derive(Debug)]
pub(crate) enum ToolHistoryLoadOutcome {
    Missing,
    Loaded(ToolHistoryState),
    Corrupt {
        path: std::path::PathBuf,
        error: String,
    },
    UnsupportedVersion {
        path: std::path::PathBuf,
        found: u8,
        supported: u8,
    },
    IoFailure {
        path: std::path::PathBuf,
        error: String,
    },
}

impl ToolHistoryLoadOutcome {
    pub(crate) fn into_state_and_warning(self) -> (ToolHistoryState, Option<String>) {
        match self {
            Self::Missing => (ToolHistoryState::default(), None),
            Self::Loaded(state) => (state, None),
            Self::Corrupt { path, error } => (
                ToolHistoryState::default(),
                Some(format!(
                    "Ignoring corrupt completed-tool history ledger {}: {error}",
                    path.display()
                )),
            ),
            Self::UnsupportedVersion {
                path,
                found,
                supported,
            } => (
                ToolHistoryState::default(),
                Some(format!(
                    "Ignoring completed-tool history ledger {} with unsupported version {found}; this build supports version {supported}",
                    path.display()
                )),
            ),
            Self::IoFailure { path, error } => (
                ToolHistoryState::default(),
                Some(format!(
                    "Could not read completed-tool history ledger {}: {error}",
                    path.display()
                )),
            ),
        }
    }
}

pub(crate) async fn load_tool_history_state(
    codex_home: &std::path::Path,
    thread_id: &str,
) -> ToolHistoryLoadOutcome {
    match load_tool_history_state_for_fork(codex_home, thread_id).await {
        ToolHistoryLoadOutcome::Loaded(state) => ToolHistoryLoadOutcome::Loaded(
            reconcile_tool_history_state(codex_home, thread_id, state).await,
        ),
        ToolHistoryLoadOutcome::Corrupt { path, error } => {
            let quarantine_path = corrupt_ledger_quarantine_path(&path);
            match tokio::fs::rename(&path, &quarantine_path).await {
                Ok(()) => ToolHistoryLoadOutcome::Corrupt {
                    path: quarantine_path.clone(),
                    error: format!(
                        "{error}; quarantined from {} to {}",
                        path.display(),
                        quarantine_path.display()
                    ),
                },
                Err(rename_error) => ToolHistoryLoadOutcome::Corrupt {
                    path,
                    error: format!("{error}; failed to quarantine ledger: {rename_error}"),
                },
            }
        }
        outcome => outcome,
    }
}

fn corrupt_ledger_quarantine_path(path: &std::path::Path) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_extension(format!("corrupt-{}-{nonce}", std::process::id()))
}

/// Reads a parent ledger for fork without reconciling the parent's protection markers.
///
/// The parent can still be live while the child is initialized. Mutating its artifact ownership
/// from the child would race with a parent tool result between marker creation and ledger persist.
pub(crate) async fn load_tool_history_state_for_fork(
    codex_home: &std::path::Path,
    thread_id: &str,
) -> ToolHistoryLoadOutcome {
    let path = ledger_path(codex_home, thread_id);
    let (mut state, checkpoint_exists) = match tokio::fs::read(&path).await {
        Ok(bytes) => match serde_json::from_slice::<ToolHistoryLedgerFile>(&bytes) {
            Ok(mut file) if file.version == LEDGER_VERSION => {
                file.state.refresh_derived_and_indexes();
                (file.state, true)
            }
            Ok(file) => {
                return ToolHistoryLoadOutcome::UnsupportedVersion {
                    path,
                    found: file.version,
                    supported: LEDGER_VERSION,
                };
            }
            Err(error) => {
                return ToolHistoryLoadOutcome::Corrupt {
                    path,
                    error: error.to_string(),
                };
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (ToolHistoryState::default(), false)
        }
        Err(error) => {
            return ToolHistoryLoadOutcome::IoFailure {
                path,
                error: error.to_string(),
            };
        }
    };
    let journal_path = journal_path(codex_home, thread_id);
    let journal_exists = match replay_tool_history_journal(&journal_path, &mut state).await {
        Ok(exists) => exists,
        Err(ToolHistoryJournalLoadError::Corrupt(error)) => {
            return ToolHistoryLoadOutcome::Corrupt {
                path: journal_path,
                error,
            };
        }
        Err(ToolHistoryJournalLoadError::UnsupportedVersion(found)) => {
            return ToolHistoryLoadOutcome::UnsupportedVersion {
                path: journal_path,
                found,
                supported: JOURNAL_VERSION,
            };
        }
        Err(ToolHistoryJournalLoadError::Io(error)) => {
            return ToolHistoryLoadOutcome::IoFailure {
                path: journal_path,
                error,
            };
        }
    };
    if !checkpoint_exists && !journal_exists {
        ToolHistoryLoadOutcome::Missing
    } else {
        state.refresh_derived_and_indexes();
        ToolHistoryLoadOutcome::Loaded(state)
    }
}

async fn replay_tool_history_journal(
    path: &std::path::Path,
    state: &mut ToolHistoryState,
) -> Result<bool, ToolHistoryJournalLoadError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(ToolHistoryJournalLoadError::Io(error.to_string())),
    };
    let mut offset = 0_usize;
    let mut writer_sequences = BTreeMap::<String, u64>::new();
    while let Some(relative_end) = bytes[offset..].iter().position(|byte| *byte == b'\n') {
        let end = offset + relative_end;
        let line = &bytes[offset..end];
        offset = end + 1;
        if line.is_empty() {
            continue;
        }
        let record = serde_json::from_slice::<ToolHistoryJournalRecord>(line)
            .map_err(|error| ToolHistoryJournalLoadError::Corrupt(error.to_string()))?;
        if record.version != JOURNAL_VERSION {
            return Err(ToolHistoryJournalLoadError::UnsupportedVersion(
                record.version,
            ));
        }
        let checksum = tool_history_journal_checksum(
            record.writer_id.as_str(),
            record.sequence,
            &record.mutation,
        )
        .map_err(ToolHistoryJournalLoadError::Corrupt)?;
        if checksum != record.checksum_sha256 {
            return Err(ToolHistoryJournalLoadError::Corrupt(format!(
                "journal checksum mismatch for writer {} sequence {}",
                record.writer_id, record.sequence
            )));
        }
        if let Some(previous) = writer_sequences.insert(record.writer_id.clone(), record.sequence)
            && record.sequence != previous.saturating_add(1)
        {
            return Err(ToolHistoryJournalLoadError::Corrupt(format!(
                "non-contiguous journal sequence for writer {}: {previous} then {}",
                record.writer_id, record.sequence
            )));
        }
        record.mutation.apply(state);
    }
    if offset != bytes.len() {
        tracing::warn!(
            path = %path.display(),
            trailing_bytes = bytes.len() - offset,
            "ignoring incomplete trailing completed-tool history journal record"
        );
    }
    Ok(true)
}

fn tool_history_journal_checksum(
    writer_id: &str,
    sequence: u64,
    mutation: &ToolHistoryMutation,
) -> Result<String, String> {
    let bytes = serde_json::to_vec(&ToolHistoryJournalChecksumRef {
        version: JOURNAL_VERSION,
        writer_id,
        sequence,
        mutation,
    })
    .map_err(|error| format!("failed to serialize tool-history journal checksum: {error}"))?;
    Ok(sha256(&bytes))
}

pub(crate) async fn reconcile_tool_history_state(
    codex_home: &std::path::Path,
    thread_id: &str,
    mut state: ToolHistoryState,
) -> ToolHistoryState {
    let expected = state.artifact_references();
    let live =
        reconcile_active_tool_history_artifact_protection(codex_home, thread_id, &expected).await;
    state.retain_retrievable_artifacts(&expected, &live);
    state
}

pub(crate) async fn remint_tool_history_state_for_fork(
    codex_home: &std::path::Path,
    source_thread_id: &str,
    target_thread_id: &str,
    state: ToolHistoryState,
) -> (ToolHistoryState, usize) {
    let workspace_evidence = state.workspace_evidence;
    let non_workspace_code_mode_calls = state.non_workspace_code_mode_calls;
    let mut reminted_by_identity = BTreeMap::<(String, u64, String), String>::new();
    let mut reminted_candidates = BTreeMap::new();
    let mut dropped_candidates = 0_usize;
    for (call_id, mut candidate) in state.candidates {
        let identity = (
            candidate.artifact_id.clone(),
            candidate.artifact_bytes,
            candidate.artifact_sha256.clone(),
        );
        let reminted_id = if let Some(reminted_id) = reminted_by_identity.get(&identity) {
            Some(reminted_id.clone())
        } else {
            match remint_tool_history_artifact_for_thread(
                codex_home,
                source_thread_id,
                target_thread_id,
                &candidate.artifact_id,
                candidate.artifact_bytes,
                &candidate.artifact_sha256,
            )
            .await
            {
                Ok(reminted_id) => {
                    reminted_by_identity.insert(identity, reminted_id.clone());
                    Some(reminted_id)
                }
                Err(err) => {
                    tracing::warn!(
                        call_id,
                        source_thread_id,
                        target_thread_id,
                        "failed to remint completed-tool artifact for fork: {err}"
                    );
                    None
                }
            }
        };
        let Some(reminted_id) = reminted_id else {
            dropped_candidates = dropped_candidates.saturating_add(1);
            continue;
        };
        candidate.artifact_id = reminted_id;
        candidate.refresh_derived();
        reminted_candidates.insert(call_id, candidate);
    }
    let mut reminted_state = ToolHistoryState {
        candidates: reminted_candidates,
        workspace_evidence,
        non_workspace_code_mode_calls,
        artifact_call_ids: BTreeMap::new(),
    };
    reminted_state.rebuild_artifact_index();
    (reminted_state, dropped_candidates)
}

pub(crate) async fn persist_tool_history_state(
    codex_home: &std::path::Path,
    thread_id: &str,
    state: &ToolHistoryState,
) -> Result<(), String> {
    let path = ledger_path(codex_home, thread_id);
    let journal_path = journal_path(codex_home, thread_id);
    if state.is_persisted_empty()
        && tool_history_storage_is_definitely_absent(&path, &journal_path).await
    {
        return Ok(());
    }
    let bytes = serde_json::to_vec(&ToolHistoryLedgerRef {
        version: LEDGER_VERSION,
        state,
    })
    .map_err(|err| format!("failed to serialize tool-history ledger: {err}"))?;
    #[cfg(test)]
    pause_tool_history_persistence_for_test_if_requested(thread_id).await;
    #[cfg(test)]
    fail_tool_history_persistence_for_test_if_requested(thread_id).await?;
    tokio::task::spawn_blocking(move || {
        let directory = path
            .parent()
            .ok_or_else(|| "tool-history ledger has no parent directory".to_string())?;
        std::fs::create_dir_all(directory)
            .map_err(|err| format!("failed to create tool-history ledger directory: {err}"))?;
        let mut temp = tempfile::NamedTempFile::new_in(directory)
            .map_err(|err| format!("failed to create tool-history ledger temporary: {err}"))?;
        temp.write_all(&bytes)
            .map_err(|err| format!("failed to write tool-history ledger: {err}"))?;
        temp.as_file_mut()
            .sync_all()
            .map_err(|err| format!("failed to sync tool-history ledger: {err}"))?;
        let installed = temp
            .persist(&path)
            .map_err(|err| format!("failed to install tool-history ledger: {}", err.error))?;
        installed
            .sync_all()
            .map_err(|err| format!("failed to sync installed tool-history ledger: {err}"))?;
        match std::fs::remove_file(&journal_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to clear compacted tool-history journal: {error}"
                ));
            }
        }
        sync_tool_history_ledger_directory(directory)?;

        Ok(())
    })
    .await
    .map_err(|err| format!("tool-history ledger writer failed: {err}"))?
}

async fn tool_history_storage_is_definitely_absent(
    ledger_path: &std::path::Path,
    journal_path: &std::path::Path,
) -> bool {
    for path in [ledger_path, journal_path] {
        match tokio::fs::try_exists(path).await {
            Ok(false) => {}
            Ok(true) | Err(_) => return false,
        }
    }
    true
}

pub(crate) async fn persist_tool_history_mutations(
    codex_home: &std::path::Path,
    thread_id: &str,
    writer_id: &str,
    mutations: &[(u64, ToolHistoryMutation)],
) -> Result<u64, String> {
    if mutations.is_empty() {
        return Ok(0);
    }
    let path = journal_path(codex_home, thread_id);
    let mut bytes = Vec::new();
    for (sequence, mutation) in mutations {
        let checksum_sha256 = tool_history_journal_checksum(writer_id, *sequence, mutation)?;
        serde_json::to_writer(
            &mut bytes,
            &ToolHistoryJournalRecordRef {
                version: JOURNAL_VERSION,
                writer_id,
                sequence: *sequence,
                mutation,
                checksum_sha256,
            },
        )
        .map_err(|error| format!("failed to serialize tool-history journal: {error}"))?;
        bytes.push(b'\n');
    }
    let persisted_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    #[cfg(test)]
    pause_tool_history_persistence_for_test_if_requested(thread_id).await;
    #[cfg(test)]
    fail_tool_history_persistence_for_test_if_requested(thread_id).await?;
    tokio::task::spawn_blocking(move || {
        let directory = path
            .parent()
            .ok_or_else(|| "tool-history journal has no parent directory".to_string())?;
        std::fs::create_dir_all(directory)
            .map_err(|error| format!("failed to create tool-history journal directory: {error}"))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("failed to open tool-history journal: {error}"))?;
        let existing_len = file
            .metadata()
            .map_err(|error| format!("failed to inspect tool-history journal: {error}"))?
            .len();
        if existing_len > 0 {
            file.seek(SeekFrom::Start(0))
                .map_err(|error| format!("failed to seek tool-history journal: {error}"))?;
            let mut existing = Vec::new();
            file.read_to_end(&mut existing)
                .map_err(|error| format!("failed to read tool-history journal: {error}"))?;
            if existing.last() != Some(&b'\n') {
                let complete_len = existing
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                    .map_or(0, |index| index.saturating_add(1));
                file.set_len(u64::try_from(complete_len).unwrap_or(0))
                    .map_err(|error| {
                        format!("failed to repair incomplete tool-history journal: {error}")
                    })?;
            }
        }
        file.seek(SeekFrom::End(0))
            .map_err(|error| format!("failed to seek tool-history journal append: {error}"))?;
        file.write_all(&bytes)
            .map_err(|error| format!("failed to append tool-history journal: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("failed to sync tool-history journal: {error}"))?;
        Ok(persisted_bytes)
    })
    .await
    .map_err(|error| format!("tool-history journal writer failed: {error}"))?
}

#[cfg(test)]
#[derive(Clone)]
struct ToolHistoryPersistencePauseState {
    thread_id: String,
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
pub(crate) struct ToolHistoryPersistencePause {
    state: ToolHistoryPersistencePauseState,
}

#[cfg(test)]
impl ToolHistoryPersistencePause {
    pub(crate) async fn wait_until_reached(&self) {
        self.state.reached.notified().await;
    }

    pub(crate) fn release(&self) {
        self.state.release.notify_one();
    }
}

#[cfg(test)]
impl Drop for ToolHistoryPersistencePause {
    fn drop(&mut self) {
        let slot = tool_history_persistence_pause_slot();
        let mut pending = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .as_ref()
            .is_some_and(|pending| Arc::ptr_eq(&pending.reached, &self.state.reached))
        {
            *pending = None;
        }
        self.state.release.notify_one();
    }
}

#[cfg(test)]
fn tool_history_persistence_pause_slot()
-> &'static std::sync::Mutex<Option<ToolHistoryPersistencePauseState>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<ToolHistoryPersistencePauseState>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
pub(crate) fn pause_next_tool_history_persistence_for_test(
    thread_id: &str,
) -> ToolHistoryPersistencePause {
    let state = ToolHistoryPersistencePauseState {
        thread_id: thread_id.to_string(),
        reached: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    let mut pending = tool_history_persistence_pause_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        pending.is_none(),
        "only one tool-history persistence pause may be installed at a time"
    );
    *pending = Some(state.clone());
    ToolHistoryPersistencePause { state }
}

#[cfg(test)]
async fn pause_tool_history_persistence_for_test_if_requested(thread_id: &str) {
    let pause = {
        let mut pending = tool_history_persistence_pause_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .as_ref()
            .is_some_and(|pending| pending.thread_id == thread_id)
        {
            pending.take()
        } else {
            None
        }
    };
    if let Some(pause) = pause {
        pause.reached.notify_one();
        pause.release.notified().await;
    }
}

#[cfg(test)]
#[derive(Clone)]
struct ToolHistoryPersistenceFailureState {
    thread_id: String,
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
pub(crate) struct ToolHistoryPersistenceFailure {
    state: ToolHistoryPersistenceFailureState,
}

#[cfg(test)]
impl ToolHistoryPersistenceFailure {
    pub(crate) async fn wait_until_reached(&self) {
        self.state.reached.notified().await;
    }

    pub(crate) fn release(&self) {
        self.state.release.notify_one();
    }
}

#[cfg(test)]
impl Drop for ToolHistoryPersistenceFailure {
    fn drop(&mut self) {
        let slot = tool_history_persistence_failure_slot();
        let mut pending = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .as_ref()
            .is_some_and(|pending| Arc::ptr_eq(&pending.reached, &self.state.reached))
        {
            *pending = None;
        }
        self.state.release.notify_one();
    }
}

#[cfg(test)]
fn tool_history_persistence_failure_slot()
-> &'static std::sync::Mutex<Option<ToolHistoryPersistenceFailureState>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<ToolHistoryPersistenceFailureState>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
pub(crate) fn fail_next_tool_history_persistence_for_test(
    thread_id: &str,
) -> ToolHistoryPersistenceFailure {
    let state = ToolHistoryPersistenceFailureState {
        thread_id: thread_id.to_string(),
        reached: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    let mut pending = tool_history_persistence_failure_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        pending.is_none(),
        "only one tool-history persistence failure may be installed at a time"
    );
    *pending = Some(state.clone());
    ToolHistoryPersistenceFailure { state }
}

#[cfg(test)]
async fn fail_tool_history_persistence_for_test_if_requested(
    thread_id: &str,
) -> Result<(), String> {
    let failure = {
        let mut pending = tool_history_persistence_failure_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .as_ref()
            .is_some_and(|pending| pending.thread_id == thread_id)
        {
            pending.take()
        } else {
            None
        }
    };
    if let Some(failure) = failure {
        failure.reached.notify_one();
        failure.release.notified().await;
        return Err("injected tool-history persistence failure".to_string());
    }
    Ok(())
}

fn ledger_path(codex_home: &std::path::Path, thread_id: &str) -> std::path::PathBuf {
    codex_home
        .join("tool-history")
        .join(format!("{thread_id}.json"))
}

fn journal_path(codex_home: &std::path::Path, thread_id: &str) -> std::path::PathBuf {
    codex_home
        .join("tool-history")
        .join(format!("{thread_id}.journal.jsonl"))
}

#[cfg(test)]
static TOOL_HISTORY_DIRECTORY_SYNC_ATTEMPTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn sync_tool_history_ledger_directory(directory: &std::path::Path) -> Result<(), String> {
    #[cfg(test)]
    TOOL_HISTORY_DIRECTORY_SYNC_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    sync_tool_history_ledger_directory_impl(directory)
}

fn sync_tool_history_ledger_directory_impl(_directory: &std::path::Path) -> Result<(), String> {
    // The file itself is synced above; Windows does not expose directory fsync through Rust.
    Ok(())
}

fn response_item_references_artifact(
    item: &ResponseItem,
    candidate: &ToolHistoryCandidate,
) -> bool {
    serde_json::to_value(item)
        .is_ok_and(|value| json_value_contains_artifact_reference(&value, candidate))
}

fn json_value_contains_artifact_reference(
    value: &serde_json::Value,
    candidate: &ToolHistoryCandidate,
) -> bool {
    match value {
        serde_json::Value::String(value) => serde_json::from_str::<serde_json::Value>(value)
            .is_ok_and(|value| json_value_contains_artifact_reference(&value, candidate)),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| json_value_contains_artifact_reference(value, candidate)),
        serde_json::Value::Object(values) => {
            json_object_matches_artifact_reference(value, values, candidate)
                || values
                    .values()
                    .any(|value| json_value_contains_artifact_reference(value, candidate))
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            false
        }
    }
}

fn json_object_matches_artifact_reference(
    value: &serde_json::Value,
    values: &serde_json::Map<String, serde_json::Value>,
    candidate: &ToolHistoryCandidate,
) -> bool {
    if values.contains_key("receipt_id")
        && (values.contains_key("artifact") || values.contains_key("artifact_id"))
        && ToolHistoryReceipt::deserialize(value)
            .is_ok_and(|receipt| candidate.matches_parsed_receipt(&receipt))
    {
        return true;
    }

    values
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| kind == "tool_history_artifact_pin")
        && ToolHistoryArtifactPinV1::deserialize(value).is_ok_and(|pin| {
            pin.version == 1
                && pin.artifact_id == candidate.artifact_id
                && pin.bytes == candidate.artifact_bytes
                && pin.sha256 == candidate.artifact_sha256
        })
}

fn canonical_textual_output_identity(item: &ResponseItem) -> Option<(&str, Cow<'_, str>)> {
    match item {
        ResponseItem::FunctionCallOutput {
            call_id, output, ..
        }
        | ResponseItem::CustomToolCallOutput {
            call_id, output, ..
        } => canonical_model_visible_output_text(&output.body).map(|text| (call_id.as_str(), text)),
        _ => None,
    }
}

fn textual_output_body_mut(item: &mut ResponseItem) -> Option<(&str, &mut FunctionCallOutputBody)> {
    match item {
        ResponseItem::FunctionCallOutput {
            call_id, output, ..
        }
        | ResponseItem::CustomToolCallOutput {
            call_id, output, ..
        } => Some((call_id.as_str(), &mut output.body)),
        _ => None,
    }
}

fn canonical_model_visible_output_text(body: &FunctionCallOutputBody) -> Option<Cow<'_, str>> {
    match body {
        FunctionCallOutputBody::Text(text) => Some(Cow::Borrowed(text)),
        FunctionCallOutputBody::ContentItems(items) => {
            let mut text_items = items.iter().filter_map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
                FunctionCallOutputContentItem::InputImage { .. }
                | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
            });
            let first = text_items.next()?;
            let Some(second) = text_items.next() else {
                return Some(Cow::Borrowed(first));
            };
            let mut joined = String::with_capacity(first.len() + second.len() + 1);
            joined.push_str(first);
            joined.push('\n');
            joined.push_str(second);
            for text in text_items {
                joined.push('\n');
                joined.push_str(text);
            }
            Some(Cow::Owned(joined))
        }
    }
}

fn replace_model_visible_output_text(body: &mut FunctionCallOutputBody, replacement: String) {
    match body {
        FunctionCallOutputBody::Text(text) => *text = replacement,
        FunctionCallOutputBody::ContentItems(items) => {
            let mut replacement = Some(replacement);
            items.retain_mut(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => {
                    let Some(replacement) = replacement.take() else {
                        return false;
                    };
                    *text = replacement;
                    true
                }
                FunctionCallOutputContentItem::InputImage { .. }
                | FunctionCallOutputContentItem::EncryptedContent { .. } => true,
            });
        }
    }
}

fn textual_output_identity(item: &ResponseItem) -> Option<(&str, &str)> {
    match item {
        ResponseItem::FunctionCallOutput {
            call_id, output, ..
        }
        | ResponseItem::CustomToolCallOutput {
            call_id, output, ..
        } => model_visible_output_text(&output.body).map(|text| (call_id.as_str(), text)),
        _ => None,
    }
}

fn model_visible_output_text(body: &FunctionCallOutputBody) -> Option<&str> {
    match body {
        FunctionCallOutputBody::Text(text) => Some(text),
        FunctionCallOutputBody::ContentItems(items) => {
            let mut text_items = items.iter().filter_map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
                FunctionCallOutputContentItem::InputImage { .. }
                | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
            });
            let text = text_items.next()?;
            text_items.next().is_none().then_some(text)
        }
    }
}

pub(crate) fn response_item_has_valid_tool_history_receipt(item: &ResponseItem) -> bool {
    let Some((call_id, text)) = textual_output_identity(item) else {
        return false;
    };
    let Ok(receipt) = serde_json::from_str::<ToolHistoryReceipt>(text) else {
        return false;
    };
    receipt.is_valid_for_call(call_id)
}

pub(crate) fn substitutions_overlap_items(
    substitutions: &[ToolHistorySubstitution],
    items: &[ResponseItem],
) -> bool {
    substitutions.iter().any(|substitution| {
        items
            .get(substitution.item_index)
            .and_then(textual_output_identity)
            .is_some_and(|(call_id, text)| {
                call_id == substitution.call_id
                    && sha256(text.as_bytes()) == substitution.bounded_output_sha256
            })
    })
}

pub(crate) fn substitutions_match_items(
    substitutions: &[ToolHistorySubstitution],
    items: &[ResponseItem],
) -> bool {
    substitutions.iter().all(|substitution| {
        items
            .get(substitution.item_index)
            .and_then(textual_output_identity)
            .is_some_and(|(call_id, text)| {
                let receipt_id_matches = serde_json::from_str::<ToolHistoryReceipt>(text)
                    .is_ok_and(|receipt| receipt.receipt_id() == substitution.receipt_id);
                call_id == substitution.call_id
                    && sha256(text.as_bytes()) == substitution.substituted_output_sha256
                    && receipt_id_matches
            })
    })
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn receipt_id_for(
    call_id: &str,
    artifact_sha256: &str,
    tool_identity: &str,
    semantic_class: &str,
    artifact_bytes: u64,
) -> String {
    format!(
        "thr1-{}",
        &format!(
            "{:x}",
            Sha256::digest(
                format!(
                    "{call_id}:{artifact_sha256}:{tool_identity}:{semantic_class}:{artifact_bytes}"
                )
                .as_bytes()
            )
        )[..16]
    )
}

fn admission_priority(candidate: &ToolHistoryCandidate, output: &str) -> u8 {
    if candidate.semantic_class.contains("validation") {
        0
    } else if matches!(
        candidate.semantic_class.as_str(),
        "tool_failure" | "tool_timeout"
    ) || output.contains("\"outcome\":\"failure\"")
        || output.contains("\"outcome\":\"timeout\"")
        || output.contains("\"outcome\":\"timed_out\"")
    {
        1
    } else {
        2
    }
}

fn tool_search_admission_priority(status: &str, tools: &[serde_json::Value]) -> u8 {
    if status != "completed" || tools.is_empty() {
        1
    } else {
        2
    }
}

pub(crate) fn tool_search_receipt_item(
    item: &ResponseItem,
    arguments: Option<&serde_json::Value>,
) -> Option<(ResponseItem, usize)> {
    let ResponseItem::ToolSearchOutput {
        call_id: Some(call_id),
        status,
        execution,
        tools,
        omitted_result_count,
        ..
    } = item
    else {
        return None;
    };
    let serialized_tools = serde_json::to_vec(tools).ok()?;
    let result_set_sha256 = sha256(&serialized_tools);
    let mut ordered_tool_identities = tools
        .iter()
        .filter_map(tool_search_result_identity)
        .collect::<Vec<_>>();
    let total_identity_count = ordered_tool_identities.len();
    let arguments = compact_tool_search_arguments(arguments);

    loop {
        let complete = status == "completed" && omitted_result_count.unwrap_or(0) == 0;
        let omitted_identity_count =
            total_identity_count.saturating_sub(ordered_tool_identities.len());
        let receipt = ToolSearchReceiptV1 {
            version: TOOL_SEARCH_RECEIPT_VERSION,
            receipt_id: tool_search_receipt_id(
                call_id,
                status,
                execution,
                &arguments,
                &result_set_sha256,
                tools.len(),
                *omitted_result_count,
                complete,
                omitted_identity_count,
            ),
            call_id: call_id.clone(),
            status: status.clone(),
            execution: execution.clone(),
            arguments: arguments.clone(),
            result_set_sha256: result_set_sha256.clone(),
            result_count: tools.len(),
            omitted_result_count: *omitted_result_count,
            complete,
            omitted_identity_count,
            ordered_tool_identities: ordered_tool_identities.clone(),
        };
        let receipt_value = serde_json::json!({
            "type": "tool_search_receipt",
            "receipt": receipt,
        });
        let mut receipt_item = item.clone();
        let ResponseItem::ToolSearchOutput { tools, .. } = &mut receipt_item else {
            return None;
        };
        *tools = vec![receipt_value];
        let serialized = serde_json::to_string(&receipt_item).ok()?;
        let tokens = approx_token_count(&serialized);
        if tokens <= RECEIPT_MAX_TOKENS {
            return Some((receipt_item, tokens));
        }
        if ordered_tool_identities.is_empty() {
            return None;
        }
        ordered_tool_identities.pop();
    }
}

#[allow(clippy::too_many_arguments)]
fn tool_search_receipt_id(
    call_id: &str,
    status: &str,
    execution: &str,
    arguments: &serde_json::Value,
    result_set_sha256: &str,
    result_count: usize,
    omitted_result_count: Option<usize>,
    complete: bool,
    omitted_identity_count: usize,
) -> String {
    let semantic_identity = serde_json::json!({
        "call_id": call_id,
        "status": status,
        "execution": execution,
        "arguments": arguments,
        "result_set_sha256": result_set_sha256,
        "result_count": result_count,
        "omitted_result_count": omitted_result_count,
        "complete": complete,
        "omitted_identity_count": omitted_identity_count,
    });
    format!(
        "tsr1-{}",
        &sha256(semantic_identity.to_string().as_bytes())[..16]
    )
}

fn compact_tool_search_arguments(arguments: Option<&serde_json::Value>) -> serde_json::Value {
    let Some(arguments) = arguments else {
        return serde_json::Value::Null;
    };
    let mut compact = serde_json::Map::new();
    for key in ["query", "namespace", "limit", "cursor"] {
        let Some(value) = arguments.get(key) else {
            continue;
        };
        let serialized = value.to_string();
        if approx_token_count(&serialized) > RECEIPT_DIGEST_TARGET_TOKENS {
            let bounded = value.as_str().map(|text| {
                serde_json::Value::String(truncate_text_to_token_ceiling(
                    text,
                    RECEIPT_DIGEST_TARGET_TOKENS,
                ))
            });
            compact.insert(
                key.to_string(),
                bounded.unwrap_or_else(|| {
                    serde_json::json!({
                        "value_sha256": sha256(serialized.as_bytes())
                    })
                }),
            );
            compact.insert(
                format!("{key}_sha256"),
                serde_json::Value::String(sha256(serialized.as_bytes())),
            );
        } else {
            compact.insert(key.to_string(), value.clone());
        }
    }
    if compact.is_empty() {
        serde_json::json!({"arguments_sha256": sha256(arguments.to_string().as_bytes())})
    } else {
        serde_json::Value::Object(compact)
    }
}

fn tool_search_result_identity(tool: &serde_json::Value) -> Option<String> {
    let name = tool
        .get("name")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            tool.pointer("/function/name")
                .and_then(serde_json::Value::as_str)
        })?;
    let namespace = tool
        .get("namespace")
        .and_then(serde_json::Value::as_str)
        .filter(|namespace| !namespace.is_empty());
    Some(match namespace {
        Some(namespace) => format!("{namespace}.{name}"),
        None => name.to_string(),
    })
}

#[cfg(test)]
fn tool_search_receipt(item: &ResponseItem) -> Option<ToolSearchReceiptV1> {
    let ResponseItem::ToolSearchOutput { tools, .. } = item else {
        return None;
    };
    let value = tools.first()?;
    (value.get("type")?.as_str()? == "tool_search_receipt")
        .then(|| serde_json::from_value(value.get("receipt")?.clone()).ok())
        .flatten()
}

fn item_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. }
        | ResponseItem::FunctionCallOutput { call_id, .. }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id),
        ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            ..
        }
        | ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => Some(call_id),
        _ => None,
    }
}

fn output_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCallOutput { call_id, .. }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id),
        ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => Some(call_id),
        _ => None,
    }
}

pub(crate) fn tool_observes_workspace(tool_identity: &str) -> bool {
    matches!(
        tool_identity,
        "exec_command" | "shell_command" | "unified_exec" | "write_stdin" | "cargo_test"
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceCallClassification {
    pub(crate) observes_workspace: bool,
    pub(crate) workspace_cwd: PathBuf,
    pub(crate) source_dependencies: BTreeSet<SourceDependencyV1>,
}

pub(crate) fn classify_workspace_tool_call(
    tool_identity: &str,
    payload: &ToolPayload,
    default_cwd: &Path,
) -> WorkspaceCallClassification {
    if !tool_observes_workspace(tool_identity) {
        return WorkspaceCallClassification {
            observes_workspace: false,
            workspace_cwd: default_cwd.to_path_buf(),
            source_dependencies: BTreeSet::new(),
        };
    }
    let arguments = workspace_call_arguments(payload);
    let observes_workspace = workspace_call_observes_from_arguments(arguments.as_ref());
    let workspace_cwd = arguments.as_ref().map_or_else(
        || default_cwd.to_path_buf(),
        |arguments| workspace_cwd_from_arguments(arguments, default_cwd),
    );
    let source_dependencies = arguments.as_ref().map_or_else(BTreeSet::new, |arguments| {
        source_dependencies_from_arguments(tool_identity, arguments, &workspace_cwd)
    });
    WorkspaceCallClassification {
        observes_workspace,
        workspace_cwd,
        source_dependencies,
    }
}

#[cfg(test)]
pub(crate) fn tool_call_observes_workspace(tool_identity: &str, payload: &ToolPayload) -> bool {
    if !tool_observes_workspace(tool_identity) {
        return false;
    }
    let ToolPayload::Function { arguments } = payload else {
        return true;
    };
    tool_call_observes_workspace_parts(tool_identity, arguments)
}

fn tool_call_observes_workspace_parts(tool_identity: &str, arguments: &str) -> bool {
    if !tool_observes_workspace(tool_identity) {
        return false;
    }
    let arguments = serde_json::from_str(arguments).ok();
    workspace_call_observes_from_arguments(arguments.as_ref())
}

fn workspace_call_arguments(payload: &ToolPayload) -> Option<serde_json::Value> {
    let ToolPayload::Function { arguments } = payload else {
        return None;
    };
    serde_json::from_str(arguments).ok()
}

fn workspace_call_observes_from_arguments(arguments: Option<&serde_json::Value>) -> bool {
    let Some(arguments) = arguments else {
        return true;
    };
    let Some(command) = dependency_command(arguments) else {
        return true;
    };
    !crate::turn_diff_tracker::command_may_mutate(&command)
        && !crate::turn_diff_tracker::command_reads_repository_history(&command)
}

pub(crate) fn source_dependencies_for_tool_call(
    tool_identity: &str,
    payload: &ToolPayload,
    default_cwd: &Path,
) -> BTreeSet<SourceDependencyV1> {
    source_dependencies_for_tool_call_with_parsed_arguments(
        tool_identity,
        payload,
        None,
        default_cwd,
    )
}

pub(crate) fn source_dependencies_for_tool_call_with_parsed_arguments(
    tool_identity: &str,
    payload: &ToolPayload,
    parsed_arguments: Option<&serde_json::Value>,
    default_cwd: &Path,
) -> BTreeSet<SourceDependencyV1> {
    if !tool_observes_workspace(tool_identity) {
        return BTreeSet::new();
    }
    let arguments = parsed_arguments
        .cloned()
        .or_else(|| workspace_call_arguments(payload));
    let Some(arguments) = arguments else {
        return BTreeSet::new();
    };
    let cwd = workspace_cwd_from_arguments(&arguments, default_cwd);
    source_dependencies_from_arguments(tool_identity, &arguments, &cwd)
}

fn source_dependencies_from_arguments(
    tool_identity: &str,
    arguments: &serde_json::Value,
    cwd: &Path,
) -> BTreeSet<SourceDependencyV1> {
    if tool_identity == "cargo_test" {
        return cargo_test_dependencies(arguments, cwd);
    }
    if let Some((command, shell_type)) = dependency_search_command(arguments) {
        match crate::tools::handlers::command_search::rg_search_path_operands(&command, shell_type)
        {
            Ok(Some(scopes)) => return dependencies_for_search_scopes(scopes, cwd),
            Err(_) => return BTreeSet::from([SourceDependencyV1::new(cwd, true)]),
            Ok(None) => {}
        }
    }
    let Some(command) = dependency_command(arguments) else {
        return BTreeSet::new();
    };
    dependencies_for_command(&command, cwd)
}

fn cargo_test_dependencies(
    arguments: &serde_json::Value,
    cwd: &Path,
) -> BTreeSet<SourceDependencyV1> {
    let package = arguments
        .get("package")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| cargo_test_package_from_args(arguments));
    let Some(package) = package else {
        return BTreeSet::from([SourceDependencyV1::new(cwd, true)]);
    };
    let workspace = cargo_workspace_root(cwd).unwrap_or_else(|| CargoWorkspaceRoot {
        path: cwd.to_path_buf(),
        manifest: std::fs::read_to_string(cwd.join("Cargo.toml"))
            .ok()
            .map(CargoManifestRecord::new),
    });
    let workspace_graph =
        cargo_workspace_graph_with_root_manifest(&workspace.path, workspace.manifest);
    let Some(package_root) = workspace_graph.packages.get(&package) else {
        return BTreeSet::from([SourceDependencyV1::new(cwd, true)]);
    };
    let mut dependencies = BTreeSet::from([
        SourceDependencyV1::new(&workspace.path.join("Cargo.toml"), false),
        SourceDependencyV1::new(&workspace.path.join("Cargo.lock"), false),
    ]);
    let mut visited = BTreeSet::new();
    collect_cargo_package_dependencies(
        package_root,
        &workspace_graph,
        &mut visited,
        &mut dependencies,
    );
    dependencies
}

fn cargo_test_package_from_args(arguments: &serde_json::Value) -> Option<String> {
    let args = arguments.get("cargo_args")?.as_array()?;
    let args = args
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    args.windows(2)
        .find(|pair| matches!(pair[0], "-p" | "--package"))
        .map(|pair| pair[1].to_string())
        .or_else(|| {
            args.iter()
                .find_map(|arg| arg.strip_prefix("--package=").map(str::to_string))
        })
}

struct CargoWorkspaceRoot {
    path: PathBuf,
    manifest: Option<CargoManifestRecord>,
}

fn cargo_workspace_root(cwd: &Path) -> Option<CargoWorkspaceRoot> {
    cwd.ancestors().find_map(|root| {
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
        let manifest = CargoManifestRecord::new(manifest);
        manifest
            .parsed
            .as_ref()?
            .get("workspace")
            .is_some()
            .then(|| CargoWorkspaceRoot {
                path: root.to_path_buf(),
                manifest: Some(manifest),
            })
    })
}

#[derive(Clone, Debug)]
struct CargoManifestRecord {
    source: String,
    parsed: Option<toml::Value>,
}

impl CargoManifestRecord {
    fn new(source: String) -> Self {
        let parsed = toml::from_str::<toml::Value>(&source).ok();
        Self { source, parsed }
    }
}

#[derive(Debug, Default)]
struct CargoWorkspaceGraph {
    packages: BTreeMap<String, PathBuf>,
    manifests: BTreeMap<PathBuf, CargoManifestRecord>,
}

#[cfg(test)]
fn cargo_package_index(workspace_root: &Path) -> CargoWorkspaceGraph {
    let root_manifest = std::fs::read_to_string(workspace_root.join("Cargo.toml")).ok();
    cargo_package_index_with_root_manifest(workspace_root, root_manifest)
}

#[cfg(test)]
fn cargo_package_index_with_root_manifest(
    workspace_root: &Path,
    root_manifest: Option<String>,
) -> CargoWorkspaceGraph {
    cargo_workspace_graph_with_root_manifest(
        workspace_root,
        root_manifest.map(CargoManifestRecord::new),
    )
}

fn cargo_workspace_graph_with_root_manifest(
    workspace_root: &Path,
    root_manifest: Option<CargoManifestRecord>,
) -> CargoWorkspaceGraph {
    cargo_workspace_graph_with_manifest_reader(workspace_root, root_manifest, |path| {
        std::fs::read_to_string(path).ok()
    })
}

#[cfg(test)]
fn cargo_package_index_with_manifest_reader(
    workspace_root: &Path,
    root_manifest: Option<String>,
    read_manifest: impl FnMut(&Path) -> Option<String>,
) -> CargoWorkspaceGraph {
    cargo_workspace_graph_with_manifest_reader(
        workspace_root,
        root_manifest.map(CargoManifestRecord::new),
        read_manifest,
    )
}

fn cargo_workspace_graph_with_manifest_reader(
    workspace_root: &Path,
    root_manifest: Option<CargoManifestRecord>,
    mut read_manifest: impl FnMut(&Path) -> Option<String>,
) -> CargoWorkspaceGraph {
    let workspace_root =
        dunce::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    let mut manifest_cache = BTreeMap::new();
    manifest_cache.insert(workspace_root.join("Cargo.toml"), root_manifest);
    cargo_workspace_graph_with_manifest_cache(
        &workspace_root,
        &mut manifest_cache,
        &mut read_manifest,
    )
}

fn cargo_workspace_graph_with_manifest_cache(
    workspace_root: &Path,
    manifest_cache: &mut BTreeMap<PathBuf, Option<CargoManifestRecord>>,
    read_manifest: &mut impl FnMut(&Path) -> Option<String>,
) -> CargoWorkspaceGraph {
    let workspace_root =
        dunce::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    let mut graph = CargoWorkspaceGraph::default();
    let mut pending = vec![workspace_root.to_path_buf()];
    let root_manifest = cached_cargo_manifest(
        &workspace_root.join("Cargo.toml"),
        manifest_cache,
        read_manifest,
    );
    if let Some(members) = root_manifest
        .as_ref()
        .and_then(|manifest| manifest.parsed.as_ref())
        .and_then(|parsed| parsed.get("workspace")?.get("members")?.as_array())
    {
        pending.extend(
            members
                .iter()
                .filter_map(toml::Value::as_str)
                // Cargo workspace member globs are still discovered by the
                // recursive fallback below; explicit members are seeded here
                // so package discovery is reliable on Windows temp roots.
                .filter(|member| !member.contains(['*', '?', '[']))
                .map(|member| workspace_root.join(member)),
        );
    }
    let mut visited = BTreeSet::new();
    while let Some(directory) = pending.pop() {
        let directory = dunce::canonicalize(&directory).unwrap_or_else(|_| directory.to_path_buf());
        if !visited.insert(directory.clone()) {
            continue;
        }
        let manifest_path = directory.join("Cargo.toml");
        let manifest = cached_cargo_manifest(&manifest_path, manifest_cache, read_manifest);
        if let Some(manifest) = manifest {
            pending.extend(cargo_manifest_path_dependencies(&manifest, &directory));
            if let Some(name) = cargo_manifest_package_name(&manifest) {
                graph.packages.insert(name, directory.clone());
            }
            graph.manifests.insert(directory.clone(), manifest);
        }

        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name();
                if !matches!(
                    name.to_str(),
                    Some("target" | ".git" | "vendor" | "third_party" | "node_modules")
                ) {
                    pending.push(path);
                }
            }
        }
    }
    graph
}

fn cached_cargo_manifest(
    path: &Path,
    cache: &mut BTreeMap<PathBuf, Option<CargoManifestRecord>>,
    read_manifest: &mut impl FnMut(&Path) -> Option<String>,
) -> Option<CargoManifestRecord> {
    if let Some(manifest) = cache.get(path) {
        return manifest.clone();
    }
    let manifest = read_manifest(path).map(CargoManifestRecord::new);
    cache.insert(path.to_path_buf(), manifest.clone());
    manifest
}

fn cargo_manifest_path_dependencies(
    manifest: &CargoManifestRecord,
    package_root: &Path,
) -> BTreeSet<PathBuf> {
    let Some(parsed) = manifest.parsed.as_ref() else {
        return cargo_manifest_dependency_fallback(
            &manifest.source,
            package_root,
            &BTreeMap::new(),
        );
    };
    let tables = ["dependencies", "dev-dependencies", "build-dependencies"]
        .into_iter()
        .filter_map(|key| parsed.get(key).and_then(toml::Value::as_table));
    let target_tables = parsed
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|targets| targets.values())
        .filter_map(toml::Value::as_table)
        .flat_map(|target| {
            ["dependencies", "dev-dependencies", "build-dependencies"]
                .into_iter()
                .filter_map(move |key| target.get(key).and_then(toml::Value::as_table))
        });
    tables
        .chain(target_tables)
        .flat_map(|table| table.values())
        .filter_map(toml::Value::as_table)
        .filter_map(|specification| specification.get("path"))
        .filter_map(toml::Value::as_str)
        .map(|path| package_root.join(path))
        .collect()
}

fn cargo_manifest_package_name(manifest: &CargoManifestRecord) -> Option<String> {
    if let Some(name) = manifest.parsed.as_ref().and_then(|parsed| {
        parsed
            .get("package")?
            .get("name")?
            .as_str()
            .map(str::to_string)
    }) {
        return Some(name);
    }

    // Keep dependency tracking fail-safe when a future Cargo syntax is newer
    // than the bundled TOML parser. Package names are simple quoted scalars,
    // so this narrow fallback can still identify local workspace members.
    let mut in_package = false;
    for raw_line in manifest.source.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "name" {
            continue;
        }
        let value = value.trim();
        return value
            .strip_prefix('"')
            .and_then(|value| value.split_once('"').map(|(name, _)| name.to_string()))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.split_once('\'').map(|(name, _)| name.to_string()))
            });
    }
    None
}

fn collect_cargo_package_dependencies(
    package_root: &Path,
    workspace_graph: &CargoWorkspaceGraph,
    visited: &mut BTreeSet<PathBuf>,
    dependencies: &mut BTreeSet<SourceDependencyV1>,
) {
    let package_root =
        dunce::canonicalize(package_root).unwrap_or_else(|_| package_root.to_path_buf());
    if !visited.insert(package_root.clone()) {
        return;
    }
    dependencies.insert(SourceDependencyV1::new(&package_root, true));
    let manifest = workspace_graph.manifests.get(&package_root);
    let Some(manifest) = manifest else {
        return;
    };
    let Some(parsed) = manifest.parsed.as_ref() else {
        for local_root in cargo_manifest_dependency_fallback(
            &manifest.source,
            &package_root,
            &workspace_graph.packages,
        ) {
            collect_cargo_package_dependencies(&local_root, workspace_graph, visited, dependencies);
        }
        return;
    };
    let mut dependency_tables = ["dependencies", "dev-dependencies", "build-dependencies"]
        .into_iter()
        .filter_map(|key| parsed.get(key).and_then(toml::Value::as_table))
        .collect::<Vec<_>>();
    if let Some(targets) = parsed.get("target").and_then(toml::Value::as_table) {
        for target in targets.values().filter_map(toml::Value::as_table) {
            dependency_tables.extend(
                ["dependencies", "dev-dependencies", "build-dependencies"]
                    .into_iter()
                    .filter_map(|key| target.get(key).and_then(toml::Value::as_table)),
            );
        }
    }
    for table in dependency_tables {
        for (dependency_name, specification) in table {
            let local_root = specification
                .as_table()
                .and_then(|specification| {
                    specification
                        .get("path")
                        .and_then(toml::Value::as_str)
                        .map(|path| package_root.join(path))
                        .or_else(|| {
                            let package_name = specification
                                .get("package")
                                .and_then(toml::Value::as_str)
                                .unwrap_or(dependency_name);
                            workspace_graph.packages.get(package_name).cloned()
                        })
                })
                .or_else(|| workspace_graph.packages.get(dependency_name).cloned());
            if let Some(local_root) = local_root {
                collect_cargo_package_dependencies(
                    &local_root,
                    workspace_graph,
                    visited,
                    dependencies,
                );
            }
        }
    }
}

fn cargo_manifest_dependency_fallback(
    manifest: &str,
    package_root: &Path,
    package_index: &BTreeMap<String, PathBuf>,
) -> BTreeSet<PathBuf> {
    let mut in_dependency_table = false;
    let mut roots = BTreeSet::new();
    for raw_line in manifest.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            let section = line.trim_matches(['[', ']']).trim();
            in_dependency_table = matches!(
                section,
                "dependencies" | "dev-dependencies" | "build-dependencies"
            ) || section.ends_with(".dependencies")
                || section.ends_with(".dev-dependencies")
                || section.ends_with(".build-dependencies");
            continue;
        }
        if !in_dependency_table || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((dependency_name, specification)) = line.split_once('=') else {
            continue;
        };
        let dependency_name = dependency_name.trim().trim_matches(['\'', '"']);
        let local_root = inline_dependency_string(specification, "path")
            .map(|path| package_root.join(path))
            .or_else(|| {
                let package_name =
                    inline_dependency_string(specification, "package").unwrap_or(dependency_name);
                package_index.get(package_name).cloned()
            });
        if let Some(local_root) = local_root {
            roots.insert(local_root);
        }
    }
    roots
}

fn inline_dependency_string<'a>(specification: &'a str, key: &str) -> Option<&'a str> {
    specification
        .trim()
        .trim_matches(['{', '}'])
        .split(',')
        .filter_map(|field| field.split_once('='))
        .find_map(|(field_key, value)| {
            (field_key.trim() == key).then(|| value.trim().trim_matches(['\'', '"']))
        })
}

#[cfg(test)]
pub(crate) fn workspace_evidence_cwd_for_tool_call(
    tool_identity: &str,
    payload: &ToolPayload,
    default_cwd: &Path,
) -> PathBuf {
    if !tool_observes_workspace(tool_identity) {
        return default_cwd.to_path_buf();
    }
    let Some(arguments) = workspace_call_arguments(payload) else {
        return default_cwd.to_path_buf();
    };
    workspace_cwd_from_arguments(&arguments, default_cwd)
}

fn workspace_cwd_from_arguments(arguments: &serde_json::Value, default_cwd: &Path) -> PathBuf {
    arguments
        .get("workdir")
        .or_else(|| arguments.get("cwd"))
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                default_cwd.join(path)
            }
        })
        .unwrap_or_else(|| default_cwd.to_path_buf())
}

fn dependency_command(arguments: &serde_json::Value) -> Option<Vec<String>> {
    if let Some(program) = arguments.get("program").and_then(serde_json::Value::as_str) {
        let mut command = vec![program.to_string()];
        command.extend(
            arguments
                .get("args")
                .and_then(serde_json::Value::as_array)?
                .iter()
                .map(|value| value.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?,
        );
        return Some(command);
    }
    for key in ["command", "cmd"] {
        match arguments.get(key) {
            Some(serde_json::Value::Array(values)) => {
                return values
                    .iter()
                    .map(|value| value.as_str().map(str::to_string))
                    .collect();
            }
            Some(serde_json::Value::String(command))
                if !command
                    .chars()
                    .any(|ch| matches!(ch, '|' | ';' | '&' | '>' | '<' | '"' | '\'' | '`')) =>
            {
                return Some(command.split_whitespace().map(str::to_string).collect());
            }
            _ => {}
        }
    }
    None
}

fn dependency_search_command(
    arguments: &serde_json::Value,
) -> Option<(Vec<String>, Option<crate::shell::ShellType>)> {
    if let Some(command) = dependency_command(arguments) {
        return Some((command, None));
    }
    let script = arguments
        .get("command")
        .or_else(|| arguments.get("cmd"))?
        .as_str()?;
    let shell_type = arguments
        .get("shell")
        .and_then(serde_json::Value::as_str)
        .and_then(shell_type_from_name)
        .unwrap_or_else(|| crate::shell::default_user_shell().shell_type);
    let command = match shell_type {
        crate::shell::ShellType::PowerShell => vec![
            "powershell".to_string(),
            "-Command".to_string(),
            script.to_string(),
        ],
        crate::shell::ShellType::Cmd => {
            vec!["cmd".to_string(), "/c".to_string(), script.to_string()]
        }
        crate::shell::ShellType::Bash => {
            vec!["bash".to_string(), "-c".to_string(), script.to_string()]
        }
        crate::shell::ShellType::Zsh => {
            vec!["zsh".to_string(), "-c".to_string(), script.to_string()]
        }
        crate::shell::ShellType::Sh => {
            vec!["sh".to_string(), "-c".to_string(), script.to_string()]
        }
    };
    Some((command, Some(shell_type)))
}

fn shell_type_from_name(value: &str) -> Option<crate::shell::ShellType> {
    let name = command_basename(value);
    match name.as_str() {
        "pwsh" | "powershell" => Some(crate::shell::ShellType::PowerShell),
        "cmd" | "cmd.exe" => Some(crate::shell::ShellType::Cmd),
        "bash" => Some(crate::shell::ShellType::Bash),
        "zsh" => Some(crate::shell::ShellType::Zsh),
        "sh" => Some(crate::shell::ShellType::Sh),
        _ => None,
    }
}

fn dependencies_for_command(command: &[String], cwd: &Path) -> BTreeSet<SourceDependencyV1> {
    let Some(program) = command.first().map(|value| command_basename(value)) else {
        return BTreeSet::new();
    };
    let lower = command
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if lower
        .iter()
        .any(|value| value.ends_with("source_owners.py"))
    {
        return [
            "source_owners.toml",
            "architecture_index.json",
            "SOURCEMAP.md",
            "scripts/source_owners.py",
        ]
        .into_iter()
        .map(|path| SourceDependencyV1::new(&cwd.join(path), false))
        .collect();
    }
    let is_test = (program == "cargo"
        && lower
            .get(1)
            .is_some_and(|arg| arg == "test" || arg == "nextest"))
        || matches!(program.as_str(), "pytest" | "nextest")
        || (matches!(program.as_str(), "python" | "python3" | "py")
            && lower
                .windows(2)
                .any(|args| args == ["-m", "pytest"] || args == ["-m", "unittest"]))
        || (matches!(program.as_str(), "npm" | "pnpm" | "yarn")
            && lower.iter().skip(1).any(|arg| arg == "test"));
    if is_test {
        return BTreeSet::from([SourceDependencyV1::new(cwd, true)]);
    }
    if matches!(
        program.as_str(),
        "cat" | "type" | "get-content" | "gc" | "bat" | "head" | "tail"
    ) {
        return dependencies_for_read_command(&program, command, cwd);
    }
    if !matches!(
        program.as_str(),
        "rg" | "ripgrep" | "grep" | "ag" | "fd" | "find"
    ) {
        return BTreeSet::new();
    }

    let files_mode = lower.iter().any(|arg| arg == "--files");
    let mut skipped_pattern = files_mode || matches!(program.as_str(), "fd" | "find");
    let mut option_value = false;
    let mut scopes = Vec::new();
    for arg in command.iter().skip(1) {
        if option_value {
            option_value = false;
            continue;
        }
        if matches!(
            arg.as_str(),
            "-g" | "--glob" | "--iglob" | "-t" | "--type" | "-e" | "--regexp" | "-f" | "--file"
        ) {
            option_value = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        if !skipped_pattern {
            skipped_pattern = true;
            continue;
        }
        scopes.push(arg);
    }
    if scopes.is_empty() {
        return BTreeSet::from([SourceDependencyV1::new(cwd, true)]);
    }
    if scopes.len() > 8 {
        return BTreeSet::from([SourceDependencyV1::new(cwd, true)]);
    }
    scopes
        .into_iter()
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .map(|path| {
            let recursive = path.is_dir() || path.extension().is_none();
            SourceDependencyV1::new(&path, recursive)
        })
        .collect()
}

fn dependencies_for_search_scopes(scopes: Vec<String>, cwd: &Path) -> BTreeSet<SourceDependencyV1> {
    if scopes.is_empty() || scopes.len() > 8 {
        return BTreeSet::from([SourceDependencyV1::new(cwd, true)]);
    }
    scopes
        .into_iter()
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .map(|path| {
            let recursive = path.is_dir() || path.extension().is_none();
            SourceDependencyV1::new(&path, recursive)
        })
        .collect()
}

fn dependencies_for_read_command(
    program: &str,
    command: &[String],
    cwd: &Path,
) -> BTreeSet<SourceDependencyV1> {
    let mut paths = Vec::new();
    let mut option_value = false;
    for arg in command.iter().skip(1) {
        let lower = arg.to_ascii_lowercase();
        if option_value {
            option_value = false;
            continue;
        }
        if matches!(program, "get-content" | "gc") {
            if matches!(lower.as_str(), "-path" | "-literalpath") {
                option_value = false;
                continue;
            }
            if matches!(
                lower.as_str(),
                "-totalcount"
                    | "-tail"
                    | "-readcount"
                    | "-encoding"
                    | "-filter"
                    | "-include"
                    | "-exclude"
                    | "-stream"
            ) {
                option_value = true;
                continue;
            }
            if matches!(lower.as_str(), "-raw" | "-force" | "-wait") {
                continue;
            }
        } else if matches!(program, "head" | "tail")
            && matches!(lower.as_str(), "-n" | "--lines" | "-c" | "--bytes")
        {
            option_value = true;
            continue;
        }
        if arg.starts_with('-') {
            return BTreeSet::new();
        }
        paths.push(arg.as_str());
    }
    if paths.is_empty() || paths.len() > 8 {
        return BTreeSet::new();
    }
    paths
        .into_iter()
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .map(|path| SourceDependencyV1::new(&path, false))
        .collect()
}

fn command_basename(value: &str) -> String {
    Path::new(value)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(value)
        .to_ascii_lowercase()
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
#[path = "tool_history_tests.rs"]
mod tests;
