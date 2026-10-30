use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::FunctionCallError;
use crate::tool_history::ReadStatusQuery;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::checksum_file::checksum_source;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::wait_for_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub(crate) struct CheckSnapshotFreshnessHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    path: String,
    environment_id: Option<String>,
    source_sha256: Option<String>,
    snapshot_id: Option<String>,
}

impl ToolExecutor<ToolInvocation> for CheckSnapshotFreshnessHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("check_snapshot_freshness")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "check_snapshot_freshness".into(),
            description: "Check current file bytes against an existing snapshot identity without redisplaying or retaining content. Supply exactly one of source_sha256 (the whole raw-file hash from read_file/checksum_file) or snapshot_id (from this thread's read_status for this path/environment). Reuses checksum_file's authorized, revision-guarded streaming SHA-256, including large/binary files. Returns status matches, changed, or unknown with a reason; unavailable sources, denied access, unsupported backends, missing/ambiguous attribution and unstable reads never imply a match or change. HTML snapshot IDs are unknown, as is a mismatching hash that may identify rendered HTML or decoded UTF-16 text rather than raw bytes; use a known raw-file hash instead. Host/Git locators are not current filesystem sources. A match is a point-in-time content observation, not acquisition/semantic-read coverage or a continuing guarantee. Never reruns a producer or refreshes historical snapshots.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("path".into(), JsonSchema::string(Some("Original file path in the selected environment.".into()))),
                ("environment_id".into(), JsonSchema::string(Some("Original execution environment; omit for the selected default.".into()))),
                ("source_sha256".into(), JsonSchema::string(Some("Whole raw-file SHA-256, 64 hex characters; not a rendered-text or decoded-representation hash. Mutually exclusive with snapshot_id.".into()))),
                ("snapshot_id".into(), JsonSchema::string(Some("Exact snapshot_id returned by read_status for this path and environment. Missing history remains unknown.".into()))),
            ]), Some(vec!["path".into()]), Some(false.into())),
            output_schema: Some(json!({
                "type":"object", "additionalProperties":false,
                "properties":{
                    "path":{"type":"string"}, "environment_id":{"type":"string"},
                    "snapshot_id":{"type":"string"}, "source_sha256":{"type":"string"},
                    "status":{"type":"string","enum":["matches","changed","unknown"]},
                    "reason":{"type":"string"},
                    "checked_sha256":{"type":"string"},
                    "bytes_hashed":{"type":"integer","minimum":0}
                },
                "required":["path","status","reason"]
            }).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "check_snapshot_freshness requires function arguments".into(),
                ));
            };
            let mut args: Arguments = parse_arguments(arguments)?;
            if args.path.trim().is_empty()
                || args.source_sha256.is_some() == args.snapshot_id.is_some()
            {
                return Err(FunctionCallError::RespondToModel(
                    "provide a nonempty path and exactly one of source_sha256 or snapshot_id"
                        .into(),
                ));
            }
            for hash in [&mut args.source_sha256, &mut args.snapshot_id]
                .into_iter()
                .flatten()
            {
                if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(FunctionCallError::RespondToModel(
                        "snapshot identity must be a 64-character hexadecimal SHA-256".into(),
                    ));
                }
                hash.make_ascii_lowercase();
            }
            let mut value =
                json!({"path":args.path, "status":"unknown", "reason":"source_unavailable"});
            if let Some(id) = &args.snapshot_id {
                value["snapshot_id"] = json!(id);
            }
            if let Some(hash) = &args.source_sha256 {
                value["source_sha256"] = json!(hash);
            }
            if args.path.starts_with("skill:")
                || args.path.starts_with("context:")
                || args.path.starts_with("git:")
            {
                value["reason"] = json!("unsupported_source");
            } else {
                let observation = check(&invocation, &args, &mut value).await;
                if let Err(reason) = observation {
                    value["reason"] = json!(reason);
                }
            }
            if invocation.cancellation_token.is_cancelled() {
                value["status"] = json!("unknown");
                value["reason"] = json!("cancelled");
                if let Some(fields) = value.as_object_mut() {
                    fields.remove("checked_sha256");
                    fields.remove("bytes_hashed");
                }
            }
            let result = JsonToolOutput::new(value);
            crate::tools::context::ensure_exact_model_delivery(&result, &invocation)?;
            Ok(boxed_tool_output(result))
        })
    }
}

