//! Canonical source snapshot assembly and recovery recipes.
use codex_tools::CanonicalToolResult;
use crate::{ReadToolOutputResult, ReadToolOutputError, ToolOutputSelector, raw_json_pointer_index};
use serde_json::json;

pub fn file_snapshot(
    contents: String,
    selectors: Option<&[ToolOutputSelector]>,
) -> Result<CanonicalToolResult, ReadToolOutputError> {
    let mut canonical = CanonicalToolResult::text(contents);
    if selectors
        .into_iter()
        .flatten()
        .any(|selector| matches!(selector, ToolOutputSelector::JsonPointer { .. }))
    {
        // Index failure belongs to JSON selectors, not the raw snapshot or its
        // independent line/byte/search siblings. The selector owner reports it.
        canonical.json_pointers = raw_json_pointer_index(&canonical.bytes).unwrap_or_default();
    }
    Ok(canonical)
}

pub fn file_recovery_recipe(
    output: &serde_json::Value,
    artifact_id: &str,
    max_selectors: usize,
) -> Option<serde_json::Value> {
    let mut pending = Vec::new();
    if let Some(continuation) = output.get("continuation").filter(|value| !value.is_null()) {
        pending.push(continuation.clone());
    }
    for result in output["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|result| result["complete"] == false)
    {
        let children = result["child_selectors"].as_array();
        let missing_context = result["selector"]["kind"] == "search"
            && result["status"] == "ok"
            && result["value"]["hydrated_ranges"]
                .as_array()
                .is_some_and(Vec::is_empty);
        if missing_context || result["status"] != "ok" {
            pending.extend(children.into_iter().flatten().cloned());
            // Direct overflow may advertise just the first bounded byte child.
            // Include its unconsumed suffix rather than silently losing it.
            if let Some(end) = result["canonical_range"]["end"].as_u64()
                && let Some(last) = children
                    .into_iter()
                    .flatten()
                    .filter(|child| child["kind"] == "bytes")
                    .filter_map(|child| child["end"].as_u64())
                    .max()
                && last < end
            {
                pending.push(json!({"kind":"bytes", "start":last, "end":end}));
            }
        }
        if let Some(continuation) = result.get("continuation").filter(|value| !value.is_null()) {
            if result["selector"]["kind"] == "search" && result["status"] == "ok" {
                // Recover only the undelivered portion of the requested page,
                // not another full page beyond the caller's max_results.
                let requested = result["selector"]["max_results"]
                    .as_u64()
                    .unwrap_or(20)
                    .min(crate::ARTIFACT_SEARCH_MAX_RESULTS as u64)
                    .min(result["value"]["total_matches"].as_u64().unwrap_or(0));
                let remaining = requested
                    .saturating_sub(result["value"]["matches_returned"].as_u64().unwrap_or(0));
                if remaining > 0 {
                    let mut continuation = continuation.clone();
                    continuation["max_results"] = json!(remaining);
                    pending.push(continuation);
                }
            } else if result["selector"]["kind"] == "search" || children.is_none_or(Vec::is_empty) {
                // Exact overflows with child ranges already describe their remainder.
                pending.push(continuation.clone());
            }
        }
    }
    let mut unique = Vec::new();
    for selector in pending {
        if !unique.contains(&selector) {
            unique.push(selector);
        }
    }
    let mut selectors = Vec::new();
    let mut bytes = 0usize;
    let mut omitted = 0;
    for selector in &unique {
        let size = selector.to_string().len();
        if selectors.len() < max_selectors
            && bytes.saturating_add(size) <= 16 * 1024
        {
            bytes += size;
            selectors.push(selector.clone());
        } else {
            omitted += 1;
        }
    }
    if selectors.is_empty() {
        return None;
    }
    let mut recipe = json!({
        "tool":"read_tool_output",
        "snapshot": {
            "artifact_id":artifact_id,
            "canonical_sha256":output.get("canonical_sha256").unwrap_or(&output["source_sha256"]),
            "canonical_bytes":output["canonical_bytes"],
        },
        "selection_complete":output["complete"] == true,
        "file_complete":output["file_complete"] == true,
        "retained_artifact_complete":output["retained_artifact_complete"] == true,
        "remaining_selectors":unique,
        "arguments":{"artifact_id":artifact_id, "selectors":selectors},
    });
    if omitted > 0 {
        recipe["omitted_selectors"] = json!(omitted);
    }
    Some(recipe)
}

/// Coverage belongs to delivered bytes, not requested ranges or retained storage.
/// Shared search references need not be counted: their original text is already
/// present in an earlier result of this same response.
pub fn file_selection_complete(result: &ReadToolOutputResult) -> bool {
    let ranges = result.delivered_ranges();
    // An explicit empty byte selection can cover an empty file; no matches or
    // an invalid selector cannot claim coverage merely because its size is zero.
    if ranges.is_empty() {
        return false;
    }
    let mut covered = 0;
    for (start, end) in ranges {
        if start > covered {
            break;
        }
        covered = covered.max(end);
    }
    covered == result.canonical_bytes
}
