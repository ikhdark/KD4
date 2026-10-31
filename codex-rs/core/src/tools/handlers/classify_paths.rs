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

pub(crate) struct ClassifyPathsHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    paths: Vec<String>,
    repository: Option<String>,
    environment_id: Option<String>,
}

impl ToolExecutor<ToolInvocation> for ClassifyPathsHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("classify_paths")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "classify_paths".into(),
            description: "Classify a batch of literal paths in one local Git repository. Returns independent tracked, ignored, generated and build_output labels: true, false, or null with unknown_reason, each with evidence. Labels are not mutually exclusive: tracked paths are still evaluated with check-ignore --no-index (including global/info excludes and winning negations). tracked means exact live-index membership, not a directory's descendants; tracked_descendants reports those separately. generated uses effective linguist-generated/generated Git attributes and recognized generated-file comment headers in at most the first 8192 bytes/16 lines. build_output uses an explicit build-output Git attribute or a Cargo-created CACHEDIR.TAG in an ancestor within this repository. Attribute set/true means true; unset/false means false; unspecified or other values are not proof. Conflicting declarations yield unknown. No declaration, absent marker, extension, directory name, ignored status or tracked status proves another label false. Directories and missing files can still have Git evidence; headers are not followed through symlinks/junctions. Per-mechanism failures remain errors/unknown rather than erasing independent evidence. Provenance declarations are not generator execution proof; point-in-time observations are not coherent snapshots or source-review coverage. No fetches, filters, hooks or repository writes. Remote/restricted environments are unsupported; select a local environment with full disk read access. Outputs must fit the delivery budget; narrow the path batch if necessary.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("paths".into(), JsonSchema { min_items: Some(1), max_items: Some(64), ..JsonSchema::array(JsonSchema::string(None), Some("1-64 literal paths relative to the discovered repository root, not cwd. No absolute, empty, dot, parent or trailing-separator components; at most 4096 UTF-8 bytes/64 components per path. Wildcards are literal.".into())) }),
                ("repository".into(), JsonSchema::string(Some("Directory in the repository to inspect, relative to the selected environment cwd or absolute. Defaults to that cwd; Git discovers its worktree root.".into()))),
                ("environment_id".into(), JsonSchema::string(Some("Selected local environment; omit for the primary environment.".into()))),
            ]), Some(vec!["paths".into()]), Some(false.into())),
            output_schema: Some(output_schema().into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "classify_paths requires function arguments".into(),
                ));
            };
            let args: Arguments = parse_arguments(arguments)?;
            let environment = wait_for_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
                &invocation.cancellation_token,
            )
            .await?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "classify_paths requires a selected local environment".into(),
                )
            })?;
            if environment.environment.is_remote()
                || !invocation
                    .step_context
                    .turn
                    .file_system_sandbox_policy()
                    .has_full_disk_read_access()
            {
                return Err(FunctionCallError::RespondToModel(
                    "classify_paths requires a local environment with full disk read access".into(),
                ));
            }
            crate::tools::parallel::wait_for_workspace_baseline().await;
            let cwd = environment
                .cwd()
                .join(args.repository.as_deref().unwrap_or("."))
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
                .to_abs_path()
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
                .to_path_buf();
            let result = tokio::select! {
                _ = invocation.cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("Path classification cancelled; no repository writes were requested".into())),
                result = codex_git_utils::classify_paths(cwd, args.paths) => result.map_err(FunctionCallError::RespondToModel)?,
            };
            let mut value = serde_json::to_value(result)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            value["environment_id"] = json!(environment.environment_id);
            let output = JsonToolOutput::new(value);
            crate::tools::context::ensure_exact_model_delivery(&output, &invocation).map_err(|_| FunctionCallError::RespondToModel("Path classification exceeds the model delivery budget; nothing was delivered. Select fewer paths.".into()))?;
            Ok(boxed_tool_output(output))
        })
    }
}

impl CoreToolRuntime for ClassifyPathsHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}

fn output_schema() -> serde_json::Value {
    let string = json!({"type":"string"});
    let label = json!({"type":"object","additionalProperties":false,
        "properties":{"value":{"type":["boolean","null"]},"unknown_reason":{"type":["string","null"]},
            "evidence":{"type":"array","items":{"type":"object","additionalProperties":false,
                "properties":{"source":string,"detail":string},"required":["source","detail"]}}},
        "required":["value","evidence","unknown_reason"]});
    json!({"type":"object","additionalProperties":false,
        "properties":{"repository_root":string,"environment_id":string,"labels_independent":{"type":"boolean"},
            "coherent_snapshot_verified":{"type":"boolean"},"paths":{"type":"array","items":{"type":"object","additionalProperties":false,
                "properties":{"path":string,"tracked":label,"ignored":label,"generated":label,"build_output":label,
                    "tracked_descendants":{"type":["integer","null"],"minimum":0},"errors":{"type":"array","items":string}},
                "required":["path","tracked","ignored","generated","build_output","tracked_descendants","errors"]}}},
        "required":["repository_root","environment_id","labels_independent","coherent_snapshot_verified","paths"]})
}