async fn check(
    invocation: &ToolInvocation,
    args: &Arguments,
    value: &mut serde_json::Value,
) -> Result<(), &'static str> {
    let environment = wait_for_tool_environment(
        &invocation.step_context.environments,
        args.environment_id.as_deref(),
        &invocation.cancellation_token,
    )
    .await
    .map_err(|_| "environment_unavailable")?
    .ok_or("environment_unavailable")?;
    value["environment_id"] = json!(environment.environment_id);
    let path = environment
        .cwd()
        .join(&args.path)
        .map_err(|_| "source_unavailable")?;
    let paths = [PathBuf::from(path.inferred_native_path_string())];
    let expected = if let Some(id) = &args.snapshot_id {
        let history = invocation.session.clone_history().await;
        let status = history.read_status_page(
            &paths,
            Some(&environment.environment_id),
            &ReadStatusQuery {
                snapshot_id: Some(id.clone()),
                ..Default::default()
            },
        );
        let snapshot = status["paths"][0]["snapshots"]
            .as_array()
            .filter(|snapshots| snapshots.len() == 1)
            .and_then(|snapshots| snapshots.first())
            .ok_or("snapshot_unavailable")?;
        if snapshot["source_attribution"] != "recorded"
            || snapshot["environment_id"] != environment.environment_id
            || snapshot["canonical_uri"] != path.to_string()
        {
            return Err("source_attribution_unknown");
        }
        // Existing IDs intentionally do not encode an HTML rendering mode.
        // Do not reinterpret a historical rendered hash as raw filesystem bytes.
        if html_path(&args.path) {
            return Err("representation_unavailable");
        }
        snapshot["source_sha256"]
            .as_str()
            .ok_or("snapshot_unavailable")?
            .to_owned()
    } else {
        args.source_sha256.clone().ok_or("snapshot_unavailable")?
    };
    value["source_sha256"] = json!(expected);
    let checksum = checksum_source(invocation, &args.path, Some(&environment.environment_id))
        .await
        .map_err(|failure| failure.reason)?;
    let matches = checksum.sha256 == expected;
    // A supplied hash may identify a non-raw representation: default HTML
    // reads outside the workspace hash rendered text, and decoded UTF-16
    // snapshots (including their selector_errors) hash retained UTF-8. Such a
    // mismatch cannot establish a change.
    if !matches && args.snapshot_id.is_none() {
        if html_path(&args.path) {
            return Err("representation_unavailable");
        }
        let status = invocation.session.clone_history().await.read_status_page(
            &paths,
            Some(&environment.environment_id),
            &ReadStatusQuery {
                canonical_sha256: Some(expected.clone()),
                ..Default::default()
            },
        );
        if status["paths"][0]["snapshots"]
            .as_array()
            .is_some_and(|snapshots| {
                snapshots
                    .iter()
                    .any(|snapshot| snapshot["source_sha256"] != expected)
            })
        {
            return Err("representation_unavailable");
        }
    }
    value["status"] = json!(if matches { "matches" } else { "changed" });
    value["reason"] = json!(if matches {
        "content_hash_matches"
    } else {
        "content_hash_changed"
    });
    value["checked_sha256"] = json!(checksum.sha256);
    value["bytes_hashed"] = json!(checksum.bytes_hashed);
    Ok(())
}

/// Default reads of HTML outside the workspace may identify rendered text.
fn html_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
        })
}

impl CoreToolRuntime for CheckSnapshotFreshnessHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}
