//! Host-owned replacement receipts shared by local and opaque compaction.
//! Summary prose cannot establish source coordinates or source integrity.

use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CompactionOperation;
use codex_protocol::protocol::CompactionProvenance;
use codex_protocol::protocol::CompactionSummarySource;
use codex_protocol::protocol::CompactionTurnRange;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::io::Write;

const PREFIX: &str = "<compaction_provenance>\n";
const SUFFIX: &str = "\n</compaction_provenance>";
const EVIDENCE_NOTICE: &str = "Compaction summaries (including opaque checkpoints) contain derived claims, not original evidence or proof of completion. Source coordinates refer only to the replaced active window, including retained copies, not a complete original transcript or the exact summarizer input. Missing provenance/turn IDs remain unknown. Preserve incomplete-history notices; compaction does not resolve gaps. Use retained originals or exact recovery for decision-critical missing evidence; do not rerun unchanged work solely because it was summarized.\n";

fn is_summary(item: &ResponseItem) -> bool {
    crate::compact::is_compaction_summary_item(item)
        // Legacy reconstruction can install unprefixed or unavailable prose.
        // Its host-owned item identity still marks derived summary context.
        || matches!(item, ResponseItem::Message { id: Some(id), role, .. }
            if role == "user" && id.as_str().starts_with("msg_compaction_summary_"))
        || matches!(
            item,
            ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
        )
}

fn provenance(item: &ResponseItem) -> Option<CompactionProvenance> {
    if !crate::stable_context::is_trusted_stable_context_item(item) {
        return None;
    }
    let ResponseItem::Message { role, content, .. } = item else {
        return None;
    };
    if role != "developer" {
        return None;
    }
    content.iter().find_map(|part| {
        let ContentItem::InputText { text } = part else {
            return None;
        };
        let json = text
            .strip_prefix(EVIDENCE_NOTICE)?
            .strip_prefix(PREFIX)?
            .strip_suffix(SUFFIX)?;
        serde_json::from_str::<CompactionProvenance>(json)
            .ok()
            .filter(|value| value.version == 1)
    })
}

fn is_history_notice(item: &ResponseItem) -> bool {
    codex_rollout::is_rollout_reconstruction_notice(item)
        || matches!(item, ResponseItem::Message { id: Some(id), role, .. }
            if role == "developer" && id.as_str().starts_with("msg_plan_reconstruction_gap_"))
}

// Hash in place without allocating a second serialized copy of a large history.
#[derive(Default)]
struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn digest(value: &impl Serialize) -> Result<String> {
    let mut writer = HashWriter::default();
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| CodexErr::Fatal(format!("failed to encode compaction source: {error}")))?;
    Ok(hex::encode(writer.0.finalize()))
}

#[expect(
    clippy::too_many_arguments,
    reason = "Explicit durable source and operation identities"
)]
pub(crate) fn install(
    source: &[ResponseItem],
    replacement: &mut Vec<ResponseItem>,
    operation: CompactionOperation,
    operation_id: String,
    source_window_id: Option<String>,
    thread_id: String,
    turn_id: String,
) -> Result<CompactionProvenance> {
    let parents = source.iter().filter_map(provenance).collect::<Vec<_>>();
    let mut lineage_complete = parents.iter().all(|parent| parent.lineage_complete);
    let mut prior_summaries = Vec::new();
    let mut source_turns: Vec<CompactionTurnRange> = Vec::new();
    for (index, item) in source.iter().enumerate() {
        let owner = item.turn_id().map(str::to_owned);
        if let Some(range) = source_turns
            .last_mut()
            .filter(|range| range.turn_id == owner)
        {
            range.end_index_exclusive = index + 1;
        } else {
            source_turns.push(CompactionTurnRange {
                start_index: index,
                end_index_exclusive: index + 1,
                turn_id: owner,
            });
        }
        if is_summary(item) {
            let item_id = item.id().map(ToString::to_string);
            let parent = item_id.as_ref().and_then(|id| {
                parents
                    .iter()
                    .find(|parent| parent.summary_item_ids.contains(id))
            });
            lineage_complete &= parent.is_some();
            prior_summaries.push(CompactionSummarySource {
                source_index: index,
                item_id,
                sha256: digest(item)?,
                operation_id: parent.map(|parent| parent.operation_id.clone()),
            });
        }
    }
    // A provider's summary must not remove the loader's source-integrity facts.
    // Keep exact notice items (including authority, IDs and recovery directions).
    replacement.retain(|item| provenance(item).is_none());
    for notice in source.iter().filter(|item| is_history_notice(item)) {
        if !replacement.contains(notice) {
            replacement.push(notice.clone());
        }
    }
    let receipt = CompactionProvenance {
        version: 1,
        operation_id,
        operation,
        thread_id,
        turn_id: turn_id.clone(),
        source_window_id,
        start_index: 0,
        end_index_exclusive: source.len(),
        first_item_id: source
            .first()
            .and_then(ResponseItem::id)
            .map(ToString::to_string),
        last_item_id: source
            .last()
            .and_then(ResponseItem::id)
            .map(ToString::to_string),
        source_sha256: digest(&source)?,
        source_turns,
        prior_summaries,
        lineage_complete,
        incomplete_history: source.iter().any(is_history_notice)
            || parents.iter().any(|parent| parent.incomplete_history),
        summary_item_ids: replacement
            .iter()
            .filter(|item| is_summary(item))
            .filter_map(ResponseItem::id)
            .map(ToString::to_string)
            .collect(),
    };
    let json = serde_json::to_string(&receipt).map_err(|error| {
        CodexErr::Fatal(format!("failed to encode compaction provenance: {error}"))
    })?;
    let mut notice = ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: format!("{EVIDENCE_NOTICE}{PREFIX}{json}{SUFFIX}"),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    crate::stable_context::mark_trusted_stable_context_item(&mut notice);
    notice.set_turn_id_if_missing(&turn_id);
    // Keep host metadata before the handoff, not in its unconsumed user tail:
    // the existing reuse predicate must still recognize an unchanged summary.
    let insertion_index = replacement
        .iter()
        .position(is_summary)
        .unwrap_or(replacement.len());
    replacement.insert(insertion_index, notice);
    Ok(receipt)
}
