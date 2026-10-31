//! Large-file and explicit binary pages share live revision-bound range reads.
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::FILE_RANGE_MAX_BYTES;
use codex_file_system::FileSystemSandboxContext;
use codex_tools::JsonToolOutput;
use codex_utils_path_uri::PathUri;
use serde_json::json;

use super::FileSelector;
use super::ReadFileArgs;
use super::ToolOutputSelector;
use crate::FunctionCallError;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;

#[derive(Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ByteEncoding {
    Hex,
    Base64,
}

pub(super) fn binary_recovery(
    path: &PathUri,
    environment_id: &str,
    start: u64,
    end: u64,
    revision: Option<&str>,
) -> serde_json::Value {
    json!(["hex", "base64"].map(|encoding| {
        let mut arguments = json!({
            "path":path.inferred_native_path_string(), "environment_id":environment_id,
            "encoding":encoding,
            "selectors":[{"kind":"bytes", "start":start, "end":end.min(start.saturating_add(256))}],
        });
        if let Some(revision) = revision {
            arguments["if_file_revision"] = json!(revision);
        }
        json!({"tool":"read_file", "arguments":arguments})
    }))
}

pub(super) async fn read_ranges(
    invocation: &ToolInvocation,
    args: &ReadFileArgs,
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    sandbox: &FileSystemSandboxContext,
    environment_id: &str,
) -> Result<JsonToolOutput, FunctionCallError> {
    let error = |message: String| FunctionCallError::RespondToModel(message);
    let selectors = args.selectors.as_deref().filter(|selectors| !selectors.is_empty())
        .ok_or_else(|| error("Files over 8 MiB require bounded bytes selectors; no whole-file snapshot is acquired.".into()))?;
    if args.if_source_hash.is_some() {
        return Err(error("if_source_hash requires a whole-file snapshot (at most 8 MiB); use if_file_revision for large-file byte pages.".into()));
    }
    let mut requested = Vec::new();
    for selector in selectors {
        let FileSelector::Exact(ToolOutputSelector::Bytes { start, end }) = selector else {
            return Err(error("Files over 8 MiB support only bytes selectors; lines, search and structure still require a whole-file snapshot.".into()));
        };
        if end <= start {
            return Err(error("byte ranges require end > start".into()));
        }
        requested.push((*start, *end));
    }
    let script = matches!(invocation.source, ToolCallSource::CodeMode { .. });
    // Leave room for worst-case JSON escaping and metadata on direct delivery.
    // Script pages stay well below the existing 1 MiB serialized payload cap.
    let mut remaining = if script {
        FILE_RANGE_MAX_BYTES
    } else {
        let policy: codex_protocol::protocol::TruncationPolicy = invocation
            .step_context
            .turn
            .model_info
            .truncation_policy
            .into();
        invocation.step_context.model_output_token_limit(
            &invocation.session, policy.token_budget(),
        ).await
            .saturating_sub(2_000)
            .saturating_mul(4)
            .saturating_div(6)
            .clamp(1, 8_192)
    };
    let mut revision = args.if_file_revision.clone();
    let mut file_bytes = None;
    let mut results = Vec::new();
    let mut pending = Vec::new();
    let mut cursor = 0;
    while cursor < requested.len() && remaining > 0 {
        // Fit whole selectors together; the first oversized selector gets its
        // original bounded prefix. EOF savings remain usable by the next group.
        // Duplicate delivery still counts against the existing payload budget.
        let mut group = Vec::new();
        let mut planned = 0usize;
        while let Some(&(start, end)) = requested.get(cursor) {
            let len = (end - start).min(remaining as u64) as usize;
            if !group.is_empty() && len > remaining - planned { break; }
            group.push((cursor, start, end, start + len as u64));
            planned += len;
            cursor += 1;
            if planned == remaining { break; }
        }
        let merged = crate::tools::command_output_artifact::normalize_byte_ranges_with_fit(
            group.iter().map(|&(_, start, _, end)| (start, end)).collect(),
            |_, _| true,
        );
        let mut acquired = Vec::with_capacity(merged.len());
        for (start, end) in merged {
            let read = tokio::select! {
                biased;
                _ = invocation.cancellation_token.cancelled() => return Err(error("read_file cancelled".into())),
                read = fs.read_file_range(path, start, (end - start) as usize, revision.as_deref(), Some(sandbox)) =>
                    read.map_err(|err| error(format!("unable to read byte range: {err}")))?,
            };
            if file_bytes.is_some_and(|size| size != read.file_bytes)
                || revision.as_ref().is_some_and(|expected| expected != &read.revision)
                || read.bytes.len() as u64 != end.min(read.file_bytes).saturating_sub(start)
            {
                return Err(error("file revision changed or range incomplete during byte-range batch; no mixed-revision bytes delivered".into()));
            }
            file_bytes = Some(read.file_bytes);
            revision = Some(read.revision);
            acquired.push((start, end, read.bytes));
        }
        for (selector_index, start, end, planned_end) in group {
            let (base, _, bytes) = acquired.iter()
                .find(|(base, limit, _)| *base <= start && *limit >= planned_end)
                .expect("normalized range covers each admitted selector");
            let offset = (start - base).min(bytes.len() as u64) as usize;
            let len = (planned_end - start).min(bytes.len().saturating_sub(offset) as u64) as usize;
            let bytes = &bytes[offset..offset + len];
            let delivered_end = start + len as u64;
            let selected_end = end.min(file_bytes.expect("range acquired")).max(start);
            let complete = delivered_end == selected_end;
            remaining -= len;
            let mut result = json!({
                "selector":{"kind":"bytes", "start":start, "end":end},
                "selector_index":selector_index,
                "canonical_range":{"start":start, "end":delivered_end},
                "exact_bytes":len, "status":"ok", "complete":complete,
            });
            match args.encoding {
                Some(ByteEncoding::Hex) => result["data_hex"] = json!(hex::encode(bytes)),
                Some(ByteEncoding::Base64) => result["data_base64"] = json!(STANDARD.encode(bytes)),
                None => {
                    if let Some(classification) = super::encoding::classify_page(bytes, start == 0) {
                        result["classification"] = json!(classification);
                        result["binary_recovery"] = binary_recovery(
                            path, environment_id, start, delivered_end, revision.as_deref(),
                        );
                        result["data_base64"] = json!(STANDARD.encode(bytes));
                    } else if let Ok(text) = std::str::from_utf8(bytes) {
                        result["text"] = json!(text);
                    }
                }
            }
            if !complete {
                let next = json!({"kind":"bytes", "start":delivered_end, "end":end});
                result["continuation"] = next.clone();
                pending.push(next);
            }
            results.push(result);
        }
    }
    pending.extend(requested[cursor..].iter().map(|&(start, end)| json!({"kind":"bytes","start":start,"end":end})));
    if invocation.cancellation_token.is_cancelled() {
        return Err(error("read_file cancelled".into()));
    }
    let complete = pending.is_empty();
    let mut output = json!({
        "path":path.inferred_native_path_string(), "canonical_uri":path.to_string(),
        "environment_id":environment_id, "read_mode":"byte_ranges",
        "selector_coordinate_space":"source_bytes",
        "file_revision":revision, "file_bytes":file_bytes,
        "complete":complete, "delivered_selection_complete":complete,
        "selection_status":if complete { "complete" } else { "partial" },
        "file_complete":false, "retained_bytes":0, "retained_artifact_complete":false,
        "results":results,
        "delivery_note":"Only the returned byte ranges were acquired. No full-file hash, line count or snapshot is available; file_revision is an opened-object metadata revision, not a content hash. Further pages reread the current file and refuse a changed revision.",
    });
    if let Some(encoding) = args.encoding {
        output["encoding"] = json!(encoding);
    }
    if !pending.is_empty() {
        output["range_recovery"] = json!({"tool":"read_file", "arguments":{
            "path":path.inferred_native_path_string(), "environment_id":environment_id,
            "if_file_revision":revision, "selectors":pending,
        }});
        if let Some(encoding) = args.encoding {
            output["range_recovery"]["arguments"]["encoding"] = json!(encoding);
        }
    }
    // Preserve explicit no-retention metadata on both direct and script paths.
    let result = JsonToolOutput::with_success(output.clone(), Some(true)).with_model_value(output);
    crate::tools::context::ensure_exact_model_delivery(&result, invocation)?;
    Ok(result)
}
