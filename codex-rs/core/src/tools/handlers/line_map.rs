use crate::FunctionCallError;
use crate::tools::command_output_artifact::line_mapping::MAX_REQUEST_LINES;
use crate::tools::command_output_artifact::line_mapping::map_lines;
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

pub(crate) struct LineMapHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    before_artifact_id: String,
    after_artifact_id: String,
    before_sha256: String,
    after_sha256: String,
    start_line: usize,
    end_line: usize,
}

impl ToolExecutor<ToolInvocation> for LineMapHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("line_map")
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
                "total_lines":{"type":"integer","minimum":0}
            },
            "required":["artifact_id","canonical_sha256","canonical_bytes","total_lines"]
        });
        let hash = || {
            JsonSchema {
            pattern: Some("^[0-9a-fA-F]{64}$".into()),
            ..JsonSchema::string(Some("Exact canonical_sha256 of the retained source bytes (decoded UTF-8 for UTF-16 reads), not a preview/excerpt hash.".into()))
        }
        };
        ToolSpec::Function(ResponsesApiTool {
            name: "line_map".into(),
            description: "Secondary, read-only line alignment between two complete authenticated retained UTF-8 artifacts from this thread. Requires exact before/after canonical SHA-256 hashes; rejects mismatches, gaps, incomplete/legacy captures and invalid UTF-8. Request an inclusive 1-based BEFORE range of at most 1000 lines; each source is limited to 1 MiB/20000 lines. LF-only coordinates preserve CRLF, lone CR and final-terminator bytes. A unique exact equal line in the ordered diff is mapped; duplicates or equal text outside that alignment are ambiguous; deleted/replaced text is unmapped. Only mapped entries have after_line. This is textual correspondence, NOT semantic identity or reliable moved-code detection: even mapped text may have moved or changed meaning. Never relocate edits/findings or transfer read coverage using this alone; inspect the target snapshot. Hashes bind only these historical bytes, not current filesystem freshness. Diff work is time-bounded and may conservatively leave more lines unmapped. Does not rerun producers, change files or grant source-inspection credit.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("before_artifact_id".into(), JsonSchema::string(Some("Thread-owned retained before snapshot UUID.".into()))),
                ("after_artifact_id".into(), JsonSchema::string(Some("Thread-owned retained after snapshot UUID.".into()))),
                ("before_sha256".into(), hash()), ("after_sha256".into(), hash()),
                ("start_line".into(), JsonSchema { minimum: Some(1.into()), ..JsonSchema::integer(Some("First before line, inclusive.".into())) }),
                ("end_line".into(), JsonSchema { minimum: Some(1.into()), ..JsonSchema::integer(Some("Last before line, inclusive; at most 1000 lines and no past-EOF clipping.".into())) }),
            ]), Some(vec!["before_artifact_id".into(), "after_artifact_id".into(), "before_sha256".into(), "after_sha256".into(), "start_line".into(), "end_line".into()]), Some(false.into())),
            output_schema: Some(json!({
                "type":"object", "additionalProperties":false,
                "properties": {
                    "before":source, "after":source,
                    "semantics":{"type":"string","enum":["exact_text_alignment_only"]},
                    "limitations":{"type":"array","items":{"type":"string"}},
                    "mappings":{"type":"array","maxItems":MAX_REQUEST_LINES,"items":{
                        "type":"object","additionalProperties":false,
                        "properties": {
                            "before_line":{"type":"integer","minimum":1},
                            "after_line":{"type":["integer","null"],"minimum":1},
                            "status":{"type":"string","enum":["mapped","ambiguous","unmapped"]},
                            "reason":{"type":"string"}
                        }, "required":["before_line","after_line","status","reason"]
                    }}
                }, "required":["before","after","semantics","limitations","mappings"]
            }).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "line_map requires function arguments".into(),
                ));
            };
            let args: Arguments = parse_arguments(arguments)?;
            let mapping = map_lines(
                invocation.step_context.turn.config.codex_home.as_path(),
                &invocation.session.thread_id.to_string(),
                &args.before_artifact_id,
                &args.after_artifact_id,
                &args.before_sha256,
                &args.after_sha256,
                args.start_line,
                args.end_line,
                &invocation.cancellation_token,
            )
            .await
            .map_err(|error| FunctionCallError::RespondToModel(error.for_model()))?;
            let result = JsonToolOutput::new(serde_json::to_value(mapping).map_err(|error| {
                FunctionCallError::RespondToModel(format!("failed to serialize line map: {error}"))
            })?);
            crate::tools::context::ensure_exact_model_delivery(&result, &invocation)?;
            Ok(boxed_tool_output(result))
        })
    }
}

impl CoreToolRuntime for LineMapHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}
