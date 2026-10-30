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
        policy
            .token_budget()
            .saturating_sub(2_000)
            .saturating_mul(4)
            .saturating_div(6)
            .clamp(1, 8_192)
    };
    let mut revision = args.if_file_revision.clone();
    let mut file_bytes = None;
    let mut results = Vec::new();
    let mut pending = Vec::new();
    for (start, end) in requested {
        if remaining == 0 {
            pending.push(json!({"kind":"bytes", "start":start, "end":end}));
            continue;
        }
        if invocation.cancellation_token.is_cancelled() {
            return Err(error("read_file cancelled".into()));
        }
        let len = (end - start).min(remaining as u64) as usize;
        let read = fs
            .read_file_range(path, start, len, revision.as_deref(), Some(sandbox))
            .await
            .map_err(|err| error(format!("unable to read byte range: {err}")))?;
        if file_bytes.is_some_and(|size| size != read.file_bytes)
            || revision
                .as_ref()
                .is_some_and(|expected| expected != &read.revision)
        {
            return Err(error(
                "file revision changed during byte-range batch; no mixed-revision bytes delivered"
                    .into(),
            ));
        }
        file_bytes = Some(read.file_bytes);
        revision = Some(read.revision);
        let delivered_end = start + read.bytes.len() as u64;
        let selected_end = end.min(read.file_bytes);
        let complete = delivered_end == selected_end;
        remaining -= read.bytes.len();
        let mut result = json!({
            "selector":{"kind":"bytes", "start":start, "end":end},
            "canonical_range":{"start":start, "end":delivered_end},
            "exact_bytes":read.bytes.len(), "status":"ok", "complete":complete,
        });
        match args.encoding {
            Some(ByteEncoding::Hex) => result["data_hex"] = json!(hex::encode(&read.bytes)),
            Some(ByteEncoding::Base64) => {
                result["data_base64"] = json!(STANDARD.encode(&read.bytes))
            }
            // NUL bytes mark binary data or UTF-16 code units even when the
            // page is valid UTF-8; deliver those pages as bytes, not text.
            None => match std::str::from_utf8(&read.bytes) {
                Ok(text) if !read.bytes.contains(&0) => result["text"] = json!(text),
                _ => result["data_base64"] = json!(STANDARD.encode(&read.bytes)),
            },
        }
        if !complete {
            let next = json!({"kind":"bytes", "start":delivered_end, "end":end});
            result["continuation"] = next.clone();
            pending.push(next);
        }
        results.push(result);
    }
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
