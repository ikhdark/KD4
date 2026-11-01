//! Core authorization and exact-delivery adapter for revision-bound range reads.
use codex_file_system::{ExecutorFileSystem, FILE_RANGE_MAX_BYTES, FileSystemSandboxContext};
use codex_tools::JsonToolOutput;
use codex_utils_path_uri::PathUri;
use codex_tool_artifacts::source::ranges::RangeReadOptions;
pub(super) use codex_tool_artifacts::source::ranges::{ByteEncoding, binary_recovery};
use super::ReadFileArgs;
use crate::FunctionCallError;
use crate::tools::context::{ToolCallSource, ToolInvocation};

pub(super) async fn read_ranges(
    invocation: &ToolInvocation,
    args: &ReadFileArgs,
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    sandbox: &FileSystemSandboxContext,
    environment_id: &str,
) -> Result<JsonToolOutput, FunctionCallError> {
    let script = matches!(invocation.source, ToolCallSource::CodeMode { .. });
    // Leave room for worst-case JSON escaping and metadata on direct delivery.
    // Script pages stay well below the existing 1 MiB serialized payload cap.
    let remaining = if script {
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

    let output = codex_tool_artifacts::source::ranges::read_ranges(
        RangeReadOptions {
            selectors: args.selectors.as_deref(),
            if_source_hash: args.if_source_hash.as_deref(),
            if_file_revision: args.if_file_revision.as_deref(),
            encoding: args.encoding,
            max_bytes: remaining,
        },
        &invocation.cancellation_token,
        fs, path, sandbox, environment_id,
    ).await?;
    let result = JsonToolOutput::with_success(output.clone(), Some(true)).with_model_value(output);
    crate::tools::context::ensure_exact_model_delivery(&result, invocation)?;
    Ok(result)
}
