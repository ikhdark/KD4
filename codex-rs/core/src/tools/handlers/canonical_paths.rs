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
use crate::tools::handlers::wait_for_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub(crate) struct CanonicalPathsHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    paths: Vec<String>,
    environment_id: Option<String>,
}

impl ToolExecutor<ToolInvocation> for CanonicalPathsHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("canonical_paths")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "canonical_paths".into(),
            description: "Inspect existing file or directory paths by resolving symlinks and Windows junctions through the selected environment's sandboxed filesystem, without a shell or content reads. Accepts 1-32 native paths (1-4096 UTF-8 bytes each). Absolute paths stay absolute; relative paths and dot/parent segments are resolved lexically against the selected environment cwd before filesystem resolution. Uses one bounded filesystem batch (one sandbox helper or remote RPC). Returns one result per input in input order, including duplicates; missing, inaccessible, unsupported and other failures have no canonical_path and do not discard successful siblings. Does not invent paths for missing targets or fall back to the host. inspection_only is always true: each resolution is a separate point-in-time name observation, not an atomic batch, resource identity, confinement proof, permission grant or guarantee for later access. Resolving before access does not prevent replacement races; security-sensitive operations must validate the opened resource in their owning operation. No content, snapshot, read-history or source-inspection credit is delivered.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("paths".into(), JsonSchema::array(
                    JsonSchema::string(Some("Existing native file or directory path, absolute or cwd-relative. No URI, shell expansion or wildcard interpretation.".into())),
                    Some("1-32 paths in result order; each 1-4096 UTF-8 bytes without NUL.".into()),
                )),
                ("environment_id".into(), JsonSchema::string(Some("Omit for the selected primary execution environment; never falls back to the host.".into()))),
            ]), Some(vec!["paths".into()]), Some(false.into())),
            output_schema: Some(json!({
                "type":"object", "additionalProperties":false,
                "properties":{
                    "environment_id":{"type":"string"}, "cwd":{"type":"string"},
                    "inspection_only":{"const":true},
                    "results":{"type":"array","minItems":1,"maxItems":32,"items":{
                        "type":"object","additionalProperties":false,
                        "properties":{
                            "path":{"type":"string"}, "absolute_path":{"type":["string","null"]},
                            "canonical_path":{"type":["string","null"]},
                            "status":{"enum":["resolved","invalid_path","not_found","access_denied","unsupported","error"]},
                            "error":{"type":["string","null"]}
                        },
                        "required":["path","absolute_path","canonical_path","status","error"]
                    }}
                },
                "required":["environment_id","cwd","inspection_only","results"]
            }).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let error = |message: String| FunctionCallError::RespondToModel(message);
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(error("canonical_paths requires function arguments".into()));
            };
            let args: Arguments = parse_arguments(arguments)?;
            if args.paths.is_empty()
                || args.paths.len() > 32
                || args
                    .paths
                    .iter()
                    .any(|path| path.is_empty() || path.len() > 4096 || path.contains('\0'))
            {
                return Err(error(
                    "canonical_paths requires 1-32 paths, each 1-4096 UTF-8 bytes without NUL"
                        .into(),
                ));
            }
            let environment = wait_for_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
                &invocation.cancellation_token,
            )
            .await?
            .ok_or_else(|| {
                error("canonical_paths requires a selected execution environment".into())
            })?;
            let sandbox = invocation
                .step_context
                .turn
                .file_system_sandbox_context(None, environment.cwd());
            let filesystem = environment.environment.get_filesystem();
            let cancelled =
                || error("canonical_paths cancelled; no inspection results returned".into());
            use codex_file_system::FileSystemLookup;
            use codex_file_system::FileSystemLookupResult as R;
            let paths = args.paths.iter().map(|input| environment.cwd().join(input)).collect::<Vec<_>>();
            let requests = paths.iter().filter_map(|path| path.as_ref().ok())
                .map(|path| FileSystemLookup::Canonicalize { path: path.clone() }).collect::<Vec<_>>();
            let observed = if requests.is_empty() { Vec::new() } else {
                match tokio::select! {
                    biased;
                    _ = invocation.cancellation_token.cancelled() => return Err(cancelled()),
                    result = filesystem.lookup_batch(&requests, Some(&sandbox)) => result,
                } {
                    Ok(results) if results.len() == requests.len() => results,
                    Ok(_) => return Err(error("path lookup batch result count mismatch".into())),
                    Err(err) => vec![R::Unknown { error: err.to_string() }; requests.len()],
                }
            };
            let mut observed = observed.into_iter();
            let mut results = Vec::with_capacity(args.paths.len());
            for (input, path) in args.paths.into_iter().zip(paths) {
                let path = match path {
                    Ok(path) => path,
                    Err(err) => {
                        results.push(json!({"path":input,"absolute_path":null,"canonical_path":null,
                            "status":"invalid_path","error":err.to_string()}));
                        continue;
                    }
                };
                let (canonical_path, status, message) = match observed.next().expect("validated batch count") {
                    R::Resolved { path } => (Some(path.inferred_native_path_string()), "resolved", None),
                    R::Missing => (None, "not_found", Some("path not found".to_string())),
                    R::Inaccessible { error } => (None, "access_denied", Some(error)),
                    R::Unsupported { error } => (None, "unsupported", Some(error)),
                    R::Invalid { error } => (None, "invalid_path", Some(error)),
                    R::Unknown { error } => (None, "error", Some(error)),
                };
                results.push(json!({"path":input,"absolute_path":path.inferred_native_path_string(),
                    "canonical_path":canonical_path,"status":status,"error":message}));
            }
            if invocation.cancellation_token.is_cancelled() {
                return Err(cancelled());
            }
            let output = JsonToolOutput::new(json!({
                "environment_id":environment.environment_id,
                "cwd":environment.cwd().inferred_native_path_string(),
                "inspection_only":true,"results":results,
            }));
            crate::tools::context::ensure_exact_model_delivery(&output, &invocation)?;
            Ok(boxed_tool_output(output))
        })
    }
}

impl CoreToolRuntime for CanonicalPathsHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}
