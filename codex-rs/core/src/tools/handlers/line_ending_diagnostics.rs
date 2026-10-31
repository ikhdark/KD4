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
use crate::tools::handlers::checksum_file::stream_source;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub(crate) struct LineEndingDiagnosticsHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    path: String,
    environment_id: Option<String>,
}

impl ToolExecutor<ToolInvocation> for LineEndingDiagnosticsHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("line_ending_diagnostics")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "line_ending_diagnostics".into(),
            description: "Stream a regular file's original bytes to diagnose encoding/BOM, exact LF/CRLF/lone-CR counts, mixed endings and final newline, without collecting, retaining or delivering file contents. Uses checksum_file's sandboxed, cancellable, revision-guarded pass for any file size; returns raw SHA-256, bytes_scanned and source_identity. encoding_detection:bom confirms a declared encoding (not payload validity); inferred_utf8 means BOM-less bytes merely pass strict UTF-8 validation, not a confirmed author encoding; unknown means no supported encoding is established. decoding_error reports the first failure's attempted encoding, kind, zero-based original-file byte_offset (including BOM) and byte_length; decoding stops there without replacement or normalization. Subsequent errors are not enumerated. Recognizes UTF-8 with/without BOM and BOM-marked UTF-16LE/BE and UTF-32LE/BE; validates strictly across chunks, excludes the initial BOM from text and never guesses unmarked legacy encodings. Without a BOM, invalid UTF-8 is encoding unknown; malformed/unknown text has valid_encoding:false and null newline/count fields, not partial or byte-based claims. final_newline is lf, crlf, cr or none (empty/BOM-only also none); line_count counts terminators plus an unterminated last line, with zero for empty text. Only CR and LF are terminators, not Unicode line separators. Valid UTF-8 is not a binary-file classification. Changed sources and unsupported backends fail, with no host/per-page fallback. This is a point-in-time diagnostic, not a snapshot or source-inspection credit.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("path".into(), JsonSchema::string(Some("Regular file path, absolute or relative to the selected environment cwd; follows file symlinks.".into()))),
                ("environment_id".into(), JsonSchema::string(Some("Omit for the selected default execution environment.".into()))),
            ]), Some(vec!["path".into()]), Some(false.into())),
            output_schema: Some(json!({
                "type":"object", "additionalProperties":false,
                "properties":{
                    "path":{"type":"string"}, "environment_id":{"type":"string"},
                    "sha256":{"type":"string","pattern":"^[0-9a-f]{64}$"},
                    "bytes_scanned":{"type":"integer","minimum":0},
                    "source_identity":{"type":"string"},
                    "diagnostics":{
                        "type":"object","additionalProperties":false,
                        "properties":{
                            "encoding":{"enum":["utf-8","utf-16le","utf-16be","utf-32le","utf-32be","unknown"]},
                            "encoding_detection":{"enum":["bom","inferred_utf8","unknown"]},
                            "bom":{"enum":["none","utf-8","utf-16le","utf-16be","utf-32le","utf-32be"]},
                            "bom_bytes":{"type":"integer","enum":[0,2,3,4]},
                            "valid_encoding":{"type":"boolean"},
                            "decoding_error":{"type":["object","null"],"additionalProperties":false,
                                "properties":{
                                    "encoding":{"enum":["utf-8","utf-16le","utf-16be","utf-32le","utf-32be"]},
                                    "byte_offset":{"type":"integer","minimum":0},
                                    "byte_length":{"type":"integer","minimum":1,"maximum":4},
                                    "kind":{"enum":["invalid_utf8_sequence","incomplete_utf8_sequence","unpaired_utf16_surrogate","incomplete_utf16_code_unit","invalid_utf32_scalar","incomplete_utf32_code_unit"]}
                                },
                                "required":["encoding","byte_offset","byte_length","kind"]},
                            "line_endings":{"type":["object","null"],"additionalProperties":false,
                                "properties":{"lf":{"type":"integer","minimum":0},"crlf":{"type":"integer","minimum":0},"cr":{"type":"integer","minimum":0}},
                                "required":["lf","crlf","cr"]},
                            "mixed_endings":{"type":["boolean","null"]},
                            "final_newline":{"enum":["lf","crlf","cr","none",null]},
                            "line_count":{"type":["integer","null"],"minimum":0}
                        },
                        "required":["encoding","encoding_detection","bom","bom_bytes","valid_encoding","decoding_error","line_endings","mixed_endings","final_newline","line_count"]
                    }
                },
                "required":["path","environment_id","sha256","bytes_scanned","source_identity","diagnostics"]
            }).into()),
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "line_ending_diagnostics requires function arguments".into(),
                ));
            };
            let args: Arguments = parse_arguments(arguments)?;
            let source = stream_source(
                &invocation,
                &args.path,
                args.environment_id.as_deref(),
                true,
            )
            .await
            .map_err(|error| FunctionCallError::RespondToModel(error.message))?;
            let diagnostics = source.text_diagnostics.ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "executor does not support text diagnostics; no diagnostics returned".into(),
                )
            })?;
            let result = JsonToolOutput::new(json!({
                "path":source.path, "environment_id":source.environment_id,
                "sha256":source.sha256, "bytes_scanned":source.bytes_hashed,
                "source_identity":source.source_identity, "diagnostics":diagnostics,
            }));
            crate::tools::context::ensure_exact_model_delivery(&result, &invocation)?;
            Ok(boxed_tool_output(result))
        })
    }
}

impl CoreToolRuntime for LineEndingDiagnosticsHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }
}
