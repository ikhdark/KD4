use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Write;
use std::sync::Arc;

use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::tools::command_output_artifact::reconcile_active_tool_history_artifact_protection;

const RECEIPT_VERSION: u8 = 1;
const RECEIPT_MAX_TOKENS: usize = 512;
const RECEIPT_DIGEST_TARGET_TOKENS: usize = 240;
const MINIMUM_RAW_TOKENS: u64 = 512;
const MINIMUM_SAVED_TOKENS: u64 = 128;
const MINIMUM_RELATIVE_SAVINGS_PERCENT: u64 = 25;
const LEDGER_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ModelGenerationId {
    pub(crate) turn_id: String,
    pub(crate) ordinal: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderHistoryDeliveryMode {
    ManualFullHistory,
    PreviousResponseId,
    StandaloneCompaction,
    ProviderManagedCompaction,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ProviderBaselineProvenance {
    pub(crate) delivery_mode: ProviderHistoryDeliveryMode,
    pub(crate) parent_or_baseline_identity: Option<String>,
    pub(crate) transmitted_input_fingerprint: Option<String>,
    pub(crate) transmitted_receipt_ids: BTreeSet<String>,
    pub(crate) transmitted_receipt_input_delta: u64,
    pub(crate) returned_response_or_compaction_identity: Option<String>,
    pub(crate) provider_reported_input_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ToolHistoryTokenAccounting {
    pub(crate) canonical_local_tokens: u64,
    pub(crate) prepared_projected_tokens: u64,
    pub(crate) effective_provider_tokens: Option<u64>,
    pub(crate) theoretical_savings: u64,
    pub(crate) realized_provider_savings: Option<u64>,
    pub(crate) transmitted_receipt_ids: BTreeSet<String>,
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
    pub(crate) bounded_digest: String,
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

    fn receipt(&self) -> Option<(String, String, u64)> {
        if !self.complete || !self.projection_eligible || self.consumed_by_generation.is_none() {
            return None;
        }
        if self.original_tokens < MINIMUM_RAW_TOKENS {
            return None;
        }
        let receipt_id = format!(
            "thr1-{}",
            &format!(
                "{:x}",
                Sha256::digest(format!("{}:{}", self.call_id, self.artifact_sha256).as_bytes())
            )[..16]
        );
        let mut digest_limit = RECEIPT_DIGEST_TARGET_TOKENS;
        loop {
            let receipt = ToolHistoryReceiptV1 {
                version: RECEIPT_VERSION,
                receipt_id: receipt_id.clone(),
                call_id: self.call_id.clone(),
                tool_identity: self.tool_identity.clone(),
                semantic_class: self.semantic_class.clone(),
                digest: truncate_text_to_token_ceiling(&self.bounded_digest, digest_limit),
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
                let saved = self.original_tokens.saturating_sub(receipt_tokens);
                let relative = saved
                    .saturating_mul(100)
                    .checked_div(self.original_tokens.max(1))
                    .unwrap_or(0);
                if saved < MINIMUM_SAVED_TOKENS || relative < MINIMUM_RELATIVE_SAVINGS_PERCENT {
                    return None;
                }
                return Some((receipt_id, rendered, saved));
            }
            if digest_limit == 0 {
                return None;
            }
            digest_limit = digest_limit.saturating_sub(32);
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ToolHistoryState {
    candidates: BTreeMap<String, ToolHistoryCandidate>,
    provider_authoritative_outputs: BTreeSet<String>,
    provider_baseline: Option<ProviderBaselineProvenance>,
}

impl ToolHistoryState {
    pub(crate) fn register(&mut self, candidate: ToolHistoryCandidate) {
        self.candidates.insert(candidate.call_id.clone(), candidate);
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
            if exposed.get(&candidate.call_id) == Some(&candidate.original_output_sha256) {
                candidate.consumed_by_generation = Some(generation.clone());
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn project(
        &self,
        items: Arc<[ResponseItem]>,
    ) -> (Arc<[ResponseItem]>, ToolHistoryTokenAccounting) {
        let canonical_local_tokens = items
            .iter()
            .map(crate::context_manager::estimate_item_token_count)
            .map(|tokens| u64::try_from(tokens).unwrap_or(0))
            .sum();
        let mut projected = items.to_vec();
        let mut transmitted_receipt_ids = BTreeSet::new();
        let mut theoretical_savings = 0_u64;
        for item in &mut projected {
            let Some((call_id, output)) = textual_output_mut(item) else {
                continue;
            };
            if self.provider_authoritative_outputs.contains(call_id) {
                continue;
            }
            let Some(candidate) = self.candidates.get(call_id) else {
                continue;
            };
            if sha256(output.as_bytes()) != candidate.original_output_sha256 {
                continue;
            }
            let Some((receipt_id, receipt, saved)) = candidate.receipt() else {
                continue;
            };
            *output = receipt;
            transmitted_receipt_ids.insert(receipt_id);
            theoretical_savings = theoretical_savings.saturating_add(saved);
        }
        let prepared_projected_tokens = projected
            .iter()
            .map(crate::context_manager::estimate_item_token_count)
            .map(|tokens| u64::try_from(tokens).unwrap_or(0))
            .sum();
        (
            Arc::from(projected),
            ToolHistoryTokenAccounting {
                canonical_local_tokens,
                prepared_projected_tokens,
                effective_provider_tokens: None,
                theoretical_savings,
                realized_provider_savings: None,
                transmitted_receipt_ids,
            },
        )
    }

    pub(crate) fn retain_for_history(&mut self, items: &[ResponseItem]) {
        let live = items
            .iter()
            .filter_map(output_call_id)
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        self.candidates.retain(|call_id, _| live.contains(call_id));
        self.provider_authoritative_outputs
            .retain(|call_id| live.contains(call_id));
    }

    pub(crate) fn artifact_references(&self) -> BTreeMap<String, (u64, String)> {
        self.candidates
            .values()
            .filter(|candidate| candidate.consumed_by_generation.is_some())
            .map(|candidate| {
                (
                    candidate.artifact_id.clone(),
                    candidate.artifact_reference(),
                )
            })
            .collect()
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
    let path = ledger_path(codex_home, thread_id);
    let Ok(bytes) = tokio::fs::read(path).await else {
        return ToolHistoryState::default();
    };
    let Ok(file) = serde_json::from_slice::<ToolHistoryLedgerFile>(&bytes) else {
        return ToolHistoryState::default();
    };
    if file.version != LEDGER_VERSION {
        return ToolHistoryState::default();
    }
    let mut state = file.state;
    let expected = state.artifact_references();
    let live =
        reconcile_active_tool_history_artifact_protection(codex_home, thread_id, &expected).await;
    state.candidates.retain(|_, candidate| {
        candidate.consumed_by_generation.is_none() || live.contains(&candidate.artifact_id)
    });
    state
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
        } => match &output.body {
            FunctionCallOutputBody::Text(text) => Some((call_id, text)),
            FunctionCallOutputBody::ContentItems(_) => None,
        },
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
        } => match &mut output.body {
            FunctionCallOutputBody::Text(text) => Some((call_id.as_str(), text)),
            FunctionCallOutputBody::ContentItems(_) => None,
        },
        _ => None,
    }
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
