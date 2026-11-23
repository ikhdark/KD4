//! A tool-disabled answer cannot follow a native read's continuation. Fill
//! ordinary exact clipped selections and restore compacted exact read results
//! from their authenticated archives. The original receipt, cap, and rollout
//! are unchanged; the prepared prompt still has to pass full-window admission.

use std::sync::Arc;
use std::sync::Mutex;

use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseInputItem;
use codex_tool_artifacts::ToolOutputSelector;
use codex_tool_artifacts::load_tool_output_snapshot;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use super::Prompt;
use super::Session;
use super::TurnContext;
use super::project_provider_read_outputs;
use super::unique_read_output;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES;

#[derive(Default)]
pub(crate) struct TerminalReadDelivery {
    clipped: Mutex<Vec<Arc<ClippedRead>>>,
    exact: Mutex<Vec<(String, [u8; 32])>>,
}

struct ClippedRead {
    call_id: String,
    original: String,
}

pub(super) struct RecoveryUnavailable;

fn range(row: &Value) -> Option<(u64, u64)> {
    Some((
        row["canonical_range"]["start"].as_u64()?,
        row["canonical_range"]["end"].as_u64()?,
    ))
}

pub(crate) fn is_clipped_selection(packet: &Value) -> bool {
    let Some(bytes) = packet["canonical_bytes"].as_u64() else {
        return false;
    };
    let Some(rows) = packet["results"].as_array() else {
        return false;
    };
    let Some(id) = packet["artifact_id"].as_str() else {
        return false;
    };
    packet["complete"] == false
        && packet["retained_artifact_complete"] == true
        && packet["retained_bytes"] == bytes
        && packet["source_sha256"].is_string()
        && packet["recovery"]["tool"] == "read_tool_output"
        && packet["recovery"]["arguments"]["artifact_id"] == id
        && packet["recovery"]["snapshot"]["canonical_sha256"] == packet["source_sha256"]
        && packet["recovery"]["snapshot"]["canonical_bytes"] == bytes
        && [
            "continuation",
            "page_selectors",
            "delivery_note",
            "selector_errors",
            "selector_bindings",
            "selector_errors_omitted",
            "selector_bindings_omitted",
            "snapshot_error",
            "inspection_proof_error",
            "evidence_advisory",
        ]
        .iter()
        .all(|key| packet.get(*key).is_none())
        && rows.len() <= 64
        && rows.iter().any(|row| row["status"] == "selector_too_large")
        && rows.iter().all(|row| {
            matches!(row["selector"]["kind"].as_str(), Some("bytes" | "lines"))
                && range(row).is_some_and(|(start, end)| {
                    start < end
                        && end <= bytes
                        && match row["status"].as_str() {
                            Some("selector_too_large") => {
                                row["complete"] == false && row.get("text").is_none()
                            }
                            Some("ok") => {
                                row["complete"] == true
                                    && row["text"]
                                        .as_str()
                                        .is_some_and(|s| s.len() as u64 == end - start)
                            }
                            _ => false,
                        }
                })
        })
}

impl TerminalReadDelivery {
    // Keep identity, not a second copy of an already delivered result. It may
    // outlive the offer when compaction archives a previously inline sibling.
    pub(crate) fn remember_exact(&self, call_id: &str, digest: [u8; 32]) {
        self.exact
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((call_id.to_owned(), digest));
    }

    // The registry calls this only for ordinary provider-visible reads whose
    // hooks and workspace checks retained the producer's original receipt.
    pub(crate) fn stage(&self, response: &ResponseInputItem) {
        let ResponseInputItem::FunctionCallOutput { call_id, output } = response else {
            return;
        };
        let FunctionCallOutputBody::Text(original) = &output.body else {
            return;
        };
        if !serde_json::from_str::<Value>(original)
            .is_ok_and(|packet| is_clipped_selection(&packet))
        {
            return;
        }
        self.clipped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::new(ClippedRead {
                call_id: call_id.clone(),
                original: original.clone(),
            }));
    }
}

