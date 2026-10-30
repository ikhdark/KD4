use std::collections::BTreeMap;

use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::wait_for_file::sharing_violation;
use crate::tools::handlers::wait_for_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub(crate) struct ChecksumFileHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    path: String,
    environment_id: Option<String>,
    if_sha256: Option<String>,
}

impl ToolExecutor<ToolInvocation> for ChecksumFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("checksum_file")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "checksum_file".into(),
            description: "Stream the complete raw bytes of a regular file through SHA-256 in the selected environment's sandbox, with cancellation and change-during-read detection. Supports binary and large files without collecting or retaining their contents. Returns sha256, bytes_hashed and source_identity (an opaque opened-object revision including native identity and change time, not a content hash). Refuses a changed/replaced source or an unsupported backend; never certifies stability from size/mtime alone. Optional if_sha256 reports unchanged only after freshly hashing every byte. No immutable snapshot, file contents or source-inspection credit is delivered; the file can change after completion.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("path".into(), JsonSchema::string(Some("Regular file path, absolute or relative to the selected environment cwd; symlinks resolve to their file target.".into()))),
                ("environment_id".into(), JsonSchema::string(Some("Execution environment; omit for the selected default.".into()))),
                ("if_sha256".into(), JsonSchema::string(Some("Optional 64-hex SHA-256 to compare with the freshly streamed digest. Never a metadata-cache shortcut.".into()))),
            ]), Some(vec!["path".into()]), Some(false.into())),
            output_schema: Some(json!({
                "type":"object", "additionalProperties":false,
                "properties":{
                    "path":{"type":"string"}, "environment_id":{"type":"string"},
                    "algorithm":{"const":"sha256"},
                    "sha256":{"type":"string","pattern":"^[0-9a-f]{64}$"},
                    "bytes_hashed":{"type":"integer","minimum":0},
                    "source_identity":{"type":"string"},
                    "unchanged":{"type":"boolean"}
                },
                "required":["path","environment_id","algorithm","sha256","bytes_hashed","source_identity"]
            }).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let error = |message: String| FunctionCallError::RespondToModel(message);
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(error("checksum_file requires function arguments".into()));
            };
            let args: Arguments = parse_arguments(arguments)?;
            let expected = args
                .if_sha256
                .map(|hash| {
                    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        return Err(error(
                            "if_sha256 must be a 64-character hexadecimal SHA-256".into(),
                        ));
                    }
                    Ok(hash.to_ascii_lowercase())
                })
                .transpose()?;
            let checksum = checksum_source(&invocation, &args.path, args.environment_id.as_deref())
                .await
                .map_err(|failure| error(failure.message))?;
            let sha256 = &checksum.sha256;
            let mut value = json!({
                "path":checksum.path, "environment_id":checksum.environment_id,
                "algorithm":"sha256", "sha256":sha256, "bytes_hashed":checksum.bytes_hashed,
                "source_identity":checksum.source_identity,
            });
            if let Some(expected) = expected {
                value["unchanged"] = json!(expected == *sha256);
            }
            let result = JsonToolOutput::new(value);
            crate::tools::context::ensure_exact_model_delivery(&result, &invocation)?;
            Ok(boxed_tool_output(result))
        })
    }
}

pub(super) struct FileChecksum {
    pub(super) path: String,
    pub(super) environment_id: String,
    pub(super) sha256: String,
    pub(super) bytes_hashed: u64,
    pub(super) source_identity: String,
}

pub(super) struct ChecksumFailure {
    pub(super) reason: &'static str,
    pub(super) message: String,
}

/// The checksum and freshness tools share the same authorized, revision-guarded
/// streaming acquisition. No content, retention or inspection receipt escapes.
pub(super) async fn checksum_source(
    invocation: &ToolInvocation,
    path: &str,
    environment_id: Option<&str>,
) -> Result<FileChecksum, ChecksumFailure> {
    let error = |message: String| ChecksumFailure {
        reason: "source_unavailable",
        message,
    };
    let environment = wait_for_tool_environment(
        &invocation.step_context.environments,
        environment_id,
        &invocation.cancellation_token,
    )
    .await
    .map_err(|failure| ChecksumFailure {
        reason: "environment_unavailable",
        message: failure.to_string(),
    })?
    .ok_or_else(|| error("checksum_file requires a selected execution environment".into()))?;
    let path = environment
        .cwd()
        .join(path)
        .map_err(|err| error(err.to_string()))?;
    let sandbox = invocation
        .step_context
        .turn
        .file_system_sandbox_context(None, environment.cwd());
    let filesystem = environment.environment.get_filesystem();
    let cancelled = || ChecksumFailure {
        reason: "cancelled",
        message: "checksum_file cancelled; no digest returned".into(),
    };
    // One backend operation streams one opened object; a sandboxed or remote
    // backend hashes every byte within a single helper launch or request.
    let digest = tokio::select! {
        biased;
        _ = invocation.cancellation_token.cancelled() => return Err(cancelled()),
        result = filesystem.checksum_file(&path, Some(&sandbox)) => result,
    }
    .map_err(|err| ChecksumFailure {
        reason: match err.kind() {
            std::io::ErrorKind::PermissionDenied => "access_denied",
            // Windows sharing/lock violations deny this read even
            // though std::io does not classify them PermissionDenied.
            _ if sharing_violation(&err) => "access_denied",
            std::io::ErrorKind::NotFound => "source_unavailable",
            std::io::ErrorKind::Unsupported => "unsupported_backend",
            _ => "source_unavailable",
        },
        message: format!(
            "unable to checksum {}: {err}; no digest returned",
            path.inferred_native_path_string()
        ),
    })?
    // The backend reports a change or replacement as a typed absence.
    .ok_or_else(|| ChecksumFailure {
        reason: "source_changed_during_check",
        message: "file revision changed during checksum acquisition; no digest returned".into(),
    })?;
    if invocation.cancellation_token.is_cancelled() {
        return Err(cancelled());
    }
    Ok(FileChecksum {
        path: path.inferred_native_path_string(),
        environment_id: environment.environment_id.clone(),
        sha256: digest.sha256,
        bytes_hashed: digest.file_bytes,
        source_identity: digest.revision,
    })
}

impl CoreToolRuntime for ChecksumFileHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}
