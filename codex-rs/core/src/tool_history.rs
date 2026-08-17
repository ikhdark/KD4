use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Write;
use std::sync::Arc;

use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::tools::command_output_artifact::reconcile_active_tool_history_artifact_protection;
use crate::tools::command_output_artifact::remint_tool_history_artifact_for_thread;

const RECEIPT_VERSION: u8 = 1;
const RECEIPT_MAX_TOKENS: usize = 256;
const RECEIPT_DIGEST_TARGET_TOKENS: usize = 96;
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
    digest: String,
    artifact: ReceiptArtifact,
    original: ReceiptOriginalSize,
    retrieval: ReceiptRetrieval,
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
    pub(crate) artifact_id: String,
    pub(crate) artifact_bytes: u64,
    pub(crate) artifact_sha256: String,
    pub(crate) original_output_sha256: String,
    pub(crate) original_tokens: u64,
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
        if !self.complete || !self.projection_eligible || self.consumed_by_generation.is_none() {
            return None;
        }
        let bounded_tokens =
            u64::try_from(approx_token_count(&self.bounded_model_output)).unwrap_or(u64::MAX);
        if bounded_tokens < MINIMUM_RAW_TOKENS {
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
                digest: truncate_text_to_token_ceiling(&self.bounded_model_output, digest_limit),
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
                if saved < MINIMUM_SAVED_TOKENS || relative < MINIMUM_RELATIVE_SAVINGS_PERCENT {
                    return None;
                }
                return Some((receipt_id, rendered));
            }
            if digest_limit == 0 {
                return None;
            }
            digest_limit = digest_limit.saturating_sub(32);
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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ToolHistoryState {
    candidates: BTreeMap<String, ToolHistoryCandidate>,
}

impl ToolHistoryState {
    pub(crate) fn register(&mut self, candidate: ToolHistoryCandidate) {
        self.candidates.insert(candidate.call_id.clone(), candidate);
    }

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
            .map(|(call_id, text)| (call_id.to_string(), sha256(text.as_bytes())))
            .collect::<BTreeMap<_, _>>();
        let mut changed = false;
        for candidate in self.candidates.values_mut() {
            if candidate.consumed_by_generation.is_some() {
                continue;
            }
            let bounded_output_sha256 = sha256(candidate.bounded_model_output.as_bytes());
            if exposed.get(&candidate.call_id) == Some(&bounded_output_sha256) {
                candidate.consumed_by_generation = Some(generation.clone());
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn project(&self, items: Arc<[ResponseItem]>) -> ToolHistoryProjection {
        let mut projected = items.to_vec();
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
            let Some((receipt_id, receipt)) = candidate.receipt() else {
                continue;
            };
            let substituted_output_sha256 = sha256(receipt.as_bytes());
            *output = receipt;
            substitutions.push(ToolHistorySubstitution {
                item_index,
                call_id: call_id.to_string(),
                bounded_output_sha256,
                receipt_id,
                substituted_output_sha256,
            });
        }
        ToolHistoryProjection {
            items: Arc::from(projected),
            unreplaced_items: items,
            substitutions: Arc::from(substitutions),
        }
    }

    pub(crate) fn retain_for_history(&mut self, items: &[ResponseItem]) {
        let live = items
            .iter()
            .filter_map(output_call_id)
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        self.candidates.retain(|call_id, _| live.contains(call_id));
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
        #[cfg(unix)]
        std::fs::File::open(directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|err| format!("failed to sync tool-history ledger directory: {err}"))?;
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

#[cfg(test)]
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

fn output_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCallOutput { call_id, .. }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id),
        _ => None,
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
#[path = "tool_history_tests.rs"]
mod tests;
