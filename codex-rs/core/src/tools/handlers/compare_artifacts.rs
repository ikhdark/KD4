use crate::FunctionCallError;
use crate::tools::command_output_artifact::comparison::DEFAULT_COMPARISON_BYTES;
use crate::tools::command_output_artifact::comparison::MAX_COMPARISON_BYTES;
use crate::tools::command_output_artifact::comparison::compare_artifacts;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) struct CompareArtifactsHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    left_artifact_id: String,
    right_artifact_id: String,
    max_bytes: Option<usize>,
}

impl ToolExecutor<ToolInvocation> for CompareArtifactsHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("compare_artifacts")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        let source = json!({
            "type":"object", "additionalProperties":false,
            "properties": {
                "artifact_id":{"type":"string"}, "canonical_sha256":{"type":"string"},
                "canonical_bytes":{"type":"integer","minimum":0},
                "retained_bytes":{"type":"integer","minimum":0},
                "capture_complete":{"type":"boolean"},
                "historical_authenticity":{"type":"string","enum":["authenticated","unverified"]},
                "available_prefix_bytes":{"type":"integer","minimum":0}
            },
            "required":["artifact_id","canonical_sha256","canonical_bytes","retained_bytes",
                "capture_complete","historical_authenticity","available_prefix_bytes"]
        });
        ToolSpec::Function(ResponsesApiTool {
            name: "compare_artifacts".into(),
            description: "Compare two retained tool artifacts from this thread without rerunning producers or comparing truncated previews. Exact bytes, not semantic/JSON equivalence: formatting, Unicode and line endings remain significant. Compares the common canonical prefix before any capture gap, up to max_bytes per side (default 64 KiB, maximum 1 MiB). Verdict equal requires authenticated complete captures and full byte coverage; different requires an authenticated mismatch where both producers captured complete output (retention gaps only shorten the compared prefix), or known complete lengths that differ; a mismatch involving a producer-truncated capture, such as head/tail command output with an inline omission marker, stays inconclusive. Otherwise inconclusive. Incomplete captures, unverified legacy/command-log origins and bounded comparisons stay explicit; missing bytes are never additions/deletions. difference_ranges are zero-based half-open mismatching positions, not an edit script, and retain at most 32 ranges; observed_different_bytes counts all mismatches within compared_bytes. difference_ranges_complete concerns only that compared prefix. comparison_complete concerns full capture/byte coverage, not origin authentication or current filesystem freshness. Use read_tool_output on the same artifacts for exact contexts or structured extraction, which this tool does not replace. Artifacts remain thread-private; unavailable or still-writing inputs return errors, not equality.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("left_artifact_id".into(), JsonSchema::string(Some("Retained artifact UUID owned by this thread.".into()))),
                ("right_artifact_id".into(), JsonSchema::string(Some("Second retained artifact UUID owned by this thread.".into()))),
                ("max_bytes".into(), JsonSchema {
                    minimum: Some(1.into()), maximum: Some(MAX_COMPARISON_BYTES.into()),
                    ..JsonSchema::integer(Some("Maximum prefix bytes compared per artifact; default 65536. Not an output/display budget.".into()))
                }),
            ]), Some(vec!["left_artifact_id".into(), "right_artifact_id".into()]), Some(false.into())),
            output_schema: Some(json!({
                "type":"object", "additionalProperties":false,
                "properties": {
                    "verdict":{"type":"string","enum":["equal","different","inconclusive"]},
                    "left":source, "right":source,
                    "compared_bytes":{"type":"integer","minimum":0},
                    "comparison_complete":{"type":"boolean"},
                    "observed_different_bytes":{"type":"integer","minimum":0},
                    "difference_ranges":{"type":"array","maxItems":32,"items":{
                        "type":"object","additionalProperties":false,
                        "properties":{"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":0}},
                        "required":["start","end"]
                    }},
                    "difference_ranges_complete":{"type":"boolean"},
                    "length_difference":{"type":"boolean"},
                    "limitations":{"type":"array","items":{"type":"string","enum":["incomplete_capture","unverified_origin","comparison_limit"]}}
                },
                "required":["verdict","left","right","compared_bytes","comparison_complete",
                    "observed_different_bytes","difference_ranges","difference_ranges_complete","length_difference","limitations"]
            }).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "compare_artifacts requires function arguments".into(),
                ));
            };
            let args: Arguments = parse_arguments(arguments)?;
            let max_bytes = args.max_bytes.unwrap_or(DEFAULT_COMPARISON_BYTES);
            if !(1..=MAX_COMPARISON_BYTES).contains(&max_bytes) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "max_bytes must be between 1 and {MAX_COMPARISON_BYTES}"
                )));
            }
            let comparison = compare_artifacts(
                invocation.step_context.turn.config.codex_home.as_path(),
                &invocation.session.thread_id.to_string(),
                &args.left_artifact_id,
                &args.right_artifact_id,
                max_bytes,
                &invocation.cancellation_token,
            )
            .await
            .map_err(|error| FunctionCallError::RespondToModel(error.for_model()))?;
            let result =
                JsonToolOutput::new(serde_json::to_value(comparison).map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to serialize artifact comparison: {error}"
                    ))
                })?);
            crate::tools::context::ensure_exact_model_delivery(&result, &invocation)?;
            Ok(boxed_tool_output(result))
        })
    }
}

impl CoreToolRuntime for CompareArtifactsHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}
