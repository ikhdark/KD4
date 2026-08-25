use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Write;
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

const RECEIPT_VERSION: u8 = 1;
const RECEIPT_MAX_TOKENS: usize = 256;
const RECEIPT_DIGEST_TARGET_TOKENS: usize = 96;
const MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET: usize = 10_000;
const MINIMUM_RAW_TOKENS: u64 = 256;
const MINIMUM_SAVED_TOKENS: u64 = 64;
const MINIMUM_RELATIVE_SAVINGS_PERCENT: u64 = 25;
const LEDGER_VERSION: u8 = 1;

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
}

impl ToolHistoryCandidate {
    pub(crate) fn artifact_reference(&self) -> (u64, String) {
        (self.artifact_bytes, self.artifact_sha256.clone())
    }

    fn receipt(&self) -> Option<(String, String)> {
        self.render_receipt(
            /*require_consumed*/ true, /*require_savings*/ true,
        )
    }

    fn admission_receipt(&self) -> Option<(String, String)> {
        self.render_receipt(
            /*require_consumed*/ false, /*require_savings*/ false,
        )
    }

    fn render_receipt(
        &self,
        require_consumed: bool,
        require_savings: bool,
    ) -> Option<(String, String)> {
        if !self.complete
            || !self.projection_eligible
            || (require_consumed && self.consumed_by_generation.is_none())
        {
            return None;
        }
        let bounded_tokens =
            u64::try_from(approx_token_count(&self.bounded_model_output)).unwrap_or(u64::MAX);
        if require_savings && bounded_tokens < MINIMUM_RAW_TOKENS {
            return None;
        }
        let receipt_id = receipt_id_for(&self.call_id, &self.artifact_sha256);
        let mut digest_limit = RECEIPT_DIGEST_TARGET_TOKENS;
        loop {
            let receipt = ToolHistoryReceiptV1 {
                version: RECEIPT_VERSION,
                receipt_id: receipt_id.clone(),
                call_id: self.call_id.clone(),
                tool_identity: self.tool_identity.clone(),
                semantic_class: self.semantic_class.clone(),
                source_dependencies_current: self.source_dependencies_current,
                digest: if self.source_dependencies_current {
                    truncate_text_to_token_ceiling(&self.bounded_model_output, digest_limit)
                } else {
                    "STALE: a source dependency changed after this result was produced; rerun the tool before relying on it."
                        .to_string()
                },
                artifact: ReceiptArtifact {
                    artifact_id: self.artifact_id.clone(),
                    byte_start: 0,
                    byte_end: self.artifact_bytes,
                    sha256: self.artifact_sha256.clone(),
                    complete: self.complete,
                },
                original: ReceiptOriginalSize {
                    bytes: self.artifact_bytes,
                    approximate_tokens: self.original_tokens,
                },
                retrieval: ReceiptRetrieval {
                    tool: "read_tool_output".to_string(),
                    instruction: "Use artifact_id with a narrow byte/line range; verify canonical_sha256 for exact recovery.".to_string(),
                },
            };
            let rendered = serde_json::to_string(&receipt).ok()?;
            let receipt_tokens = u64::try_from(approx_token_count(&rendered)).unwrap_or(u64::MAX);
            if receipt_tokens <= RECEIPT_MAX_TOKENS as u64 {
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
                return Some((receipt_id, rendered));
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

    fn matches_receipt(&self, text: &str) -> bool {
        serde_json::from_str::<ToolHistoryReceiptV1>(text).is_ok_and(|receipt| {
            receipt.version == RECEIPT_VERSION
                && receipt.call_id == self.call_id
                && receipt.receipt_id == receipt_id_for(&self.call_id, &self.artifact_sha256)
                && receipt.source_dependencies_current == self.source_dependencies_current
                && receipt.artifact.artifact_id == self.artifact_id
                && receipt.artifact.sha256 == self.artifact_sha256
                && receipt.artifact.byte_start == 0
                && receipt.artifact.byte_end == self.artifact_bytes
                && receipt.artifact.complete == self.complete
        })
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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ToolHistoryState {
    #[serde(default)]
    candidates: BTreeMap<String, ToolHistoryCandidate>,
    #[serde(default)]
    workspace_evidence: BTreeMap<String, WorkspaceEvidenceObservation>,
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
        let (call_id, output) = textual_output_identity(item)?;
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

impl ToolHistoryState {
    pub(crate) fn register(&mut self, candidate: ToolHistoryCandidate) {
        self.candidates.insert(candidate.call_id.clone(), candidate);
    }

    pub(crate) fn invalidate_source_dependencies(
        &mut self,
        affected_paths: Option<&BTreeSet<PathBuf>>,
        current_workspace_identity: Option<&WorkspaceEvidenceIdentity>,
    ) -> bool {
        let normalized_affected = affected_paths.map(|paths| {
            paths
                .iter()
                .map(|path| normalized_source_path(path))
                .collect::<BTreeSet<_>>()
        });
        let mut changed = false;
        for candidate in self.candidates.values_mut() {
            if !candidate.source_dependencies_current {
                continue;
            }
            let affected = if candidate.source_dependencies.is_empty() {
                tool_observes_workspace(&candidate.tool_identity)
            } else {
                normalized_affected.as_ref().is_none_or(|paths| {
                    candidate.source_dependencies.iter().any(|dependency| {
                        paths
                            .iter()
                            .any(|path| source_dependency_overlaps(dependency, path))
                    })
                })
            };
            if affected {
                candidate.source_dependencies_current = false;
                changed = true;
            }
        }
        for observation in self.workspace_evidence.values_mut() {
            if !observation.source_dependencies_current {
                continue;
            }
            let affected = observation.source_dependencies.is_empty()
                || normalized_affected.as_ref().is_none_or(|paths| {
                    observation.source_dependencies.iter().any(|dependency| {
                        paths
                            .iter()
                            .any(|path| source_dependency_overlaps(dependency, path))
                    })
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

    pub(crate) fn mark_consumed(
        &mut self,
        input: &[ResponseItem],
        generation: ModelGenerationId,
    ) -> bool {
        let exposed = input
            .iter()
            .filter_map(textual_output_identity)
            .map(|(call_id, text)| (call_id.to_string(), text.to_string()))
            .collect::<BTreeMap<_, _>>();
        let mut changed = false;
        for candidate in self.candidates.values_mut() {
            if candidate.consumed_by_generation.is_some() {
                continue;
            }
            let bounded_output_sha256 = sha256(candidate.bounded_model_output.as_bytes());
            if exposed.get(&candidate.call_id).is_some_and(|text| {
                sha256(text.as_bytes()) == bounded_output_sha256 || candidate.matches_receipt(text)
            }) {
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
        let mut projected = items.to_vec();
        self.invalidate_stale_workspace_evidence(
            &mut projected,
            workspace_identity,
            Some(git_workspace),
        );
        let projected: Arc<[ResponseItem]> = Arc::from(projected);
        ToolHistoryProjection {
            items: Arc::clone(&projected),
            unreplaced_items: projected,
            substitutions: Arc::from([]),
        }
    }

    fn project_inner(
        &self,
        items: Arc<[ResponseItem]>,
        workspace_identity: Option<Option<&WorkspaceEvidenceIdentity>>,
        git_workspace: Option<&GitWorkspaceCache>,
    ) -> ToolHistoryProjection {
        let mut projected = items.to_vec();
        if let Some(workspace_identity) = workspace_identity {
            self.invalidate_stale_workspace_evidence(
                &mut projected,
                workspace_identity,
                git_workspace,
            );
        }
        let mut latest_supersession = BTreeMap::<String, String>::new();
        let mut superseded_call_ids = BTreeSet::new();
        for item in &projected {
            let Some((call_id, output)) = textual_output_identity(item) else {
                continue;
            };
            let Some(candidate) = self.candidates.get(call_id) else {
                continue;
            };
            if sha256(output.as_bytes()) != sha256(candidate.bounded_model_output.as_bytes()) {
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
                let (call_id, output) = textual_output_identity(item)?;
                let candidate = self.candidates.get(call_id)?;
                (sha256(output.as_bytes()) == sha256(candidate.bounded_model_output.as_bytes()))
                    .then(|| AdmissionCandidate {
                        priority: admission_priority(candidate, output),
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
                    ..
                } = item
                else {
                    return None;
                };
                let serialized = serde_json::to_string(item).ok()?;
                Some(AdmissionCandidate {
                    priority: 2,
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
            Drop,
        }
        #[derive(Debug)]
        struct AdmissionDecision {
            representation: AdmissionRepresentation,
            retain_raw_fallback: bool,
        }

        let mut decisions = BTreeMap::<String, AdmissionDecision>::new();
        let mut remaining_tokens = MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET;
        for admission_candidate in admission_candidates {
            let item_index = admission_candidate.item_index.0;
            let call_id = admission_candidate.call_id;
            if let Some(raw_tokens) = admission_candidate.structured_tokens {
                let representation = if raw_tokens <= remaining_tokens {
                    remaining_tokens = remaining_tokens.saturating_sub(raw_tokens);
                    AdmissionRepresentation::Raw
                } else {
                    AdmissionRepresentation::Drop
                };
                decisions.insert(
                    call_id,
                    AdmissionDecision {
                        representation,
                        retain_raw_fallback: true,
                    },
                );
                continue;
            }
            let Some((_, output)) = projected.get(item_index).and_then(textual_output_identity)
            else {
                continue;
            };
            let Some(candidate) = self.candidates.get(&call_id) else {
                continue;
            };
            let non_text_tokens =
                usize::try_from(candidate.preserved_non_text_tokens).unwrap_or(usize::MAX);
            let raw_tokens = approx_token_count(output).saturating_add(non_text_tokens);
            let receipt = if candidate.consumed_by_generation.is_some() {
                candidate
                    .receipt()
                    .or_else(|| candidate.admission_receipt())
            } else {
                candidate.admission_receipt()
            };
            let receipt = receipt.map(|(receipt_id, text)| {
                let tokens = approx_token_count(&text).saturating_add(non_text_tokens);
                (receipt_id, text, tokens)
            });

            let decision = if raw_tokens <= remaining_tokens {
                remaining_tokens = remaining_tokens.saturating_sub(raw_tokens);
                if candidate.consumed_by_generation.is_some()
                    && let Some((receipt_id, text, _)) = receipt
                {
                    AdmissionDecision {
                        representation: AdmissionRepresentation::Receipt { receipt_id, text },
                        retain_raw_fallback: true,
                    }
                } else {
                    AdmissionDecision {
                        representation: AdmissionRepresentation::Raw,
                        retain_raw_fallback: true,
                    }
                }
            } else if let Some((receipt_id, text, receipt_tokens)) = receipt
                && receipt_tokens <= remaining_tokens
            {
                remaining_tokens = remaining_tokens.saturating_sub(receipt_tokens);
                AdmissionDecision {
                    representation: AdmissionRepresentation::Receipt { receipt_id, text },
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
                    !matches!(&decision.representation, AdmissionRepresentation::Drop)
                        && decision.retain_raw_fallback
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

        let mut substitutions = Vec::new();
        for (item_index, item) in projected.iter_mut().enumerate() {
            let Some((call_id, output)) = textual_output_mut(item) else {
                continue;
            };
            let Some(candidate) = self.candidates.get(call_id) else {
                continue;
            };
            let bounded_output_sha256 = sha256(candidate.bounded_model_output.as_bytes());
            if sha256(output.as_bytes()) != bounded_output_sha256 {
                continue;
            }
            let Some(AdmissionDecision {
                representation: AdmissionRepresentation::Receipt { receipt_id, text },
                ..
            }) = decisions.get(call_id)
            else {
                continue;
            };
            let substituted_output_sha256 = sha256(text.as_bytes());
            *output = text.clone();
            substitutions.push(ToolHistorySubstitution {
                item_index,
                call_id: call_id.to_string(),
                bounded_output_sha256,
                receipt_id: receipt_id.clone(),
                substituted_output_sha256,
            });
        }
        ToolHistoryProjection {
            items: Arc::from(projected),
            unreplaced_items: Arc::from(unreplaced_projected),
            substitutions: Arc::from(substitutions),
        }
    }

    fn invalidate_stale_workspace_evidence(
        &self,
        items: &mut [ResponseItem],
        workspace_identity: Option<&WorkspaceEvidenceIdentity>,
        git_workspace: Option<&GitWorkspaceCache>,
    ) {
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
            let call_observes_workspace = name == "functions.exec"
                || tool_call_observes_workspace(
                    name,
                    &ToolPayload::Function {
                        arguments: arguments.clone(),
                    },
                );
            if call_observes_workspace || self.workspace_evidence.contains_key(call_id) {
                requirements.insert(call_id.clone(), call_id.clone());
                continue;
            }
            if name != "read_tool_output" {
                continue;
            }
            let Some(artifact_id) = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|value| value.get("artifact_id")?.as_str().map(str::to_string))
            else {
                continue;
            };
            let origin = self
                .candidates
                .values()
                .find(|candidate| candidate.artifact_id == artifact_id);
            match origin {
                Some(candidate)
                    if candidate.tool_identity == "functions.exec"
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

        for item in items.iter_mut() {
            let Some((call_id, output)) = textual_output_mut(item) else {
                continue;
            };
            let Some(origin_call_id) = requirements.get(call_id) else {
                continue;
            };
            let observation = self.workspace_evidence.get(origin_call_id);
            let revision_matches = observation.is_some_and(|observation| {
                observation.source_dependencies_current
                    && ((observation.revision.as_ref() == workspace_identity)
                        || observation.source_paths_are_current(workspace_identity, git_workspace))
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
            *output = serde_json::json!({
                "call_id": call_id,
                "reason": reason,
                "stale_workspace_evidence": true,
            })
            .to_string();
        }
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
            let Some(artifact_id) = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|value| value.get("artifact_id")?.as_str().map(str::to_string))
            else {
                continue;
            };
            if let Some(origin_call_id) = self
                .candidates
                .values()
                .find(|candidate| candidate.artifact_id == artifact_id)
                .map(|candidate| candidate.call_id.clone())
            {
                live.insert(origin_call_id);
            }
        }
        self.candidates.retain(|call_id, _| live.contains(call_id));
        self.workspace_evidence
            .retain(|call_id, _| live.contains(call_id));
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
    }
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

    let normalized = normalized.to_ascii_lowercase();
    let trimmed = normalized.trim_end_matches('/');
    if trimmed.is_empty() {
        normalized
    } else {
        trimmed.to_string()
    }
}

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

#[derive(Deserialize, Serialize)]
struct ToolHistoryLedgerFile {
    version: u8,
    state: ToolHistoryState,
}

pub(crate) async fn load_tool_history_state(
    codex_home: &std::path::Path,
    thread_id: &str,
) -> ToolHistoryState {
    let state = load_tool_history_state_for_fork(codex_home, thread_id).await;
    reconcile_tool_history_state(codex_home, thread_id, state).await
}

/// Reads a parent ledger for fork without reconciling the parent's protection markers.
///
/// The parent can still be live while the child is initialized. Mutating its artifact ownership
/// from the child would race with a parent tool result between marker creation and ledger persist.
pub(crate) async fn load_tool_history_state_for_fork(
    codex_home: &std::path::Path,
    thread_id: &str,
) -> ToolHistoryState {
    let path = ledger_path(codex_home, thread_id);
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice::<ToolHistoryLedgerFile>(&bytes)
            .ok()
            .filter(|file| file.version == LEDGER_VERSION)
            .map(|file| file.state)
            .unwrap_or_default(),
        Err(_) => ToolHistoryState::default(),
    }
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
        reminted_candidates.insert(call_id, candidate);
    }
    (
        ToolHistoryState {
            candidates: reminted_candidates,
            workspace_evidence,
        },
        dropped_candidates,
    )
}

pub(crate) async fn persist_tool_history_state(
    codex_home: &std::path::Path,
    thread_id: &str,
    state: &ToolHistoryState,
) -> Result<(), String> {
    let path = ledger_path(codex_home, thread_id);
    let bytes = serde_json::to_vec(&ToolHistoryLedgerFile {
        version: LEDGER_VERSION,
        state: state.clone(),
    })
    .map_err(|err| format!("failed to serialize tool-history ledger: {err}"))?;
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

        Ok(())
    })
    .await
    .map_err(|err| format!("tool-history ledger writer failed: {err}"))?
}

fn ledger_path(codex_home: &std::path::Path, thread_id: &str) -> std::path::PathBuf {
    codex_home
        .join("tool-history")
        .join(format!("{thread_id}.json"))
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

fn textual_output_mut(item: &mut ResponseItem) -> Option<(&str, &mut String)> {
    match item {
        ResponseItem::FunctionCallOutput {
            call_id, output, ..
        }
        | ResponseItem::CustomToolCallOutput {
            call_id, output, ..
        } => model_visible_output_text_mut(&mut output.body).map(|text| (call_id.as_str(), text)),
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

fn model_visible_output_text_mut(body: &mut FunctionCallOutputBody) -> Option<&mut String> {
    match body {
        FunctionCallOutputBody::Text(text) => Some(text),
        FunctionCallOutputBody::ContentItems(items) => {
            let mut text_indexes = items.iter().enumerate().filter_map(|(index, item)| {
                matches!(item, FunctionCallOutputContentItem::InputText { .. }).then_some(index)
            });
            let text_index = text_indexes.next()?;
            if text_indexes.next().is_some() {
                return None;
            }
            match items.get_mut(text_index)? {
                FunctionCallOutputContentItem::InputText { text } => Some(text),
                FunctionCallOutputContentItem::InputImage { .. }
                | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn response_item_has_valid_tool_history_receipt(item: &ResponseItem) -> bool {
    let Some((call_id, text)) = textual_output_identity(item) else {
        return false;
    };
    let Ok(receipt) = serde_json::from_str::<ToolHistoryReceiptV1>(text) else {
        return false;
    };
    receipt.version == RECEIPT_VERSION
        && receipt.call_id == call_id
        && receipt.receipt_id == receipt_id_for(call_id, &receipt.artifact.sha256)
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
                let receipt_id_matches = serde_json::from_str::<ToolHistoryReceiptV1>(text)
                    .is_ok_and(|receipt| receipt.receipt_id == substitution.receipt_id);
                call_id == substitution.call_id
                    && sha256(text.as_bytes()) == substitution.substituted_output_sha256
                    && receipt_id_matches
            })
    })
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn receipt_id_for(call_id: &str, artifact_sha256: &str) -> String {
    format!(
        "thr1-{}",
        &format!(
            "{:x}",
            Sha256::digest(format!("{call_id}:{artifact_sha256}").as_bytes())
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

pub(crate) fn tool_call_observes_workspace(tool_identity: &str, payload: &ToolPayload) -> bool {
    if !tool_observes_workspace(tool_identity) {
        return false;
    }
    workspace_call_observes_from_arguments(workspace_call_arguments(payload).as_ref())
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

/// Returns whether a workspace-capable process call has an independently
/// enforced read-only execution boundary and may use the shared gate.
pub(crate) fn tool_call_is_proven_read_only(_tool_identity: &str, _payload: &ToolPayload) -> bool {
    // A command-name mutation heuristic cannot prove that launching a process
    // is side-effect-free: validation commands can run build scripts and an
    // otherwise read-oriented executable can be replaced or configured to
    // write. Keep workspace-capable process calls behind the exclusive gate.
    false
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

pub(crate) fn find_cargo_package_directory(package: &str, cwd: &Path) -> Option<PathBuf> {
    find_cargo_package_directory_with_manifest_reader(package, cwd, |path| {
        std::fs::read_to_string(path).ok()
    })
}

pub(crate) fn find_cargo_package_directory_with_manifest_reader(
    package: &str,
    cwd: &Path,
    mut read_manifest: impl FnMut(&Path) -> Option<String>,
) -> Option<PathBuf> {
    let mut candidates = BTreeSet::new();
    for ancestor in cwd.ancestors() {
        for candidate in [ancestor.to_path_buf(), ancestor.join("codex-rs")] {
            candidates.insert(
                dunce::canonicalize(&candidate).unwrap_or_else(|_| candidate.to_path_buf()),
            );
        }
    }

    let mut manifest_cache = BTreeMap::new();
    let mut roots = Vec::new();
    for candidate in candidates {
        let manifest = cached_cargo_manifest(
            &candidate.join("Cargo.toml"),
            &mut manifest_cache,
            &mut read_manifest,
        );
        if manifest
            .as_ref()
            .and_then(|manifest| manifest.parsed.as_ref())
            .is_some_and(|parsed| parsed.get("workspace").is_some())
        {
            roots.push(candidate);
        }
    }

    for root in roots {
        let graph = cargo_workspace_graph_with_manifest_cache(
            &root,
            &mut manifest_cache,
            &mut read_manifest,
        );
        if let Some(directory) = graph.packages.get(package) {
            return Some(directory.clone());
        }
    }
    None
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