pub(super) async fn project(
    sess: &Session,
    turn: &TurnContext,
    prompt: &Prompt,
) -> Result<Option<Prompt>, RecoveryUnavailable> {
    let Some(delivery) = turn.extension_data.get::<TerminalReadDelivery>() else {
        return Ok(None);
    };
    let entries = delivery
        .clipped
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let inputs = [
        &prompt.input,
        &prompt.stable_context_fallback_input,
        &prompt.tool_history_fallback_input,
        &prompt.stable_context_tool_history_fallback_input,
    ];
    let mut remaining = READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES as u64;
    let mut hydrated = Vec::new();
    let thread_id = sess.thread_id().to_string();
    for entry in &entries {
        if !inputs.iter().any(|input| {
            unique_read_output(input, &entry.call_id)
                .is_some_and(|(_, text)| text == entry.original)
        }) {
            continue;
        }
        // All variants refer to the same authenticated snapshot. Load it
        // once, then substitute only the exact matching original receipts.
        let mut packet: Value =
            serde_json::from_str(&entry.original).map_err(|_| RecoveryUnavailable)?;
        let artifact = packet["artifact_id"].as_str().ok_or(RecoveryUnavailable)?;
        let bytes = packet["canonical_bytes"]
            .as_u64()
            .ok_or(RecoveryUnavailable)?;
        let hash = packet["source_sha256"]
            .as_str()
            .ok_or(RecoveryUnavailable)?;
        let rows = packet["results"].as_array().ok_or(RecoveryUnavailable)?;
        let needed = rows
            .iter()
            .filter(|row| row["status"] == "selector_too_large")
            .try_fold(0_u64, |total, row| {
                let (start, end) = range(row)?;
                total.checked_add(end.checked_sub(start)?)
            })
            .ok_or(RecoveryUnavailable)?;
        remaining = remaining.checked_sub(needed).ok_or(RecoveryUnavailable)?;
        let snapshot = load_tool_output_snapshot(&turn.config.codex_home, &thread_id, artifact)
            .await
            .map_err(|_| RecoveryUnavailable)?;
        if snapshot.historical_authenticity() != "authenticated" {
            return Err(RecoveryUnavailable);
        }
        let mut rows = rows.clone();
        let mut ranges = Vec::with_capacity(rows.len());
        for row in &mut rows {
            let (start, end) = range(row).ok_or(RecoveryUnavailable)?;
            ranges.push((start, end));
            if row["status"] != "selector_too_large" {
                continue;
            }
            let selected = snapshot
                .select_within_source_bytes(
                    vec![ToolOutputSelector::Bytes { start, end }],
                    READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES,
                    (end - start) as usize,
                )
                .await
                .map_err(|_| RecoveryUnavailable)?;
            let [selected_row] = selected.results.as_slice() else {
                return Err(RecoveryUnavailable);
            };
            if !selected.complete
                || !selected.capture_complete
                || !selected_row.complete
                || selected.artifact_id != artifact
                || selected.canonical_bytes != bytes
                || selected.canonical_sha256 != hash
                || !selected_row
                    .canonical_range
                    .as_ref()
                    .is_some_and(|r| (r.start, r.end) == (start, end))
            {
                return Err(RecoveryUnavailable);
            }
            let text = selected_row
                .text
                .as_ref()
                .filter(|text| text.len() as u64 == end - start)
                .ok_or(RecoveryUnavailable)?;
            let fields = row.as_object_mut().ok_or(RecoveryUnavailable)?;
            for key in [
                "continuation",
                "child_selectors",
                "subdivision_plan",
                "message",
            ] {
                fields.remove(key);
            }
            row["status"] = json!("ok");
            row["complete"] = json!(true);
            row["exact_bytes"] = json!(end - start);
            row["text"] = json!(text);
        }
        ranges.sort_unstable();
        let covered = ranges.into_iter().try_fold(0_u64, |end, (start, next)| {
            (start <= end).then_some(end.max(next))
        });
        packet["results"] = json!(rows);
        packet["complete"] = json!(true);
        packet["delivered_selection_complete"] = json!(true);
        packet["selection_status"] = json!("complete");
        packet["file_complete"] = json!(covered == Some(bytes));
        packet["delivery_note"] = json!(
            "Exact selected bytes were included from the retained snapshot for this tool-disabled request. The original bounded read receipt is unchanged."
        );
        packet
            .as_object_mut()
            .ok_or(RecoveryUnavailable)?
            .remove("recovery");
        hydrated.push((
            entry.call_id.as_str(),
            entry.original.as_str(),
            packet.to_string(),
        ));
    }
    let exact = delivery
        .exact
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for (call_id, digest) in &exact {
        for input in inputs {
            let Some((_, original)) = unique_read_output(input, call_id) else {
                continue;
            };
            // A full read is already usable. Only inspect the compaction
            // owner's typed locator, not arbitrary tool output or plan pins.
            if !original.contains("compaction_unread_tool_output")
                || hydrated
                    .iter()
                    .any(|(id, body, _)| *id == call_id.as_str() && *body == original)
            {
                continue;
            }
            let Ok(pin) = serde_json::from_str::<Value>(original) else {
                continue;
            };
            if pin["kind"] != "compaction_unread_tool_output" {
                continue;
            }
            let full = recover_compacted_read(
                turn,
                &thread_id,
                call_id,
                digest,
                &pin["recovery"],
                &mut remaining,
            )
            .await?;
            hydrated.push((call_id.as_str(), original, full));
        }
    }
    let replacements = hydrated
        .iter()
        .map(|&(id, body, ref full)| (id, body, full.as_str()))
        .collect::<Vec<_>>();
    Ok((!replacements.is_empty()).then(|| project_provider_read_outputs(prompt, &replacements)))
}

