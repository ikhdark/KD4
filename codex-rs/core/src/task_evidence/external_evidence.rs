//! MCP evidence metadata validation and canonical artifact encoding.

use codex_protocol::mcp::CallToolResult;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use super::EXTERNAL_EVIDENCE_ARTIFACT_CHUNK_BYTES;
use super::EXTERNAL_EVIDENCE_ARTIFACT_HEADER;
use super::canonicalize_json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum EvidenceCompleteness {
    Complete,
    Partial,
    Unknown,
}

pub(super) struct ExternalEvidenceMetadata {
    pub(super) producer: String,
    pub(super) producer_schema_version: u32,
    pub(super) provider_snapshot: Option<String>,
    pub(super) payload_completeness: EvidenceCompleteness,
    pub(super) truncated: bool,
    pub(super) approximate: bool,
    pub(super) limitations: Vec<String>,
}

pub(super) fn extract_external_evidence_metadata(
    result: &CallToolResult,
) -> Result<Option<ExternalEvidenceMetadata>, &'static str> {
    let Some(structured) = result.structured_content.as_ref() else {
        return Ok(None);
    };
    let Some(evidence_meta) = structured.get("evidenceMeta") else {
        return Ok(None);
    };
    let Some(meta) = evidence_meta.as_object() else {
        return Err("MCP evidenceMeta is malformed and was ignored");
    };
    let Some(schema_version) = meta.get("schemaVersion").and_then(Value::as_u64) else {
        return Err("MCP evidenceMeta schemaVersion is malformed and was ignored");
    };
    if schema_version != 1 {
        return Err("MCP evidenceMeta schemaVersion is unsupported and was ignored");
    }
    let Some(producer) = meta.get("producer").and_then(Value::as_str) else {
        return Err("MCP evidenceMeta producer is malformed and was ignored");
    };
    if producer.trim().is_empty() {
        return Err("MCP evidenceMeta producer is malformed and was ignored");
    }
    let Some(evidence_bearing) = meta.get("evidenceBearing").and_then(Value::as_bool) else {
        return Err("MCP evidenceMeta evidenceBearing is malformed and was ignored");
    };
    if !evidence_bearing {
        return Ok(None);
    }
    if meta
        .get("operation")
        .and_then(Value::as_str)
        .is_none_or(|operation| operation.trim().is_empty())
    {
        return Err("MCP evidenceMeta operation is malformed and was ignored");
    }
    let payload_completeness = match meta.get("payloadCompleteness").and_then(Value::as_str) {
        Some("complete") => EvidenceCompleteness::Complete,
        Some("partial") => EvidenceCompleteness::Partial,
        Some("unknown") => EvidenceCompleteness::Unknown,
        _ => return Err("MCP evidenceMeta payloadCompleteness is malformed and was ignored"),
    };
    let Some(truncated) = meta.get("truncated").and_then(Value::as_bool) else {
        return Err("MCP evidenceMeta truncated flag is malformed and was ignored");
    };
    if payload_completeness == EvidenceCompleteness::Complete && truncated {
        return Err("MCP evidenceMeta complete payload cannot be truncated and was ignored");
    }
    let Some(approximate) = meta.get("approximate").and_then(Value::as_bool) else {
        return Err("MCP evidenceMeta approximate flag is malformed and was ignored");
    };
    let Some(limitations) = meta.get("limitations").and_then(Value::as_array) else {
        return Err("MCP evidenceMeta limitations are malformed and were ignored");
    };
    let Some(limitations) = limitations
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
    else {
        return Err("MCP evidenceMeta limitations are malformed and were ignored");
    };
    let provider_snapshot = match meta.get("snapshot") {
        None | Some(Value::Null) => None,
        Some(Value::String(snapshot)) => Some(snapshot.clone()),
        Some(_) => return Err("MCP evidenceMeta snapshot is malformed and was ignored"),
    };
    Ok(Some(ExternalEvidenceMetadata {
        producer: producer.to_string(),
        producer_schema_version: schema_version as u32,
        provider_snapshot,
        payload_completeness,
        truncated,
        approximate,
        limitations,
    }))
}

pub(super) fn canonical_mcp_result_payload(result: &CallToolResult) -> Value {
    canonicalize_json(&serde_json::json!({
        "content": result.content,
        "structuredContent": result.structured_content,
        "isError": result.is_error,
    }))
}

pub(super) const fn evidence_completeness_name(completeness: EvidenceCompleteness) -> &'static str {
    match completeness {
        EvidenceCompleteness::Complete => "complete",
        EvidenceCompleteness::Partial => "partial",
        EvidenceCompleteness::Unknown => "unknown",
    }
}

pub(super) fn encode_external_evidence_artifact(canonical_bytes: &[u8]) -> Option<Vec<u8>> {
    let canonical = std::str::from_utf8(canonical_bytes).ok()?;
    let mut encoded = Vec::with_capacity(canonical_bytes.len() + 256);
    encoded.extend_from_slice(EXTERNAL_EVIDENCE_ARTIFACT_HEADER.as_bytes());
    let mut start = 0;
    while start < canonical.len() {
        let mut end = (start + EXTERNAL_EVIDENCE_ARTIFACT_CHUNK_BYTES).min(canonical.len());
        while !canonical.is_char_boundary(end) {
            end -= 1;
        }
        let line = Value::String(canonical[start..end].to_string()).to_string();
        encoded.extend_from_slice(line.as_bytes());
        encoded.push(b'\n');
        start = end;
    }
    Some(encoded)
}