async fn recover_compacted_read(
    turn: &TurnContext,
    thread_id: &str,
    call_id: &str,
    digest: &[u8; 32],
    recovery: &Value,
    remaining: &mut u64,
) -> Result<String, RecoveryUnavailable> {
    let artifact = recovery["artifact_id"]
        .as_str()
        .ok_or(RecoveryUnavailable)?;
    let hash = recovery["canonical_sha256"]
        .as_str()
        .ok_or(RecoveryUnavailable)?;
    let bytes = recovery["canonical_bytes"]
        .as_u64()
        .ok_or(RecoveryUnavailable)?;
    if recovery["kind"] != "local_compaction_text_recovery" || recovery["complete"] != true {
        return Err(RecoveryUnavailable);
    }
    *remaining = remaining.checked_sub(bytes).ok_or(RecoveryUnavailable)?;
    let snapshot = load_tool_output_snapshot(&turn.config.codex_home, thread_id, artifact)
        .await
        .map_err(|_| RecoveryUnavailable)?;
    if snapshot.historical_authenticity() != "authenticated" {
        return Err(RecoveryUnavailable);
    }
    let result = snapshot
        .select_within_source_bytes(
            vec![ToolOutputSelector::JsonPointer {
                pointer: "/items/0".to_owned(),
            }],
            READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES,
            bytes as usize,
        )
        .await
        .map_err(|_| RecoveryUnavailable)?;
    let [row] = result.results.as_slice() else {
        return Err(RecoveryUnavailable);
    };
    if !result.complete
        || !result.capture_complete
        || !row.complete
        || result.artifact_id != artifact
        || result.canonical_bytes != bytes
        || result.canonical_sha256 != hash
    {
        return Err(RecoveryUnavailable);
    }
    let item = row.value.as_ref().ok_or(RecoveryUnavailable)?;
    let body = item["output"].as_str().ok_or(RecoveryUnavailable)?;
    let observed: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    if item["type"] != "function_call_output" || item["call_id"] != call_id || &observed != digest {
        return Err(RecoveryUnavailable);
    }
    Ok(body.to_owned())
}
