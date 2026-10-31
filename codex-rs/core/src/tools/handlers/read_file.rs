use std::collections::BTreeMap;
use std::sync::Arc;

use codex_tools::CanonicalToolResult;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::FunctionCallError;
use crate::tools::command_output_artifact::CanonicalOutputArtifact;
use crate::tools::command_output_artifact::RECOVERY_AGGREGATE_TOKEN_CEILING;
use crate::tools::command_output_artifact::ReadToolOutputError;
use crate::tools::command_output_artifact::ReadToolOutputResult;
use crate::tools::command_output_artifact::ToolOutputSelector;
use crate::tools::command_output_artifact::raw_json_pointer_index;
use crate::tools::command_output_artifact::select_file_snapshot_for_script;
use crate::tools::command_output_artifact::select_file_snapshot_with_ceiling;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_SELECTORS;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES;
use crate::tools::handlers::read_tool_output_spec::file_selector_schema;
use crate::tools::handlers::read_tool_output_spec::read_tool_output_output_schema;
use crate::tools::handlers::wait_for_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

const MAX_FILE_MIB: usize = 8;
const MAX_FILE_BYTES: usize = MAX_FILE_MIB * 1024 * 1024;
// Path-only reads are previews, not an implicit request to fill the context.
// Sized so typical source and docs files arrive whole: six 2026-10-28
// sessions cut 40 previews at 2,000 tokens (29 were files under 16 KiB) and
// each recovery read cost a model round trip. Still bounded by the cell.
const DEFAULT_FILE_READ_TOKEN_CEILING: usize = 8_000;
// The former fixed preview: small cell budgets never shrink a script preview.
const MIN_FILE_READ_PREVIEW_TOKENS: usize = 2_000;



#[path = "code_sections.rs"]
pub(super) mod code_sections;
pub(crate) use code_sections::existing_tests_changed;
pub(crate) use code_sections::compilation_module_paths;
pub(crate) use code_sections::syntax_regression;

#[path = "markdown_sections.rs"]
mod markdown_sections;

#[path = "read_file_structure.rs"]
mod structure;
use structure::FileSelector;

#[path = "read_file_ranges.rs"]
mod ranges;

#[path = "read_file_encoding.rs"]
mod encoding;

pub(crate) struct ReadFileHandler;
pub(crate) struct ReadStatusHandler;

impl ToolExecutor<ToolInvocation> for ReadStatusHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("read_status")
    }

    fn spec(&self) -> ToolSpec {
        let mut paths = JsonSchema::array(
            JsonSchema::string(None),
            Some("1–32 file paths; no source files are read.".into()),
        );
        paths.min_items = Some(1);
        paths.max_items = Some(32);
        ToolSpec::Function(ResponsesApiTool {
            name: "read_status".into(),
            description: "Report obtained snapshot byte coverage from read/recovery history, newest observations first (legacy recency may be unknown). Select query.source_sha256 for an exact historical hash; use returned offsets for more snapshots, ranges, or artifacts. No source reread, freshness check, or semantic-read claim. Missing history is unknown; different hashes are never merged.".into(),
            strict: false, defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("paths".into(), paths),
                ("environment_id".into(), JsonSchema::string(Some("Environment id; omit to use the primary environment.".into()))),
                ("query".into(), JsonSchema::object(BTreeMap::from([
                    ("source_sha256".into(), JsonSchema::string(Some("Select the requested source snapshot hash.".into()))),
                    ("snapshot_id".into(), JsonSchema::string(Some("Select an exact returned snapshot identity, including source attribution.".into()))),
                    ("snapshot_offset".into(), JsonSchema { minimum: Some(0.into()), maximum: Some(usize::MAX.into()), ..JsonSchema::integer(Some("Continue at next_snapshot_offset; default 0.".into())) }),
                    ("range_offset".into(), JsonSchema { minimum: Some(0.into()), maximum: Some(usize::MAX.into()), ..JsonSchema::integer(Some("Continue at next_range_offset for an exact snapshot; default 0.".into())) }),
                    ("artifact_offset".into(), JsonSchema { minimum: Some(0.into()), maximum: Some(usize::MAX.into()), ..JsonSchema::integer(Some("Continue at next_artifact_offset for an exact snapshot; default 0.".into())) }),
                ]), None, Some(false.into()))),
            ]), Some(vec!["paths".into()]), Some(false.into())),
            output_schema: Some(json!({"type":"object", "properties":{
                "paths":{"type":"array", "items":{"type":"object"}}, "scope":{"type":"string"}},
                "required":["paths","scope"], "additionalProperties":false}).into()),
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                paths: Vec<String>,
                environment_id: Option<String>,
                #[serde(default)]
                query: crate::tool_history::ReadStatusQuery,
            }
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "read_status requires function arguments".into(),
                ));
            };
            let args: Args = parse_arguments(arguments)?;
            if args.paths.is_empty()
                || args.paths.len() > 32
                || args.paths.iter().any(|path| path.trim().is_empty())
            {
                return Err(FunctionCallError::RespondToModel(
                    "read_status requires 1–32 nonempty paths".into(),
                ));
            }
            let environment = wait_for_tool_environment(
                &invocation.step_context.environments,
                args.environment_id.as_deref(),
                &invocation.cancellation_token,
            )
            .await?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "read_status requires a selected execution environment.".into(),
                )
            })?;
            let paths = args
                .paths
                .iter()
                .map(|path| {
                    environment
                        .cwd()
                        .join(path)
                        .map(|path| std::path::PathBuf::from(path.inferred_native_path_string()))
                        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let history = invocation.session.clone_history().await;
            let result =
                history.read_status_page(&paths, Some(&environment.environment_id), &args.query);
            Ok(boxed_tool_output(JsonToolOutput::new(result)))
        })
    }
}

impl CoreToolRuntime for ReadStatusHandler {}

#[derive(Deserialize)]
#[serde(try_from = "RawReadFileArgs")]
struct ReadFileArgs {
    path: String,
    environment_id: Option<String>,
    revision: Option<String>,
    if_source_hash: Option<String>,
    if_file_revision: Option<String>,
    encoding: Option<ranges::ByteEncoding>,
    selectors: Option<Vec<FileSelector>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReadFileArgs {
    #[serde(alias = "file_path")]
    path: String,
    environment_id: Option<String>,
    revision: Option<String>,
    if_source_hash: Option<String>,
    if_file_revision: Option<String>,
    encoding: Option<ranges::ByteEncoding>,
    selectors: Option<Vec<FileSelector>>,
    offset: Option<usize>,
    limit: Option<usize>,
    #[serde(default)]
    force_fresh: bool,
}

impl TryFrom<RawReadFileArgs> for ReadFileArgs {
    type Error = String;

    fn try_from(raw: RawReadFileArgs) -> Result<Self, Self::Error> {
        // Dispatch consumes this flag before replay lookup/storage; the handler
        // always reads the selected filesystem when it is actually invoked.
        let _ = raw.force_fresh;
        if let Some(revision) = &raw.revision {
            if revision.trim().is_empty() || revision.contains(['\0', '\n', '\r']) {
                return Err("revision must be a nonempty Git commit revision".into());
            }
            if raw.path.starts_with("skill:") || raw.path.starts_with("context:") {
                return Err("revision is only supported for repository files".into());
            }
        }
        let if_source_hash = raw.if_source_hash.map(|hash| {
            if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("if_source_hash must be a 64-character hexadecimal SHA-256".to_string());
            }
            Ok(hash.to_ascii_lowercase())
        }).transpose()?;
        let legacy_range = raw.offset.is_some() || raw.limit.is_some();
        if legacy_range && raw.selectors.is_some() {
            return Err("use selectors or offset/limit, not both".into());
        }
        let selectors = if legacy_range {
            let start = raw.offset.unwrap_or(1);
            let limit = raw.limit.unwrap_or(2000);
            if start == 0 || limit == 0 {
                return Err("offset and limit must be positive".into());
            }
            let end = start
                .checked_add(limit - 1)
                .ok_or("line range is too large")?;
            Some(vec![FileSelector::Exact(ToolOutputSelector::Lines {
                start,
                end,
            })])
        } else {
            raw.selectors
        };
        if raw.encoding.is_some()
            && (raw.revision.is_some() || if_source_hash.is_some()
                || raw.path.starts_with("skill:") || raw.path.starts_with("context:")
                || !selectors.as_ref().is_some_and(|selectors| !selectors.is_empty()
                    && selectors.iter().all(|selector| matches!(selector,
                        FileSelector::Exact(ToolOutputSelector::Bytes { .. })))))
        {
            return Err("encoding requires filesystem bytes-only selectors and cannot be combined with revision or if_source_hash".into());
        }
        if let Some(revision) = &raw.if_file_revision {
            if revision.is_empty() || revision.len() > 512 || !revision.is_ascii() {
                return Err("if_file_revision must be an opaque ASCII revision of 1-512 bytes".into());
            }
            if raw.revision.is_some() || if_source_hash.is_some() || raw.path.starts_with("skill:") || raw.path.starts_with("context:")
                || !selectors.as_ref().is_some_and(|selectors| !selectors.is_empty() && selectors.iter()
                    .all(|selector| matches!(selector, FileSelector::Exact(ToolOutputSelector::Bytes { .. }))))
            {
                return Err("if_file_revision requires filesystem byte selectors and cannot be combined with revision or if_source_hash".into());
            }
        }
        Ok(Self {
            path: raw.path,
            environment_id: raw.environment_id,
            revision: raw.revision,
            if_source_hash,
            if_file_revision: raw.if_file_revision,
            encoding: raw.encoding,
            selectors,
        })
    }
}

pub(crate) fn validate_read_file_arguments(arguments: &str) -> Result<(), String> {
    parse_read_file_arguments(arguments)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// The handler's parser, shared with code-mode preflight. A rejected selector
/// also names the accepted forms; a bare serde mismatch was retried blind.
fn parse_read_file_arguments(arguments: &str) -> Result<ReadFileArgs, FunctionCallError> {
    parse_arguments(arguments).map_err(|error| match error {
        FunctionCallError::Structured(mut failure) => {
            if let Some(forms) = accepted_selector_forms(arguments) {
                failure.message = format!("{}. {forms}", failure.message);
            }
            FunctionCallError::Structured(failure)
        }
        error => error,
    })
}

/// Selector forms derived from the advertised schema when `selectors` holds a
/// value the handler's decoder rejects: that kind's own form, or every form for
/// an unknown kind or shape. `None` leaves other failures their own message.
fn accepted_selector_forms(arguments: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    let selectors = value
        .get("selectors")
        .filter(|selectors| !selectors.is_null())?;
    let rejected_kind = match selectors.as_array() {
        Some(selectors) => selectors
            .iter()
            .find(|selector| FileSelector::deserialize(*selector).is_err())?
            .get("kind")
            .and_then(serde_json::Value::as_str),
        None => None,
    };
    let schema = file_selector_schema();
    let mut forms = Vec::new();
    for variant in schema.one_of.iter().flatten() {
        let Some(properties) = &variant.properties else {
            continue;
        };
        let Some(kind) = properties
            .get("kind")
            .and_then(|kind| kind.enum_values.as_ref())
            .and_then(|values| values.first())
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        // Typed `required` keeps declaration order; encoding sorts it.
        let encoded = serde_json::to_value(variant).unwrap_or_default();
        let required = variant.required.as_deref().unwrap_or_default();
        let mut form = format!("{{\"kind\":\"{kind}\"");
        for field in required.iter().filter(|field| *field != "kind") {
            let field_type = encoded["properties"][field]["type"]
                .as_str()
                .unwrap_or("value");
            form.push_str(&format!(",\"{field}\":<{field_type}>"));
        }
        form.push('}');
        let optional = properties
            .keys()
            .filter(|field| *field != "kind" && !required.contains(*field))
            .map(String::as_str)
            .collect::<Vec<_>>();
        if !optional.is_empty() {
            form.push_str(&format!(" (optional {})", optional.join(", ")));
        }
        if rejected_kind == Some(kind) {
            return Some(format!("Accepted form: {form}"));
        }
        forms.push(form);
    }
    Some(format!("selectors is an array of {}", forms.join(" | ")))
}

/// Use the handler's parser for replay identity, not a second interpretation of
/// legacy aliases/defaults. Path spelling and selector order remain exact;
/// filesystem identity and authorization still belong to the replay guard.
pub(crate) fn canonical_read_file_arguments(arguments: &str) -> Option<serde_json::Value> {
    let args = parse_arguments::<ReadFileArgs>(arguments).ok()?;
    // Mutable revisions must resolve again; workspace replay guards cannot
    // establish Git ref/object identity (nor may they credit current source).
    if args.path == crate::turn_diff_tracker::TURN_DIFF_LOCATOR || args.revision.is_some() || args.if_file_revision.is_some() || args.encoding.is_some() {
        return None;
    }
    let mut identity = json!({
        "path": args.path,
        "environment_id": args.environment_id,
        "selectors": args.selectors,
    });
    if let Some(hash) = args.if_source_hash {
        identity["if_source_hash"] = json!(hash);
    }
    Some(identity)
}


impl ToolExecutor<ToolInvocation> for ReadFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("read_file")
    }

    fn spec(&self) -> ToolSpec {
        let mut selectors = JsonSchema::array(file_selector_schema(), Some("Omit to read the first page immediately. Batch known ranges here. Search results include text in results[].value.hydrated_ranges; enclosing:true also returns enclosing Rust/Python items as line selections. json_pointer selects a complete JSON value; structured projects/filters JSON, JSONL, CSV or TSV rows with explicit malformed_rows policy. {\"kind\":\"section\",\"id\":\"outline\"} lists items; symbol and enclosing share its parser; qualified names (Type::method or Class.method) disambiguate items. Oversized selections return child_selectors.".to_string()));
        selectors.min_items = Some(1);
        selectors.max_items = Some(READ_TOOL_OUTPUT_MAX_SELECTORS as u64);
        let mut output = read_tool_output_output_schema(file_selector_schema());
        output["properties"]["path"] = json!({"type": "string"});
        output["properties"]["environment_id"] = json!({"type":"string", "description":"Actual resolved execution environment, or host-skills/host-context for host-owned sources."});
        output["properties"]["canonical_uri"] = json!({"type":"string", "description":"Canonical path URI in the resolved environment, or the host-owned locator."});
        output["properties"]["git_source"] = json!({"type":"object", "properties":{
            "repository_root":{"type":"string"}, "path":{"type":"string"},
            "commit_oid":{"type":"string"}, "blob_oid":{"type":"string"}},
            "required":["repository_root","path","commit_oid","blob_oid"], "additionalProperties":false});
        output["properties"]["total_lines"] = json!({"type": "integer", "minimum": 0});
        output["properties"]["source_encoding"] = json!({"type":"string", "enum":["utf-16le","utf-16be"], "description":"Original encoding of a decoded snapshot; omitted for UTF-8. UTF-16 requires a BOM, which is omitted from the decoded snapshot; decoding is strict, not lossy."});
        output["properties"]["source_bytes"] = json!({"type":"integer", "minimum":0, "description":"Original file byte count, including the UTF-16 BOM; canonical_bytes counts the decoded UTF-8 snapshot instead."});
        output["properties"]["selector_encoding"] = json!({"const":"utf-8"});
        output["properties"]["selector_coordinate_space"] = json!({"type":"string", "enum":["decoded_utf8","source_bytes"], "description":"Omitted for UTF-8 snapshots, whose selectors address canonical bytes. Decoded snapshot selectors, byte/search offsets, canonical_range and recovery address retained UTF-8 bytes, never original UTF-16 bytes or code units. Lines/structure refer to this decoded text. Live byte_ranges use source_bytes: original bytes, including with encoding:hex|base64."});
        output["properties"]["read_mode"] = json!({"enum":["byte_ranges","unsupported_text"]});
        output["properties"]["classification"] = json!({"type":"object", "properties":{
            "kind":{"enum":["unsupported_text","likely_binary"]},
            "confidence":{"enum":["low","medium","high"]},
            "reason":{"type":"string"}, "encoding":{"type":"string"}},
            "required":["kind","confidence","reason"], "additionalProperties":false,
            "description":"Unsupported display text, not a definitive binary verdict. Confidence qualifies the stated classification; invalid UTF-8 alone has low confidence."});
        output["properties"]["binary_recovery"] = json!({"type":"array", "minItems":2, "maxItems":2,
            "items":{"type":"object", "properties":{"tool":{"const":"read_file"},
                "arguments":{"type":"object", "properties":{
                    "path":{"type":"string"}, "environment_id":{"type":"string"}, "if_file_revision":{"type":"string"},
                    "encoding":{"enum":["hex","base64"]},
                    "selectors":{"type":"array", "minItems":1, "maxItems":1, "items":file_selector_schema()}},
                    "required":["path","environment_id","encoding","selectors"], "additionalProperties":false}},
                "required":["tool","arguments"], "additionalProperties":false},
            "description":"Bounded original-byte preview recipes (at most 256 raw bytes); reread the current file, not a retained snapshot. Per-result byte-page recipes carry the observed file_revision; snapshot refusal recipes have no live-range revision yet."});
        output["properties"]["encoding"] = json!({"type":"string", "enum":["hex","base64"]});
        let classification_schema = output["properties"]["classification"].clone();
        let binary_recovery_schema = output["properties"]["binary_recovery"].clone();
        if let Some(variants) = output["$defs"]["result"]["oneOf"].as_array_mut() {
            for variant in variants {
                variant["properties"]["classification"] = classification_schema.clone();
                variant["properties"]["binary_recovery"] = binary_recovery_schema.clone();
                variant["properties"]["data_hex"] = json!({"type":"string", "description":"Lowercase hexadecimal of exactly canonical_range, two digits per source byte. Present only for encoding:hex; no text decoding."});
                variant["properties"]["selector_index"] = json!({"type":"integer", "minimum":0, "description":"Original selector index for live byte pages. I/O unions share one guarded revision; each result retains its own encoding boundaries."});
            }
        }
        output["properties"]["file_bytes"] = json!({"type":"integer", "minimum":0});
        output["properties"]["classification_scope"] = json!({"enum":["complete_file","prefix"], "description":"Bytes inspected for a non-display classification, not a format guarantee. Per-result live-page classification covers only canonical_range."});
        output["properties"]["inspected_bytes"] = json!({"type":"integer", "minimum":0});
        output["properties"]["file_revision"] = json!({"type":"string", "description":"Opaque opened-file identity/size/timestamps/change-time revision, not a content hash or retained snapshot. Pass as if_file_revision on later byte pages."});
        output["properties"]["range_recovery"] = json!({"type":"object", "properties":{
            "tool":{"const":"read_file"}, "arguments":{"type":"object", "properties":{
                "path":{"type":"string"}, "environment_id":{"type":"string"}, "if_file_revision":{"type":"string"},
                "encoding":{"type":"string", "enum":["hex","base64"]},
                "selectors":{"type":"array", "minItems":1, "maxItems":64, "items":file_selector_schema()}},
                "required":["path","environment_id","if_file_revision","selectors"], "additionalProperties":false}},
            "required":["tool","arguments"], "additionalProperties":false,
            "description":"Read current, revision-guarded byte pages via read_file, NOT read_tool_output; a change refuses the page. No snapshot was retained."});
        output["properties"]["source_sha256"] = json!({"type": "string", "description": "SHA-256 of the complete original file bytes, including the UTF-16 BOM and bytes outside this page. For decoded UTF-16, canonical_sha256 hashes the distinct retained UTF-8 snapshot. Default attached UTF-8 HTML can still be rendered text: use explicit selectors for the original hash/bytes. A UTF-16 raw hash does not make decoded ranges suitable for codex-range patches."});
        output["properties"]["unchanged"] = json!({"const":true, "description":"The freshly read source representation matches if_source_hash and the requested selection resolved completely. No content, snapshot, recovery handle or acquisition evidence is delivered; complete and file_complete are false. This compares source identity, not prior selection coverage."});
        output["properties"]["selection_status"]["enum"] = json!(["complete", "partial", "failed", "unchanged"]);
        output["properties"]["artifact_id"] = json!({"type": ["string", "null"], "description": "Immutable snapshot identity when retained; omitted or null for ordinary complete inline reads or unavailable storage. An active typed inspect:path obligation retains its acquisition identity even for a complete inline selection."});
        output["properties"]["canonical_sha256"]["description"] = json!(
            "SHA-256 of the retained selector snapshot. UTF-16 snapshots contain decoded UTF-8 without the BOM, so this differs from the original-byte source_sha256; omitted from presentation only when identical."
        );
        output["properties"]["delivered_selection_complete"]["description"] = json!(
            "Compatibility alias for complete; omitted from model presentation when identical."
        );
        if let Some(required) = output["required"].as_array_mut() {
            required.retain(|key| {
                !matches!(
                    key.as_str(),
                    Some(
                        "artifact_id"
                            | "canonical_bytes"
                            | "canonical_sha256"
                            | "delivered_selection_complete"
                            | "retained_bytes"
                            | "retained_artifact_complete"
                    )
                )
            });
        }
        output["properties"]["retained_bytes"]["description"] = json!(
            "Retained source bytes; omitted in presentation only for inline-only reads with no artifact, zero retained bytes and no snapshot error."
        );
        output["properties"]["retained_artifact_complete"]["description"] = json!(
            "Whether all source bytes are retained; omitted alongside retained_bytes only for inline-only reads with no artifact (false)."
        );
        output["properties"]["file_complete"] = json!({"type": "boolean", "description": "The returned exact bytes and hydrated ranges cover the whole selected source representation in this tool result. This is acquisition coverage, not proof that enclosing script output displayed every byte or that the model semantically reviewed it. Retention, match coordinates, and recovery selectors alone do not establish coverage."});
        output["properties"]["selector_bindings"] = json!({"type":"array", "maxItems":64,
            "description":"Successful symbol/enclosing requests bound by their original zero-based selector index to exact snapshot ranges, before result sorting/merging. Resolution is not delivery: check results and completeness separately. Bound to canonical_sha256 (decoded UTF-8 for UTF-16); no source is duplicated. qualified_name and kind are the bound item's parser-derived identity (owner included), not type resolution.",
            "items":{"type":"object", "properties":{
                "selector_index":{"type":"integer","minimum":0},
                "qualified_name":{"type":"string"},
                "kind":{"type":"string"},
                "syntax_error":{"const":true, "description":"The item or an owner header contains a parse error; its qualified identity or range may be wrong. Exact source delivery can still be complete."},
                "resolved_selectors":{"type":"array","minItems":1,"maxItems":1,"items":file_selector_schema()}},
                "required":["selector_index","resolved_selectors"],"additionalProperties":false}});
        output["properties"]["selector_errors"] = json!({"type":"array", "maxItems":64,
            "description":"Per-selector structural failures; successful results share this snapshot. Candidate line selectors are exact, not guesses.",
            "items":{"type":"object", "properties":{
                "selector_index":{"type":"integer","minimum":0}, "selector":file_selector_schema(),
                "status":{"const":"invalid_selector"}, "complete":{"const":false}, "message":{"type":"string"},
                "source_sha256":{"type":"string", "description":"Compatibility field for this diagnostic's canonical snapshot hash, not the original UTF-16 byte hash."}, "omitted_candidates":{"type":"integer","minimum":0},
                "candidates":{"type":"array","maxItems":8,"items":{"type":"object","properties":{
                    "qualified_name":{"type":"string"}, "selector":file_selector_schema()},
                    "required":["qualified_name","selector"],"additionalProperties":false}}},
                "required":["selector_index","selector","status","complete","message","source_sha256","candidates","omitted_candidates"],
                "additionalProperties":false}});
        for field in ["selector_errors_omitted", "selector_bindings_omitted"] {
            output["properties"][field] = json!({"type":"integer", "minimum":0,
                "description":"Diagnostic entries omitted only from direct delivery to fit its exact budget; omitted diagnostics are not complete selector coverage."});
        }
        output["properties"]["delivery_note"] = json!({"type":"string"});
        output["properties"]["continuation"] =
            serde_json::to_value(file_selector_schema()).unwrap_or_default();
        output["properties"]["recovery"] = json!({
            "type": "object",
            "description": "Immutable continuation hint: snapshot binds the retained selector representation; remaining_selectors preserves every unread selection and its limits. Call tools.read_tool_output(recovery.arguments), never rerun the producer. arguments is a bounded first batch; omitted_selectors counts the remaining selections outside that batch. Completeness describes this receipt, not future recovery.",
            "properties": {
                "tool": {"const": "read_tool_output"},
                "snapshot": {"type":"object", "properties":{
                    "artifact_id":{"type":"string"}, "canonical_sha256":{"type":"string"},
                    "canonical_bytes":{"type":"integer", "minimum":0}},
                    "required":["artifact_id","canonical_sha256","canonical_bytes"], "additionalProperties":false},
                "selection_complete": {"type":"boolean"},
                "file_complete": {"type":"boolean"},
                "retained_artifact_complete": {"type":"boolean"},
                "remaining_selectors": {"type":"array", "minItems":1, "items":file_selector_schema()},
                "omitted_selectors": {"type":"integer", "minimum":0},
                "arguments": {
                    "type": "object",
                    "properties": {
                        "artifact_id": {"type": "string"},
                        "selectors": {"type": "array", "items": file_selector_schema(), "minItems": 1, "maxItems": 64},
                        "max_bytes": {"type":"integer", "minimum":1, "maximum":READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES,
                            "description":"Code-mode recipes retain the script payload budget; direct recipes keep the default display-sized cap."}
                    },
                    "required": ["artifact_id", "selectors"],
                    "additionalProperties": false
                }
            },
            "required": ["tool", "arguments", "snapshot", "selection_complete", "file_complete", "retained_artifact_complete", "remaining_selectors"],
            "additionalProperties": false
        });
        output["properties"]["page_selectors"] = json!({
            "type": "array", "maxItems": 8, "items": file_selector_schema(),
            "description": "First up to eight independent pages of continuation, in source order. Fetch in parallel; continuation retains the full remaining extent."
        });
        output["properties"]["snapshot_error"] = json!({"type": "string", "description": "Snapshot storage failed; inline evidence is still valid, but no recovery handle or continuation is available."});
        output["allOf"] = json!([{
            "if":{"required":["read_mode"], "properties":{"read_mode":{"const":"byte_ranges"}}},
            "then":{"required":["file_bytes","file_revision","retained_bytes","retained_artifact_complete"]},
            "else":{"if":{"required":["read_mode"], "properties":{"read_mode":{"const":"unsupported_text"}}},
                "then":{"required":["file_bytes","classification","binary_recovery","retained_bytes","retained_artifact_complete"]},
                "else":{"required":["source_sha256","canonical_bytes","total_lines"]}}
        }]);
        #[expect(
            clippy::expect_used,
            reason = "read_tool_output_output_schema constructs an object with a required array"
        )]
        output["required"]
            .as_array_mut()
            .expect("object schema")
            .extend([json!("path"), json!("file_complete")]);
        ToolSpec::Function(ResponsesApiTool {
            name: "read_file".to_string(),
            description: format!(
                "Read a file without shell quoting. Unsupported display text is withheld before selection: read_mode:unsupported_text returns file_bytes, classification (kind/confidence/reason, not proof of binary), and bounded 256-byte hex/base64 binary_recovery recipes. Invalid UTF-8 alone is unknown encoding, not proven binary; BOM-marked UTF-16 is decoded before control-character classification. Original-byte pages encode unsupported text as data_base64 with per-result classification applying only to that range. UTF-8 and BOM-marked UTF-16LE/BE text snapshots are limited to {MAX_FILE_MIB} MiB for both original and decoded bytes. UTF-16 is strictly decoded to UTF-8 without the BOM; source_encoding names the original encoding, source_sha256 hashes original bytes including the BOM, and canonical_sha256/canonical_bytes identify the retained decoded UTF-8 snapshot. All snapshot selectors (including bytes, search offsets, lines and structure), canonical_range and read_tool_output recovery address that decoded UTF-8 snapshot, not UTF-16 bytes or code units. Use encoding:hex|base64 for original-byte offsets. UTF-16 decoded acquisition does not grant original-byte source_inspection evidence or authorize codex-range edits. Git revision reads remain UTF-8 only. Larger current filesystem files support bytes-only selectors via bounded seek reads (up to 64 KiB per script page), including binary or split UTF-8 via data_base64. These return read_mode:byte_ranges, file_bytes and file_revision, never a whole-file source_sha256, line count or retained snapshot; file_complete and retained_artifact_complete stay false. Use range_recovery.arguments with read_file, or pass if_file_revision plus byte selectors for other pages. Changed identity/metadata refuses the page; this is not an immutable snapshot. Line/search/structural selectors, host locators and Git revision reads retain the snapshot limit. Symbol, enclosing and search with enclosing:true support Rust (.rs) and Python (.py). Section outline/item IDs also support Markdown (.md, .markdown): headings have source-bound IDs, levels, parent IDs and exact byte ranges; sections include descendants until the next equal/shallower heading. Duplicate titles are distinct; fenced code is ignored and Setext headings are recognized. For other languages, use fixed-string search without enclosing or explicit line ranges; unsupported structural requests are rejected. Prefer explicit line ranges or supported symbols for known code; do not dump whole files to locate one function. Path alone returns a first page capped at about {DEFAULT_FILE_READ_TOKEN_CEILING} tokens plus continuation; small files fit inline. Host-owned skill/context guidance keeps its existing delivery budget. Default HTML outside the workspace may be rendered as text: hashes, ranges and recovery then describe that text; use explicit selectors for exact original markup. Use read_tool_output with the artifact_id and continuation only for needed remaining ranges of the immutable snapshot. complete describes the requested page or selectors; file_complete describes acquired source coverage, not final script display or semantic review. A complete inline read may have no retained artifact; that is not a read failure. Explicit lines, bytes and fixed-string search selectors remain exact. In code mode explicit selections return up to 1 MiB to the script independently of its display budget: consume it there and print only needed evidence, not the entire recovered result. Omit revision for current contents; revision reads resolve immutable Git objects and reuse the same selectors and snapshots. Pass a skill: locator to read its SKILL.md; omit environment_id for host-owned skills.\n\nFor raw binary inspection of any current file size, set encoding:hex or encoding:base64 with explicit zero-based half-open bytes selectors. This always uses the same bounded revision-guarded range backend, returning data_hex or data_base64 and exact canonical_range offsets, never decoded text. At most 64 KiB of raw bytes per script call; direct pages may be smaller to fit delivery. Compatible byte ranges are merged before I/O within each revision-bound delivery group, then sliced and encoded at original selector boundaries. Live results retain selector_index (including duplicates); merging grants no extra coverage or revision guarantee. No execution, decompression, format detection or implicit decoding. range_recovery preserves encoding; use it with read_file, not read_tool_output.\n\n{}",
                include_str!("../../../templates/result_binding_checkpoint.md").trim(),
            ),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema {
                one_of: Some(["path", "file_path"].into_iter().map(|field| JsonSchema {
                    required: Some(vec![field.to_string()]),
                    ..JsonSchema::object(BTreeMap::new(), None, None)
                }).collect()),
                ..JsonSchema::object(BTreeMap::from([
                ("path".to_string(), JsonSchema::string(Some("File path, relative to the environment cwd or absolute, a `skill:` locator, `skill:catalog` for enabled skill metadata, `context:desktop` for Desktop guidance, `context:workflow-evidence` for diagnostic/evidence-role and metric-attribution guidance, `context:resume-state` for a read-only durable-operation/process/artifact inventory, or the `context:unsettled-tools/<offset>` recovery_path from a resume notice. Omit environment_id for host-owned locators. Resume inventory is an immutable evidence snapshot, not live control authority; recover its continuation through read_tool_output rather than rebuilding pages.".to_string()))),
                ("file_path".to_string(), JsonSchema::string(Some("Legacy alias for path; use only one.".to_string()))),
                ("offset".to_string(), JsonSchema { minimum: Some(1.into()), maximum: Some(usize::MAX.into()), ..JsonSchema::integer(Some("Legacy 1-based starting line; use instead of selectors, defaults to 1.".to_string())) }),
                ("limit".to_string(), JsonSchema { minimum: Some(1.into()), maximum: Some(usize::MAX.into()), ..JsonSchema::integer(Some("Legacy positive line count; defaults to 2000 when offset is supplied.".to_string())) }),
                ("environment_id".to_string(), JsonSchema::string(Some("Environment id; omit to use the primary environment.".to_string()))),
                ("encoding".to_string(), JsonSchema { enum_values: Some(vec!["hex".into(), "base64".into()]), ..JsonSchema::string(Some("Explicit raw-byte output for filesystem bytes-only selectors at any file size. hex returns lowercase data_hex; base64 returns standard padded data_base64. No text decoding or execution. Uses bounded live ranges, not whole-file snapshots; cannot combine with revision or if_source_hash.".into())) }),
                ("revision".to_string(), JsonSchema::string(Some("Read an exact Git blob at this commit revision instead of the workspace. Path is repository-root-relative (repository selected by environment cwd). Resolves once to immutable commit_oid/blob_oid in git_source; reuse returned commit_oid as revision to pin later selectors, or recover the retained snapshot. All source selectors and the 8 MiB UTF-8 limit apply. Local environments with full disk read access only; no fetching, filters, text conversion or replacement objects. Historical path/URI are distinct from workspace evidence. Omit for current contents.".into()))),
                ("if_file_revision".to_string(), JsonSchema::string(Some("Opaque file_revision from a large-file byte read. Requires filesystem bytes-only selectors; refuses a changed object/metadata revision, even if the file shrank below 8 MiB. Not a hash, snapshot, or if_source_hash freshness shortcut. Cannot combine with revision or if_source_hash.".into()))),
                ("if_source_hash".to_string(), JsonSchema::string(Some("Optional 64-hex source_sha256 from a previous read. Reacquire current contents through the same authorized owner and compare the whole selected representation, not just requested ranges. On a match and complete selector resolution, return unchanged:true without content or retention; invalid, missing or partial selections keep their normal results. Omit when you need content (including a new selection); a match is not evidence that you previously acquired these selectors. Default rendered HTML and explicit exact markup have different identities.".to_string()))),
                ("force_fresh".to_string(), JsonSchema::boolean(Some("Legacy compatibility option; every call reads current contents.".to_string()))),
                ("selectors".to_string(), selectors),
                ]), None, Some(false.into()))
            },
            output_schema: Some(output.into()),
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "read_file requires function arguments".to_string(),
                ));
            };
            let args = parse_read_file_arguments(arguments)?;
            if args.selectors.as_ref().is_some_and(|selectors| {
                selectors.is_empty() || selectors.len() > READ_TOOL_OUTPUT_MAX_SELECTORS
            }) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "read_file requires 1-{READ_TOOL_OUTPUT_MAX_SELECTORS} selectors"
                )));
            }
            if args.selectors.iter().flatten().any(|selector| {
                !matches!(
                    selector,
                    FileSelector::Structure(_)
                        | FileSelector::Exact(
                            ToolOutputSelector::Bytes { .. }
                                | ToolOutputSelector::Lines { .. }
                                | ToolOutputSelector::Search { .. }
                                | ToolOutputSelector::Section { .. }
                                | ToolOutputSelector::JsonPointer { .. }
                                | ToolOutputSelector::Structured { .. }
                        )
                )
            }) {
                return Err(FunctionCallError::RespondToModel(
                    "read_file supports bytes, lines, search, section, json_pointer, structured, symbol, and enclosing selectors".to_string(),
                ));
            }
            if invocation.cancellation_token.is_cancelled() {
                return Err(FunctionCallError::RespondToModel(
                    "read_file cancelled".to_string(),
                ));
            }
            let turn = &invocation.step_context.turn;
            let mut source_inspection = None;
            let mut inspection_proof_error = None;
            let mut git_source = None;
            let mut decoded_source = None;
            if args.path == crate::session::resume_inventory::LOCATOR
                && (args.environment_id.is_some() || args.revision.is_some()
                    || args.encoding.is_some() || args.if_file_revision.is_some())
            {
                return Err(FunctionCallError::RespondToModel(
                    "omit environment_id, revision, encoding and if_file_revision for host-owned resume inventory".into(),
                ));
            }
            // The catalog advertises `skill:<id>` locators, so this tool has to
            // resolve them. Without it the model can see every skill listed and
            // load none of them.
            let (contents, resolved_path, environment_id, canonical_uri) = if let Some(revision) = &args.revision {
                let environment = wait_for_tool_environment(
                    &invocation.step_context.environments, args.environment_id.as_deref(),
                    &invocation.cancellation_token,
                ).await?.ok_or_else(|| FunctionCallError::RespondToModel(
                    "historical reads require a selected local environment".into(),
                ))?;
                if environment.environment.is_remote() || !turn.file_system_sandbox_policy().has_full_disk_read_access() {
                    return Err(FunctionCallError::RespondToModel(
                        "historical reads require a local environment with full disk read access; remote/restricted Git reads are not supported".into(),
                    ));
                }
                crate::tools::parallel::wait_for_workspace_baseline().await;
                let cwd = environment.cwd().to_abs_path().map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                let source = tokio::select! {
                    _ = invocation.cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("read_file cancelled".into())),
                    source = codex_git_utils::read_historical_file(cwd.to_path_buf(), args.path.clone(), revision.clone(), MAX_FILE_BYTES) => source.map_err(FunctionCallError::RespondToModel)?,
                };
                let root = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&source.repository_root)
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                let uri = format!("{}?git_commit={}&git_blob={}", codex_utils_path_uri::PathUri::from_abs_path(&root), source.commit_oid, source.blob_oid);
                let locator = format!("git:{}:{}:{}", source.repository_root.display(), source.commit_oid, source.path);
                git_source = Some(json!({"repository_root":source.repository_root, "path":source.path,
                    "commit_oid":source.commit_oid, "blob_oid":source.blob_oid}));
                (source.contents, locator, environment.environment_id.clone(), uri)
            } else if args.path
                == crate::turn_diff_tracker::TURN_DIFF_LOCATOR
            {
                return Err(FunctionCallError::RespondToModel(
                    "context:turn-diff is not available; tracked deltas are not a reliable review source yet.".to_string(),
                ));
            } else if args.path == crate::session::resume_inventory::LOCATOR {
                let contents = invocation.session.read_resume_inventory(
                    turn.config.codex_home.as_path(), &turn.sub_id,
                ).await.map_err(FunctionCallError::RespondToModel)?;
                (contents, args.path.clone(), "host-context".to_string(), args.path.clone())
            } else if let Some(offset) = args.path.strip_prefix("context:unsettled-tools/") {
                if args.environment_id.is_some() {
                    return Err(FunctionCallError::RespondToModel(
                        "omit environment_id for host-owned recovery history".into(),
                    ));
                }
                let offset = offset.parse::<usize>().map_err(|_| {
                    FunctionCallError::RespondToModel("invalid unsettled recovery offset".into())
                })?;
                let contents = invocation
                    .session
                    .read_unsettled_tool_recovery(offset, &turn.sub_id)
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                (
                    contents,
                    args.path.clone(),
                    "host-context".to_string(),
                    args.path.clone(),
                )
            } else if args.path == crate::context::desktop_instructions::LOCATOR {
                if args.environment_id.is_some() {
                    return Err(FunctionCallError::RespondToModel(
                        "omit environment_id for host-owned Desktop guidance".to_string(),
                    ));
                }
                let contents = turn
                    .developer_instructions
                    .as_deref()
                    .and_then(crate::context::desktop_instructions::deferred)
                    .ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "no Desktop guidance is configured for this turn".to_string(),
                        )
                    })?;
                if contents.len() > MAX_FILE_BYTES {
                    return Err(FunctionCallError::RespondToModel(
                        "Desktop guidance exceeds the read_file size limit".to_string(),
                    ));
                }
                // The tools this guidance names but the inventory lacks are
                // listed here, where the names are read, not in every request.
                let missing = invocation
                    .step_context
                    .tool_router()
                    .map(|router| router.desktop_guidance_unavailable_tools(contents.as_ref()))
                    .unwrap_or_default();
                (
                    if missing.is_empty() {
                        contents.into_owned()
                    } else {
                        format!("{contents}\n\n{missing}")
                    },
                    args.path.clone(),
                    "host-context".to_string(),
                    args.path.clone(),
                )
            } else if args.path == crate::session::multi_agents::USAGE_HINT_LOCATOR {
                if args.environment_id.is_some() {
                    return Err(FunctionCallError::RespondToModel(
                        "omit environment_id for host-owned agent team guidance".to_string(),
                    ));
                }
                let contents =
                    crate::session::multi_agents::full_usage_hint_text(turn, &turn.session_source)
                        .ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "no agent team guidance is configured for this turn".to_string(),
                            )
                        })?;
                (
                    contents.to_string(),
                    args.path.clone(),
                    "host-context".to_string(),
                    args.path.clone(),
                )
            } else if args.path == "context:workflow-evidence" {
                if args.environment_id.is_some() {
                    return Err(FunctionCallError::RespondToModel(
                        "omit environment_id for host-owned workflow evidence guidance".to_string(),
                    ));
                }
                let contents = concat!(
                    include_str!("../../../templates/diagnostic_triage.md"),
                    "\n",
                    include_str!("../../../templates/metric_attribution.md"),
                );
                (
                    contents.to_string(),
                    args.path.clone(),
                    "host-context".to_string(),
                    args.path.clone(),
                )
            } else if args
                .path
                .starts_with(codex_core_skills::SKILL_CATALOG_LOCATOR_PREFIX)
            {
                crate::tools::parallel::wait_for_workspace_baseline().await;
                let (contents, path) = read_skill_locator(&invocation, &args).await?;
                let uri = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&path)
                    .ok()
                    .map(|path| codex_utils_path_uri::PathUri::from_abs_path(&path).to_string())
                    .unwrap_or_else(|| args.path.clone());
                (contents, path, "host-skills".to_string(), uri)
            } else {
                match read_environment_file(&invocation, &args, &mut source_inspection, &mut inspection_proof_error, &mut decoded_source).await? {
                    EnvironmentFileRead::Snapshot(snapshot) => snapshot,
                    EnvironmentFileRead::Ranges(output) => return Ok(boxed_tool_output(output)),
                }
            };
            if invocation.cancellation_token.is_cancelled() {
                return Err(FunctionCallError::RespondToModel(
                    "read_file cancelled".to_string(),
                ));
            }
            let thread_id = invocation.session.thread_id.to_string();
            let explicit_selection = args.selectors.is_some();
            // Attached reports are read for their text. Their markup, scripts
            // and styles otherwise fill each page and are replayed by every
            // later request, and single-line documents leave no lines to page
            // by. Workspace files and explicit selections keep exact bytes.
            let render_html = !explicit_selection
                && decoded_source.is_none()
                && git_source.is_none()
                && source_inspection.is_none()
                && html_outside_workspace(&invocation, &environment_id, &resolved_path);
            let script_consumer = matches!(&invocation.source, ToolCallSource::CodeMode { .. });
            // Display-sized continuations remain useful for direct model reads.
            // Script selection itself uses a fixed byte cap, not this budget.
            let output_budget = match &invocation.source {
                ToolCallSource::CodeMode { cell_id, .. } => invocation
                    .session
                    .services
                    .code_mode_service
                    .output_budget(cell_id),
                _ => {
                    let policy: codex_protocol::protocol::TruncationPolicy =
                        turn.model_info.truncation_policy.into();
                    Some(invocation.step_context.model_output_token_limit(
                        &invocation.session,
                        policy.token_budget(),
                    ).await)
                }
            }
            .unwrap_or(RECOVERY_AGGREGATE_TOKEN_CEILING);
            // Tiny/zero projections must not disable data processing inside JS.
            // Reserve space for file metadata, page handles, and the exec wrapper.
            let token_ceiling = if script_consumer {
                output_budget.max(2_000).saturating_sub(1_000)
            } else {
                output_budget.saturating_sub(256)
            }
            .min(RECOVERY_AGGREGATE_TOKEN_CEILING.saturating_sub(1_000));
            let default_preview = !explicit_selection
                && !matches!(environment_id.as_str(), "host-context" | "host-skills");
            let selection_ceiling = if default_preview {
                if script_consumer {
                    token_ceiling
                        .clamp(MIN_FILE_READ_PREVIEW_TOKENS, DEFAULT_FILE_READ_TOKEN_CEILING)
                } else {
                    token_ceiling.min(DEFAULT_FILE_READ_TOKEN_CEILING)
                }
            } else {
                token_ceiling
            };
            let structure_path = args.path.clone();
            let if_source_hash = args.if_source_hash;
            let (
                canonical,
                mut result,
                mut continuation,
                page_selectors,
                total_lines,
                selector_errors,
                selector_bindings,
                rendered_from,
            ) = tokio::task::spawn_blocking(move || {
                let mut contents = contents;
                let mut rendered_from = None;
                // A self-declared evidence header binds the exact document bytes.
                if render_html && !contents.contains("\"evidence_lineage\"") {
                    let text = html_text::render(&contents);
                    // Mostly-text pages read as well exact.
                    if !text.trim().is_empty()
                        && text.len().saturating_mul(10) <= contents.len().saturating_mul(9)
                    {
                        rendered_from = Some(contents.len());
                        contents = text;
                    }
                }
                let total_lines = contents.lines().count();
                let exact = args
                    .selectors
                    .iter()
                    .flatten()
                    .filter_map(|selector| match selector {
                        FileSelector::Exact(selector) => Some(selector.clone()),
                        FileSelector::Structure(_) => None,
                    })
                    .collect::<Vec<_>>();
                let mut canonical = file_snapshot(contents, Some(&exact))?;
                let (selectors, selector_errors, selector_bindings) =
                    structure::resolve_batch(&structure_path, &mut canonical, args.selectors);
                let selection = if script_consumer && !default_preview {
                    select_file_snapshot_for_script(&canonical, selectors)
                } else {
                    select_file_snapshot_with_ceiling(&canonical, selectors, selection_ceiling)
                };
                selection.map(|(result, continuation)| {
                    let pages = match &continuation {
                        Some(ToolOutputSelector::Bytes { start, end }) => {
                            crate::tools::command_output_artifact::bounded_page_selectors(
                                &result.artifact_id,
                                &canonical.bytes,
                                *start,
                                *end,
                                token_ceiling.min(8_000),
                            )
                        }
                        _ => Vec::new(),
                    };
                    (
                        canonical,
                        result,
                        continuation,
                        pages,
                        total_lines,
                        selector_errors,
                        selector_bindings,
                        rendered_from,
                    )
                })
            })
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("file selection worker failed: {err}"))
            })?
            .map_err(|err| FunctionCallError::RespondToModel(err.for_model()))?;
            let canonical = Arc::new(canonical);
            result.complete &= selector_errors.is_empty();
            if let Some(source_bytes) = rendered_from
                && let Some(first) = result.results.first_mut()
            {
                let note = format!(
                    "Rendered as text from {source_bytes} bytes of HTML outside the workspace: markup, scripts and styles removed. Lines, ranges, source_sha256 and recovery describe this text; read_file selectors return exact markup."
                );
                first.message = Some(match first.message.take() {
                    Some(message) => format!("{note} {message}"),
                    None => note,
                });
            }
            let mut successful = result.selection_status() == "complete";
            // Validate/resolve the original selectors before suppressing content:
            // matching bytes must not hide an invalid symbol, missing pointer,
            // mixed failure or budget-limited selection. No retained snapshot or
            // delivered-evidence signal is created by this freshness-only reply.
            let source_hash = decoded_source.as_ref().map_or(canonical.sha256.as_str(), |source| source.sha256.as_str());
            if successful && if_source_hash.as_deref() == Some(source_hash) {
                let mut output = json!({
                    "path": resolved_path,
                    "environment_id": environment_id,
                    "canonical_uri": canonical_uri,
                    "source_sha256": canonical.sha256,
                    "canonical_sha256": canonical.sha256,
                    "canonical_bytes": canonical.exact_bytes,
                    "total_lines": total_lines,
                    "unchanged": true,
                    "selection_status": "unchanged",
                    "complete": false,
                    "delivered_selection_complete": false,
                    "file_complete": false,
                    "retained_bytes": 0,
                    "retained_artifact_complete": false,
                    "results": [],
                });
                encoding::annotate(&mut output, decoded_source.as_ref());
                if let Some(source) = &git_source { output["git_source"] = source.clone(); }
                let projected = codex_code_mode::model_visible_tool_result(
                    &ToolName::plain("read_file"), &output,
                ).unwrap_or_else(|| output.clone());
                let output = JsonToolOutput::with_success(output, Some(true))
                    .with_model_value(projected);
                crate::tools::context::ensure_exact_model_delivery(&output, &invocation)?;
                return Ok(boxed_tool_output(output));
            }
            let file_complete = selector_errors.is_empty() && file_selection_complete(&result);
            // Explicit selections retain the existing immutable-snapshot contract.
            // Complete default reads stay inline; omitted bytes need one durable
            // snapshot, but selecting them never rereads the file just written.
            let mut artifact_created = false;
            let artifact = if explicit_selection || continuation.is_some() || source_inspection.is_some() {
                let reused = if let Some(id) = invocation
                    .session
                    .reusable_tool_artifact(
                        &invocation.call_id,
                        canonical.exact_bytes,
                        &canonical.sha256,
                    )
                    .await
                {
                    let artifact =
                        crate::tools::command_output_artifact::attach_canonical_output_artifact(
                            &turn.config.codex_home,
                            &thread_id,
                            &id,
                            &canonical,
                        )
                        .await;
                    artifact.complete.then_some(artifact)
                } else {
                    None
                };
                Some(match reused {
                    Some(artifact) => artifact,
                    None => {
                        artifact_created = true;
                        let backing = invocation.session.source_snapshot_backing(canonical.exact_bytes, &canonical.sha256).await;
                        invocation.session.create_source_snapshot_artifact(
                            invocation.call_id.clone(),
                            Arc::clone(&canonical),
                            backing,
                        )
                        .await
                    }
                })
            } else {
                None
            };
            let mut artifact_id = artifact
                .as_ref()
                .filter(|artifact| artifact.complete)
                .and_then(CanonicalOutputArtifact::artifact_id);
            let registration_error = if !artifact_created && let Some(id) = &artifact_id {
                invocation
                    .session
                    .try_register_tool_artifact_origin(
                        id.clone(),
                        invocation.call_id.clone(),
                        canonical.exact_bytes,
                        canonical.sha256.clone(),
                    )
                    .await.err().map(|error| error.to_string())
            } else {
                None
            };
            if registration_error.is_some() {
                artifact_id = None;
            }
            let snapshot_error = registration_error.or_else(||
                artifact
                    .as_ref()
                    .filter(|_| artifact_id.is_none())
                    .map(|artifact| {
                        artifact.error.clone().unwrap_or_else(|| {
                            "unable to retain the complete file snapshot".to_string()
                        })
                    }));
            if artifact_id.is_none() {
                result.retained_bytes = 0;
                continuation = None;
                for selected in &mut result.results {
                    selected.child_selectors.clear();
                    selected.continuation = None;
                    selected.subdivision_plan = None;
                    if !selected.complete {
                        selected.message = Some("Selection is incomplete and snapshot recovery is unavailable; use read_file to read current file contents.".to_string());
                    }
                }
            }
            let refit_selectors = (!result.results.is_empty()).then(|| {
                result.results.iter().map(|result| result.selector.clone()).collect::<Vec<_>>()
            });
            let mut evidence = result.delivered_evidence().map(|identity| {
                let lineage =
                    crate::tools::context::declared_file_lineage_evidence(&canonical.bytes);
                json!({
                "source": "read_file",
                "producer": lineage,
                "scope": {
                    "path": resolved_path,
                    "environment": environment_id,
                    "canonical_uri": canonical_uri,
                },
                "identity": identity,
                })
            });
            let mut output = serde_json::to_value(result)
                .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
            output["artifact_id"] = json!(artifact_id);
            output["retained_artifact_complete"] = json!(artifact_id.is_some());
            output["path"] = json!(resolved_path);
            output["environment_id"] = json!(environment_id);
            output["canonical_uri"] = json!(canonical_uri);
            output["total_lines"] = json!(total_lines);
            output["source_sha256"] = json!(canonical.sha256);
            encoding::annotate(&mut output, decoded_source.as_ref());
            output["file_complete"] = json!(file_complete);
            if let Some(source) = &git_source { output["git_source"] = source.clone(); }
            if let Some(error) = inspection_proof_error {
                output["inspection_proof_error"] = json!(error);
            }
            if !selector_errors.is_empty() {
                output["selector_errors"] = json!(selector_errors);
            }
            if !selector_bindings.is_empty() {
                output["selector_bindings"] = json!(selector_bindings);
            }
            if let Some(continuation) = continuation {
                output["continuation"] = json!(continuation);
                if !page_selectors.is_empty() {
                    output["page_selectors"] = json!(page_selectors);
                }
            }
            if let Some(error) = snapshot_error {
                output["snapshot_error"] = json!(error);
            }
            if let Some(artifact_id) = &artifact_id
                && let Some(mut recovery) = file_recovery_recipe(&output, artifact_id)
            {
                // Preserve the consumer's existing byte budget across this
                // mechanical continuation. Otherwise a script-sized read
                // silently falls back to 16 KiB recovery pages. The recovery
                // owner still enforces its serialized cap and exact scope.
                if script_consumer {
                    recovery["arguments"]["max_bytes"] = json!(READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES);
                }
                output["recovery"] = recovery;
            }
            let mut projected = codex_code_mode::model_visible_tool_result(&ToolName::plain("read_file"), &output)
                .unwrap_or_else(|| output.clone());
            if !script_consumer && !file_projection_fits(&projected, &invocation) {
                compact_file_diagnostics(&mut projected, &invocation);
                if projected["selector_errors_omitted"].as_u64().unwrap_or(0) > 0
                    || projected["selector_bindings_omitted"].as_u64().unwrap_or(0) > 0
                {
                    successful = false;
                    projected["complete"] = json!(false);
                }
                if !file_projection_fits(&projected, &invocation) {
                    // Refit against the actual escaped envelope. Keep the one
                    // source snapshot; do not reread, reparse, or write it twice.
                    if artifact_id.is_none() && artifact.is_none() {
                        let backing = invocation.session.source_snapshot_backing(canonical.exact_bytes, &canonical.sha256).await;
                        let retained = invocation.session.create_source_snapshot_artifact(
                            invocation.call_id.clone(), Arc::clone(&canonical),
                            backing,
                        ).await;
                        artifact_id = retained.complete.then(|| retained.artifact_id()).flatten();
                        if artifact_id.is_none() {
                            projected["snapshot_error"] = json!(retained.error.unwrap_or_else(|| "snapshot retention unavailable".into()));
                        }
                    }
                    successful = false;
                    projected["complete"] = json!(false);
                    projected["delivered_selection_complete"] = json!(false);
                    projected["selection_status"] = json!("partial");
                    projected["file_complete"] = json!(false);
                    projected["artifact_id"] = json!(artifact_id);
                    projected["retained_artifact_complete"] = json!(artifact_id.is_some());
                    projected["retained_bytes"] = json!(if artifact_id.is_some() { canonical.exact_bytes } else { 0 });
                    projected["delivery_note"] = json!("The requested envelope exceeded delivery capacity. Exact selections were refitted; when no selector resolved, a source prefix is provided. This is not complete original-selector coverage. Recover the immutable continuation; retry a smaller original-selector batch for omitted diagnostics.");
                    let mut ceiling = token_ceiling / 2;
                    loop {
                        let snapshot = Arc::clone(&canonical);
                        let selectors = refit_selectors.clone();
                        let (mut prefix, next) = tokio::task::spawn_blocking(move || {
                            select_file_snapshot_with_ceiling(&snapshot, selectors, ceiling)
                        }).await.map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
                            .map_err(|error| FunctionCallError::RespondToModel(error.for_model()))?;
                        if artifact_id.is_none() {
                            prefix.retained_bytes = 0;
                            for selected in &mut prefix.results {
                                selected.child_selectors.clear();
                                selected.continuation = None;
                                selected.subdivision_plan = None;
                            }
                        }
                        projected["results"] = serde_json::to_value(&prefix).map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?["results"].take();
                        if let Some(fields) = projected.as_object_mut() {
                            fields.remove("continuation");
                            fields.remove("recovery");
                            fields.remove("page_selectors");
                        }
                        if let (Some(id), Some(next)) = (&artifact_id, next) {
                            projected["continuation"] = json!(next);
                            projected["recovery"] = json!({"tool":"read_tool_output", "arguments":{"artifact_id":id, "selectors":[next]}});
                        }
                        if let Some(id) = &artifact_id && let Some(recipe) = file_recovery_recipe(&projected, id) {
                            projected["recovery"] = recipe;
                        }
                        evidence = prefix.delivered_evidence().map(|identity| json!({
                            "source":"read_file", "scope":{"path":resolved_path, "environment":environment_id, "canonical_uri":canonical_uri}, "identity":identity,
                        }));
                        if file_projection_fits(&projected, &invocation) || ceiling == 0 {
                            break;
                        }
                        ceiling /= 2;
                    }
                }
            }
            if artifact_id.is_none() {
                for packet in [&mut output, &mut projected] {
                    if let Some(fields) = packet.as_object_mut() {
                        for field in ["continuation", "recovery", "page_selectors"] { fields.remove(field); }
                    }
                    for selected in packet["results"].as_array_mut().into_iter().flatten() {
                        if let Some(fields) = selected.as_object_mut() {
                            for field in ["continuation", "child_selectors", "subdivision_plan"] { fields.remove(field); }
                        }
                    }
                }
            }
            for packet in [&mut output, &mut projected] {
                let complete = packet["complete"] == true;
                let file_complete = packet["file_complete"] == true;
                if let Some(recovery) = packet.get_mut("recovery") {
                    recovery["selection_complete"] = json!(complete);
                    recovery["file_complete"] = json!(file_complete);
                }
            }
            // No inspection credit or history mutation for a packet the final
            // guard would reject. Extremely small budgets can fit no envelope.
            if !script_consumer {
                crate::tools::context::ensure_exact_model_delivery(
                    &JsonToolOutput::with_success(projected.clone(), Some(successful)),
                    &invocation,
                )?;
            }
            // Raw script data keeps its full selection/diagnostics, but any
            // newly retained fallback snapshot has the same public identity.
            for field in ["artifact_id", "retained_artifact_complete", "retained_bytes", "snapshot_error"] {
                if let Some(value) = projected.get(field) {
                    output[field] = value.clone();
                }
            }
            if let (Some(source), Some(id)) = (source_inspection, artifact_id.as_ref())
                && !invocation.cancellation_token.is_cancelled()
                && source_inspection_delivery_eligible(if script_consumer { &output } else { &projected })
            {
                let acquired = source_inspection_selectors(if script_consumer { &output } else { &projected });
                let proof = async {
                    invocation.session.bind_source_inspection_origin(
                        id.clone(), invocation.call_id.clone(), canonical.exact_bytes,
                        canonical.sha256.clone(), source,
                    ).await.map_err(|error| error.to_string())?;
                    if acquired.is_empty() {
                        return Ok(None);
                    }
                    invocation.session.record_tool_history_recovery(
                        id.clone(), invocation.call_id.clone(), acquired,
                    ).await;
                    completed_source_inspection_evidence(&invocation, id, canonical.exact_bytes, &canonical.sha256).await
                }.await;
                attach_source_inspection_evidence(&mut output, &mut projected, proof, &invocation);
            }
            let mut output = JsonToolOutput::with_success(output, Some(successful))
                .with_code_mode_failure_as_data();
            if let Some(evidence) = evidence {
                let mut signal = crate::tools::context::semantic_evidence_sampling_signal(evidence);
                // Recovery handles bind coverage but are not semantic evidence:
                // retaining the same bytes again can allocate a different handle.
                if let Some(artifact_id) = artifact_id {
                    signal["source_artifact_id"] = json!(artifact_id);
                }
                output = output.with_sampling_request_signal(signal);
            }
            output = output.with_model_value(projected);
            crate::tools::context::ensure_exact_model_delivery(&output, &invocation)?;
            Ok(boxed_tool_output(output))
        })
    }
}

// Reuse the recovery engine's lexical boundaries only when explicitly requested.
// Ordinary source reads do not parse JSON or build a new index. Exact source bytes
// (including whitespace and key order) remain the snapshot identity.
fn file_projection_fits(value: &serde_json::Value, invocation: &ToolInvocation) -> bool {
    let output = JsonToolOutput::with_success(value.clone(), None);
    crate::tools::context::ensure_exact_model_delivery(&output, invocation).is_ok()
}

/// Only exact source selections acquire inspection coverage. Search hydration,
/// parsed JSON, outline listings and failed batches retain their ordinary read
/// history without being promoted to a typed acquisition receipt.
fn source_inspection_delivery_eligible(packet: &serde_json::Value) -> bool {
    packet["selector_errors"].as_array().is_none_or(Vec::is_empty)
        && packet["selector_errors_omitted"].as_u64().unwrap_or(0) == 0
        && packet["results"].as_array().is_some_and(|results| !results.is_empty()
            && results.iter().all(|result| matches!(result["status"].as_str(),
                Some("ok" | "selector_too_large" | "aggregate_omitted"))))
}

pub(super) fn source_inspection_selectors(packet: &serde_json::Value) -> Vec<serde_json::Value> {
    if !source_inspection_delivery_eligible(packet) { return Vec::new(); }
    packet["results"].as_array().into_iter().flatten().filter_map(|result| {
        if result["status"] != "ok"
            || !matches!(result["selector"]["kind"].as_str(), Some("bytes" | "lines" | "section"))
            || !(result["text"].is_string() || result["data_base64"].is_string())
        {
            return None;
        }
        let start = result["canonical_range"]["start"].as_u64()?;
        let end = result["canonical_range"]["end"].as_u64()?;
        (start <= end).then(|| json!({"kind":"bytes", "start":start, "end":end}))
    }).collect()
}

pub(super) async fn completed_source_inspection_evidence(
    invocation: &ToolInvocation,
    artifact_id: &str,
    bytes: u64,
    sha256: &str,
) -> Result<Option<codex_agent_task_store::CriterionEvidenceRef>, String> {
    if invocation.cancellation_token.is_cancelled() { return Ok(None); }
    let coordinator = invocation.session.services.agent_control.task_coordinator();
    let Some(binding) = coordinator.binding_for_turn(&invocation.step_context.turn)
        .filter(|binding| binding.thread_id.as_deref() == Some(invocation.session.thread_id.to_string().as_str()))
    else { return Ok(None); };
    let Some((call_id, source)) = invocation.session.completed_source_inspection(
        artifact_id, bytes, sha256, binding.attempt_id,
    ).await else { return Ok(None); };
    let Some(store) = coordinator.store() else { return Ok(None); };
    let authorization = store.get_agent_task_authorization(binding.assignment_id).await
        .map_err(|error| error.to_string())?;
    if authorization.current_attempt.attempt_id != binding.attempt_id
        || authorization.current_attempt.state != codex_agent_task_store::AttemptState::Active
        || authorization.current_attempt.sealed_at.is_some()
    {
        return Ok(None);
    }
    let Some(path) = source.start.canonical_path() else { return Ok(None); };
    // A retained snapshot is historical. Reuse the authorized filesystem owner
    // only once full acquisition is possible, never once per partial page.
    let arguments = json!({"path":path, "environment_id":source.environment_id}).to_string();
    if !current_file_fingerprint(&invocation.step_context, &arguments, Some(path)).await
        .is_some_and(|(hash, size)| hash == sha256 && size == bytes)
    {
        return Err("Source snapshot no longer matches the authorized current file; no source_inspection receipt was recorded.".into());
    }
    if invocation.cancellation_token.is_cancelled() { return Ok(None); }
    store.record_source_inspection(source.start, call_id, sha256.to_string(), bytes).await
        .map(Some).map_err(|error| error.to_string())
}

pub(super) fn attach_source_inspection_evidence(
    output: &mut serde_json::Value,
    projected: &mut serde_json::Value,
    proof: Result<Option<codex_agent_task_store::CriterionEvidenceRef>, String>,
    invocation: &ToolInvocation,
) {
    let (field, value) = match proof {
        Ok(Some(reference)) => ("criterion_evidence", json!(reference)),
        Err(error) => ("inspection_proof_error", json!(error)),
        Ok(None) => return,
    };
    output[field] = value.clone();
    // The durable record is also discoverable through get_agent_task when a
    // tiny direct envelope cannot fit this optional presentation metadata.
    add_file_projection_metadata(projected, field, &value, invocation);
}

// Proof metadata is optional presentation, not source coverage. Never displace
// exact delivered bytes after recording that coverage merely to display it.
fn add_file_projection_metadata(
    projected: &mut serde_json::Value,
    field: &str,
    value: &serde_json::Value,
    invocation: &ToolInvocation,
) {
    let previous = projected.get(field).cloned();
    projected[field] = value.clone();
    if !file_projection_fits(projected, invocation) {
        if let Some(previous) = previous {
            projected[field] = previous;
        } else if let Some(fields) = projected.as_object_mut() {
            fields.remove(field);
        }
    }
}

fn compact_file_diagnostics(value: &mut serde_json::Value, invocation: &ToolInvocation) {
    if let Some(errors) = value.get_mut("selector_errors").and_then(serde_json::Value::as_array_mut) {
        for error in errors {
            let candidates = error["candidates"].as_array().map_or(0, Vec::len);
            error["omitted_candidates"] = json!(error["omitted_candidates"].as_u64().unwrap_or(0) + candidates as u64);
            error["candidates"] = json!([]);
            if let Some(message) = error["message"].as_str() {
                let end = message.floor_char_boundary(message.len().min(160));
                let bounded = format!("{}{}", &message[..end], if end < message.len() { "…" } else { "" });
                error["message"] = json!(bounded);
            }
        }
    }
    value["delivery_note"] = json!("Selector diagnostics were bounded for exact delivery. omitted_candidates and *_omitted count details not delivered; retry a smaller original-selector batch for complete diagnostics.");
    if let Some(fields) = value.as_object_mut() {
        fields.remove("page_selectors");
    }
    for (field, count) in [("selector_bindings", "selector_bindings_omitted"), ("selector_errors", "selector_errors_omitted")] {
        while !file_projection_fits(value, invocation) {
            let removed = value.get_mut(field).and_then(serde_json::Value::as_array_mut).and_then(Vec::pop);
            if removed.is_none() {
                break;
            }
            value[count] = json!(value[count].as_u64().unwrap_or(0) + 1);
        }
    }
}

fn file_snapshot(
    contents: String,
    selectors: Option<&[ToolOutputSelector]>,
) -> Result<CanonicalToolResult, ReadToolOutputError> {
    let mut canonical = CanonicalToolResult::text(contents);
    if selectors
        .into_iter()
        .flatten()
        .any(|selector| matches!(selector, ToolOutputSelector::JsonPointer { .. }))
    {
        // Index failure belongs to JSON selectors, not the raw snapshot or its
        // independent line/byte/search siblings. The selector owner reports it.
        canonical.json_pointers = raw_json_pointer_index(&canonical.bytes).unwrap_or_default();
    }
    Ok(canonical)
}

fn file_recovery_recipe(
    output: &serde_json::Value,
    artifact_id: &str,
) -> Option<serde_json::Value> {
    let mut pending = Vec::new();
    if let Some(continuation) = output.get("continuation").filter(|value| !value.is_null()) {
        pending.push(continuation.clone());
    }
    for result in output["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|result| result["complete"] == false)
    {
        let children = result["child_selectors"].as_array();
        let missing_context = result["selector"]["kind"] == "search"
            && result["status"] == "ok"
            && result["value"]["hydrated_ranges"]
                .as_array()
                .is_some_and(Vec::is_empty);
        if missing_context || result["status"] != "ok" {
            pending.extend(children.into_iter().flatten().cloned());
            // Direct overflow may advertise just the first bounded byte child.
            // Include its unconsumed suffix rather than silently losing it.
            if let Some(end) = result["canonical_range"]["end"].as_u64()
                && let Some(last) = children
                    .into_iter()
                    .flatten()
                    .filter(|child| child["kind"] == "bytes")
                    .filter_map(|child| child["end"].as_u64())
                    .max()
                && last < end
            {
                pending.push(json!({"kind":"bytes", "start":last, "end":end}));
            }
        }
        if let Some(continuation) = result.get("continuation").filter(|value| !value.is_null()) {
            if result["selector"]["kind"] == "search" && result["status"] == "ok" {
                // Recover only the undelivered portion of the requested page,
                // not another full page beyond the caller's max_results.
                let requested = result["selector"]["max_results"]
                    .as_u64()
                    .unwrap_or(20)
                    .min(crate::tools::command_output_artifact::ARTIFACT_SEARCH_MAX_RESULTS as u64)
                    .min(result["value"]["total_matches"].as_u64().unwrap_or(0));
                let remaining = requested
                    .saturating_sub(result["value"]["matches_returned"].as_u64().unwrap_or(0));
                if remaining > 0 {
                    let mut continuation = continuation.clone();
                    continuation["max_results"] = json!(remaining);
                    pending.push(continuation);
                }
            } else if result["selector"]["kind"] == "search" || children.is_none_or(Vec::is_empty) {
                // Exact overflows with child ranges already describe their remainder.
                pending.push(continuation.clone());
            }
        }
    }
    let mut unique = Vec::new();
    for selector in pending {
        if !unique.contains(&selector) {
            unique.push(selector);
        }
    }
    let mut selectors = Vec::new();
    let mut bytes = 0usize;
    let mut omitted = 0;
    for selector in &unique {
        let size = selector.to_string().len();
        if selectors.len() < READ_TOOL_OUTPUT_MAX_SELECTORS
            && bytes.saturating_add(size) <= 16 * 1024
        {
            bytes += size;
            selectors.push(selector.clone());
        } else {
            omitted += 1;
        }
    }
    if selectors.is_empty() {
        return None;
    }
    let mut recipe = json!({
        "tool":"read_tool_output",
        "snapshot": {
            "artifact_id":artifact_id,
            "canonical_sha256":output.get("canonical_sha256").unwrap_or(&output["source_sha256"]),
            "canonical_bytes":output["canonical_bytes"],
        },
        "selection_complete":output["complete"] == true,
        "file_complete":output["file_complete"] == true,
        "retained_artifact_complete":output["retained_artifact_complete"] == true,
        "remaining_selectors":unique,
        "arguments":{"artifact_id":artifact_id, "selectors":selectors},
    });
    if omitted > 0 {
        recipe["omitted_selectors"] = json!(omitted);
    }
    Some(recipe)
}

/// Coverage belongs to delivered bytes, not requested ranges or retained storage.
/// Shared search references need not be counted: their original text is already
/// present in an earlier result of this same response.
fn file_selection_complete(result: &ReadToolOutputResult) -> bool {
    let ranges = result.delivered_ranges();
    // An explicit empty byte selection can cover an empty file; no matches or
    // an invalid selector cannot claim coverage merely because its size is zero.
    if ranges.is_empty() {
        return false;
    }
    let mut covered = 0;
    for (start, end) in ranges {
        if start > covered {
            break;
        }
        covered = covered.max(end);
    }
    covered == result.canonical_bytes
}


/// Reacquire a bounded named identity through the same authorized file owner.
pub(crate) async fn current_file_fingerprint(
    step: &crate::session::step_context::StepContext,
    arguments: &str,
    expected_source: Option<&std::path::Path>,
) -> Option<(String, u64)> {

    let Ok(args) = parse_arguments::<ReadFileArgs>(arguments) else {
        return None;
    };
    if args.path.starts_with("skill:")
        || args.path.starts_with("context:")
        || args.encoding.is_some()
    {
        return None;
    }
    let environment = match args.environment_id.as_deref() {
        Some(id) => step
            .environments
            .turn_environments
            .iter()
            .find(|environment| environment.environment_id == id),
        None => step.environments.primary(),
    };
    let environment = environment.filter(|environment| !environment.environment.is_remote())?;
    let Ok(path) = environment.cwd().join(&args.path) else {
        return None;
    };
    let sandbox = step
        .turn
        .file_system_sandbox_context(None, environment.cwd());
    let fs = environment.environment.get_filesystem();
    let bytes = if let Some(expected) = expected_source {
        let (bytes, source) = fs.read_file_bounded_with_source(&path, MAX_FILE_BYTES, Some(&sandbox))
            .await.ok()??;
        if source.as_deref() != Some(expected) { return None; }
        bytes
    } else {
        fs.read_file_bounded(&path, MAX_FILE_BYTES, Some(&sandbox)).await.ok()??
    };
    tokio::task::spawn_blocking(move || (crate::tool_history::sha256(&bytes), bytes.len() as u64))
        .await.ok()
}

/// HTML documents outside the selected environment's working directory and the
/// workspace roots, such as attached reports. Remote paths are not compared on
/// the host.
fn html_outside_workspace(invocation: &ToolInvocation, environment_id: &str, path: &str) -> bool {
    let path = std::path::Path::new(path);
    let within = |root: &std::path::Path| {
        let mut components = path.components();
        root.components().all(|root| {
            components.next().is_some_and(|component| {
                // Windows names one file in any ASCII case.
                component == root
                    || cfg!(windows) && component.as_os_str().eq_ignore_ascii_case(root.as_os_str())
            })
        })
    };
    let Some(cwd) = invocation
        .step_context
        .environments
        .turn_environments
        .iter()
        .find(|environment| environment.environment_id == environment_id)
        .filter(|environment| !environment.environment.is_remote())
        .and_then(|environment| environment.cwd().to_abs_path().ok())
    else {
        return false;
    };
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
        })
        && !within(cwd.as_path())
        && !invocation
            .step_context
            .turn
            .effective_workspace_roots()
            .iter()
            .any(|root| within(root.as_path()))
}

enum EnvironmentFileRead {
    Snapshot((String, String, String, String)),
    Ranges(JsonToolOutput),
}

fn unsupported_text_output(
    invocation: &ToolInvocation,
    path: &codex_utils_path_uri::PathUri,
    environment_id: &str,
    file_bytes: u64,
    inspected_bytes: usize,
    revision: Option<&str>,
    classification: encoding::Classification,
) -> Result<EnvironmentFileRead, FunctionCallError> {
    let options = ranges::binary_recovery(path, environment_id, 0, file_bytes, revision);
    let mut output = json!({
        "path":path.inferred_native_path_string(), "canonical_uri":path.to_string(),
        "environment_id":environment_id, "read_mode":"unsupported_text",
        "file_bytes":file_bytes, "classification":classification, "binary_recovery":options,
        "classification_scope":if inspected_bytes as u64 == file_bytes { "complete_file" } else { "prefix" },
        "inspected_bytes":inspected_bytes,
        "complete":false, "delivered_selection_complete":false, "selection_status":"failed",
        "file_complete":false, "retained_bytes":0, "retained_artifact_complete":false, "results":[],
        "delivery_note":"Content was withheld before display. Classification covers only inspected_bytes in classification_scope, not definitive format detection. No snapshot was retained; recovery reads current original bytes through the same filesystem permissions and bounds.",
    });
    if let Some(revision) = revision {
        output["file_revision"] = json!(revision);
    }
    let result = JsonToolOutput::with_success(output.clone(), Some(false))
        .with_code_mode_failure_as_data()
        .with_model_value(output);
    crate::tools::context::ensure_exact_model_delivery(&result, invocation)?;
    Ok(EnvironmentFileRead::Ranges(result))
}

/// Reads an ordinary path through the selected environment's filesystem.
async fn read_environment_file(
    invocation: &ToolInvocation,
    args: &ReadFileArgs,
    source_inspection: &mut Option<crate::tool_history::SourceInspectionOrigin>,
    inspection_proof_error: &mut Option<String>,
    decoded_source: &mut Option<encoding::DecodedSource>,
) -> Result<EnvironmentFileRead, FunctionCallError> {
    let environment = wait_for_tool_environment(
        &invocation.step_context.environments,
        args.environment_id.as_deref(),
        &invocation.cancellation_token,
    )
    .await?
    .ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "read_file requires a selected execution environment for filesystem paths.".to_string(),
        )
    })?;
    let path = environment
        .cwd()
        .join(&args.path)
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
    let turn = &invocation.step_context.turn;
    let sandbox = turn.file_system_sandbox_context(None, environment.cwd());
    let fs = environment.environment.get_filesystem();
    crate::tools::parallel::wait_for_workspace_baseline().await;
    if args.if_file_revision.is_some() || args.encoding.is_some() {
        return ranges::read_ranges(invocation, args, fs.as_ref(), &path, &sandbox,
            &environment.environment_id).await.map(EnvironmentFileRead::Ranges);
    }
    let coordinator = invocation.session.services.agent_control.task_coordinator();
    if !environment.environment.is_remote()
        && let Some(binding) = coordinator.binding_for_turn(turn)
        && binding.thread_id.as_deref() == Some(invocation.session.thread_id.to_string().as_str())
        && let Some(store) = coordinator.store()
        && let Some(start) = store.prepare_source_inspection(
            binding.attempt_id, path.inferred_native_path_string(),
        ).await.map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
    {
        *source_inspection = Some(crate::tool_history::SourceInspectionOrigin {
            start,
            environment_id: environment.environment_id.clone(),
            canonical_uri: path.to_string(),
        });
    }
    // Execution backends validate the opened regular file and enforce the
    // byte limit during the stable read. Avoid a separate RPC/helper launch
    // on success; retain metadata only to explain failures as before.
    let result = if source_inspection.is_some() {
        fs.read_file_bounded_with_source(&path, MAX_FILE_BYTES, Some(&sandbox)).await
            .map(|result| result.map(|(bytes, opened)| {
                if !source_inspection.as_ref().is_some_and(|source|
                    source.start.canonical_path().is_some_and(|expected| opened.as_deref() == Some(expected)))
                {
                    *source_inspection = None;
                    *inspection_proof_error = Some("Native read did not bind the opened source to the assignment's canonical inspection target; no source_inspection receipt was recorded.".into());
                }
                bytes
            }))
    } else {
        fs.read_file_bounded(&path, MAX_FILE_BYTES, Some(&sandbox)).await
    };
    let contents = match result {
        Ok(Some(contents)) => contents,
        failure => {
            // Metadata and listings are later observations, not the failed read.
            // Preserve its original cause even if diagnostic I/O fails or the
            // path changes type or size before we can inspect it.
            let primary_error = failure.as_ref().err().map(|err| {
                format!(
                    "unable to read {}: {err}",
                    path.inferred_native_path_string()
                )
            });
            let diagnostic_error = |message: String| {
                FunctionCallError::RespondToModel(match &primary_error {
                    Some(primary) => {
                        format!("{primary}\nAdditional filesystem diagnostic: {message}")
                    }
                    None => message,
                })
            };
            // Nearest-existing-path hints for a path that is simply not there.
            // Host-side hints must never probe a remote filesystem or bypass
            // read restrictions. Failures other than NotFound need no guesses.
            //
            // This is shared by the failed read below and the metadata failure:
            // a read that fails with NotFound previously returned no hint at
            // all, because only the metadata branch offered suggestions. That
            // is the common case for a path the model guessed, so the whole
            // rejection carried no way forward and the next turn guessed again.
            let suggestion_note = async |kind: std::io::ErrorKind| -> Option<String> {
                if kind != std::io::ErrorKind::NotFound
                    || environment.environment.is_remote()
                    || !turn
                        .file_system_sandbox_policy()
                        .has_full_disk_read_access()
                {
                    return None;
                }
                let (Ok(missing), Ok(cwd)) = (path.to_abs_path(), environment.cwd().to_abs_path())
                else {
                    return None;
                };
                let suggestions = crate::tools::run_blocking_command_analysis(move || {
                    super::command_search::nearest_existing_paths(missing.as_path(), cwd.as_path())
                })
                .await
                .unwrap_or_default();
                (!suggestions.is_empty()).then(|| {
                    format!(
                        "\nPossible existing paths (not verified replacements): {}",
                        suggestions
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
            };
            // A read that failed because the path does not exist is answered
            // here: metadata would only repeat the same NotFound.
            if let Some(err) = failure.as_ref().err()
                && err.kind() == std::io::ErrorKind::NotFound
            {
                let note = suggestion_note(err.kind()).await.unwrap_or_default();
                return Err(FunctionCallError::RespondToModel(format!(
                    "{}{note}",
                    primary_error.clone().unwrap_or_else(|| format!(
                        "unable to read {}",
                        path.inferred_native_path_string()
                    ))
                )));
            }
            let metadata = match fs.get_metadata(&path, Some(&sandbox)).await {
                Ok(metadata) => metadata,
                Err(err) => {
                    let mut message = format!(
                        "unable to locate {}: {err}",
                        path.inferred_native_path_string()
                    );
                    if let Some(note) = suggestion_note(err.kind()).await {
                        message.push_str(&note);
                    }
                    return Err(diagnostic_error(message));
                }
            };
            if metadata.is_directory {
                let listing = fs
                    .walk(
                        &path,
                        codex_exec_server::WalkOptions {
                            max_depth: 0,
                            max_directories: 1,
                            max_entries: 32,
                            follow_directory_symlinks: false,
                            summary_only: false,
                            include_metadata: false,
                            prune_hidden_directories: true,
                            apply_gitignore: false,
                            filters: Default::default(),
                        },
                        Some(&sandbox),
                    )
                    .await
                    .map_err(|err| diagnostic_error(format!("unable to list directory: {err}")))?;
                // This diagnostic promises only immediate entries. Deliberately
                // not descending into child directories does not truncate them.
                let truncated = listing.truncated
                    && (listing.unexplored.is_empty()
                        || listing
                            .unexplored
                            .iter()
                            .any(|stop| stop.reason != "depth_limit"));
                return Err(diagnostic_error(format!(
                    "read_file requires a regular file; this path is a directory. Immediate entries (not file contents): {}. Use a file path, or list_files for a larger listing.",
                    json!({"path":path.inferred_native_path_string(), "entries":listing.entries,
                        "complete": !truncated && listing.errors.is_empty(),
                        "truncated":truncated, "errors":listing.errors})
                )));
            }
            if !metadata.is_file {
                return Err(diagnostic_error(
                    "read_file requires a regular file".to_string(),
                ));
            }
            if metadata.size > MAX_FILE_BYTES as u64 {
                // Never turn permission/I/O failures into another read route.
                if failure.is_ok() {
                    if args.selectors.is_none() {
                        let prefix = fs.read_file_range(&path, 0, 256, None, Some(&sandbox))
                            .await.map_err(|err| diagnostic_error(format!("unable to inspect file prefix: {err}")))?;
                        let classification = encoding::classify_page(&prefix.bytes, true)
                            .unwrap_or_else(|| encoding::Classification::unsupported(
                                "file exceeds the 8 MiB text snapshot limit; readable prefix does not establish whole-file encoding or format".into(), None,
                            ));
                        return unsupported_text_output(invocation, &path, &environment.environment_id,
                            prefix.file_bytes, prefix.bytes.len(), Some(&prefix.revision), classification);
                    }
                    return ranges::read_ranges(invocation, args, fs.as_ref(), &path, &sandbox,
                        &environment.environment_id).await.map(EnvironmentFileRead::Ranges);
                }
                return Err(diagnostic_error(format!("file exceeds the {MAX_FILE_MIB} MiB snapshot limit; use bounded byte selectors")));
            }
            return Err(FunctionCallError::RespondToModel(match failure {
                Err(err) => format!(
                    "unable to read {}: {err}",
                    path.inferred_native_path_string()
                ),
                Ok(_) => format!(
                    "file exceeds the {MAX_FILE_MIB} MiB read limit or changed while being read"
                ),
            }));
        }
    };
    let file_bytes = contents.len();
    let (contents, decoded) = match encoding::decode(contents, MAX_FILE_BYTES).await? {
        encoding::DecodedFile::Text(contents, decoded) => (contents, decoded),
        encoding::DecodedFile::Unsupported(classification) => {
            return unsupported_text_output(invocation, &path, &environment.environment_id,
                file_bytes as u64, file_bytes, None, classification);
        }
    };
    if decoded.is_some() && source_inspection.take().is_some() {
        *inspection_proof_error = Some("Decoded UTF-16 acquisition is not original-byte source_inspection evidence; no receipt was recorded.".into());
    }
    *decoded_source = decoded;
    Ok(EnvironmentFileRead::Snapshot((
        contents,
        path.inferred_native_path_string(),
        environment.environment_id.clone(),
        path.to_string(),
    )))
}

/// Reads a `skill:<catalog-id>` locator through the provider that discovered
/// the skill.
///
/// Skills are host-owned and are not addressable inside a turn environment, so
/// a caller that names one is told where the read actually happens rather than
/// being handed an error about a missing path.
async fn read_skill_locator(
    invocation: &ToolInvocation,
    args: &ReadFileArgs,
) -> Result<(String, String), FunctionCallError> {
    let turn = &invocation.step_context.turn;
    if let Some(environment_id) = args.environment_id.as_deref()
        && invocation
            .step_context
            .environments
            .primary()
            .is_none_or(|primary| primary.environment_id != environment_id)
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "skill locators are read through the skill's own provider, not environment `{environment_id}`; omit environment_id"
        )));
    }
    let snapshot = &turn.turn_skills.snapshot;
    if args.path == "skill:catalog" {
        // Complete JSONL records use the same immutable snapshot and selector
        // machinery as files. Never expose host paths or silently drop entries.
        let mut entries = snapshot
            .outcome()
            .skills_with_enabled()
            .filter(|(_, enabled)| *enabled)
            .map(|(skill, _)| {
                let locator = format!("skill:{}", codex_core_skills::skill_catalog_id(skill));
                (
                    skill.name.clone(),
                    locator.clone(),
                    json!({
                        "name": skill.name,
                        "description": skill.description,
                        "locator": locator,
                        "implicit_invocation": skill.allows_implicit_invocation(),
                    }),
                )
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        let contents = entries
            .into_iter()
            .map(|(_, _, entry)| format!("{entry}\n"))
            .collect::<String>();
        if contents.len() > MAX_FILE_BYTES {
            return Err(FunctionCallError::RespondToModel(
                "enabled skill catalog exceeds the read_file snapshot limit".to_string(),
            ));
        }
        return Ok((contents, args.path.clone()));
    }
    let skill = snapshot
        .resolve_catalog_locator(&args.path)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "unknown skill locator `{}`; use a locator from the skills catalog",
                args.path
            ))
        })?
        .clone();
    let (contents, path) = snapshot
        .read_skill_text_bounded(&skill, MAX_FILE_BYTES)
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "unable to read skill `{}`: {err}",
                args.path
            ))
        })?;
    Ok((contents, path.to_string_lossy().into_owned()))
}

impl CoreToolRuntime for ReadFileHandler {
    fn prepares_during_workspace_baseline(&self) -> bool {
        true
    }

    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }

    fn permits_shared_workspace_observation(&self, _payload: &ToolPayload) -> bool {
        true
    }
}

/// Text of an HTML document: block structure as lines, markup removed.
mod html_text {
    use std::borrow::Cow;
    use std::ops::Range;

    /// Elements whose contents are not document text.
    const SKIPPED: [&str; 9] = [
        "canvas", "head", "iframe", "noscript", "object", "script", "style", "svg", "template",
    ];
    /// Elements set apart by a blank line.
    const PARAGRAPHS: [&str; 19] = [
        "address",
        "article",
        "aside",
        "blockquote",
        "details",
        "dl",
        "fieldset",
        "figure",
        "footer",
        "form",
        "header",
        "hr",
        "main",
        "nav",
        "ol",
        "p",
        "section",
        "table",
        "ul",
    ];
    /// Elements on lines of their own.
    const LINES: [&str; 13] = [
        "body",
        "caption",
        "dd",
        "div",
        "dt",
        "figcaption",
        "html",
        "legend",
        "summary",
        "tbody",
        "tfoot",
        "thead",
        "tr",
    ];
    const HEADINGS: [&str; 6] = ["# ", "## ", "### ", "#### ", "##### ", "###### "];

    pub(super) fn render(html: &str) -> String {
        // ASCII lowercasing keeps byte offsets, so one index serves both.
        let lower = html.to_ascii_lowercase();
        let mut out = Output::default();
        let mut preformatted = 0usize;
        // The open link's target and where its text starts.
        let mut link = None::<(String, usize)>;
        // Skipped elements never closed later; searching again is futile.
        let mut unclosed = Vec::new();
        let mut position = 0;
        while let Some(offset) = html[position..].find('<') {
            let start = position + offset;
            out.text(&decode(&html[position..start]), preformatted > 0);
            position = start;
            if lower[start..].starts_with("<!--") {
                match lower[start + 4..].find("-->") {
                    Some(end) => position = start + 4 + end + 3,
                    None => break,
                }
                continue;
            }
            let (name, closing, self_closing, attributes) = match markup(&lower, start) {
                Markup::Literal => {
                    out.text("<", preformatted > 0);
                    position = start + 1;
                    continue;
                }
                Markup::Unterminated => break,
                Markup::Tag {
                    name,
                    closing,
                    self_closing,
                    attributes,
                    end,
                } => {
                    position = end;
                    (name, closing, self_closing, attributes)
                }
            };
            let heading = match name.as_bytes() {
                [b'h', level @ b'1'..=b'6'] => Some(HEADINGS[usize::from(level - b'1')]),
                _ => None,
            };
            match name {
                _ if !closing
                    && !self_closing
                    && SKIPPED.contains(&name)
                    && !unclosed.contains(&name) =>
                {
                    match closing_tag_end(&lower, position, name) {
                        Some(end) => position = end,
                        None => unclosed.push(name),
                    }
                }
                "br" => out.line_break(),
                "pre" => {
                    out.paragraph();
                    if closing {
                        preformatted = preformatted.saturating_sub(1);
                    } else if !self_closing {
                        preformatted += 1;
                    }
                    out.preformatted_start = !closing && !self_closing;
                }
                "li" => {
                    out.line();
                    out.prefix = if closing { "" } else { "- " };
                }
                "td" | "th" if !closing => out.cell = true,
                "a" if closing => {
                    if let Some((target, from)) = link.take()
                        && !out.text[from..].contains(&target)
                    {
                        out.space = true;
                        out.word(&format!("[{target}]"));
                    }
                }
                "a" => {
                    link = link_target(&html[attributes.clone()], &lower[attributes])
                        .map(|target| (target, out.text.len()));
                }
                _ if heading.is_some() => {
                    out.paragraph();
                    out.prefix = heading.filter(|_| !closing).unwrap_or_default();
                }
                _ if PARAGRAPHS.contains(&name) => out.paragraph(),
                _ if LINES.contains(&name) => out.line(),
                _ => {}
            }
        }
        out.text(&decode(&html[position..]), preformatted > 0);
        out.finish()
    }

    enum Markup<'a> {
        /// A `<` that starts no tag, as in `a < b`.
        Literal,
        /// A tag without its closing `>`; the rest stays text.
        Unterminated,
        Tag {
            /// Lowercase element name; empty for `<!...>` and `<?...>`.
            name: &'a str,
            closing: bool,
            self_closing: bool,
            attributes: Range<usize>,
            end: usize,
        },
    }

    fn markup(lower: &str, start: usize) -> Markup<'_> {
        let bytes = lower.as_bytes();
        let closing = bytes.get(start + 1) == Some(&b'/');
        let name_start = start + 1 + usize::from(closing);
        let declaration = !closing && matches!(bytes.get(name_start), Some(b'!' | b'?'));
        if !declaration && !bytes.get(name_start).is_some_and(u8::is_ascii_alphabetic) {
            return Markup::Literal;
        }
        let mut index = name_start + usize::from(declaration);
        while bytes
            .get(index)
            .is_some_and(|&byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':' | b'_'))
        {
            index += 1;
        }
        let name = if declaration {
            ""
        } else {
            &lower[name_start..index]
        };
        let attributes = index;
        // A quoted value may contain `>`.
        let mut quote = None;
        let mut value = false;
        while let Some(&byte) = bytes.get(index) {
            match quote {
                Some(open) if byte == open => quote = None,
                Some(_) => {}
                None => match byte {
                    b'>' => {
                        return Markup::Tag {
                            name,
                            closing,
                            self_closing: bytes[..index].ends_with(b"/"),
                            attributes: attributes..index,
                            end: index + 1,
                        };
                    }
                    b'=' => value = true,
                    b'"' | b'\'' if value => {
                        quote = Some(byte);
                        value = false;
                    }
                    byte if byte.is_ascii_whitespace() => {}
                    _ => value = false,
                },
            }
            index += 1;
        }
        Markup::Unterminated
    }

    /// The end of the first `</name>` at or after `from`.
    fn closing_tag_end(lower: &str, from: usize, name: &str) -> Option<usize> {
        let pattern = format!("</{name}");
        let mut search = from;
        while let Some(offset) = lower[search..].find(&pattern) {
            search += offset + pattern.len();
            if lower[search..]
                .starts_with(|next: char| next == '>' || next == '/' || next.is_ascii_whitespace())
            {
                return lower[search..].find('>').map(|end| search + end + 1);
            }
        }
        None
    }

    /// A link's `http(s)` target, shortened to `path:lines` for blob line links.
    /// Fragment and relative links name nothing beyond the document.
    fn link_target(attributes: &str, lower: &str) -> Option<String> {
        let href = decode(attribute(attributes, lower, "href")?.trim()).into_owned();
        let scheme = href.to_ascii_lowercase();
        if !scheme.starts_with("https://") && !scheme.starts_with("http://") {
            return None;
        }
        if let Some((_, blob)) = href.split_once("/blob/")
            && let Some((_, file)) = blob.split_once('/')
            && let Some((path, lines)) = file.split_once("#L")
        {
            let path = path.split_once('?').map_or(path, |(path, _)| path);
            let lines = lines.replacen("-L", "-", 1);
            if !path.is_empty()
                && !lines.is_empty()
                && lines
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'-')
            {
                return Some(format!("{path}:{lines}"));
            }
        }
        Some(href)
    }

    /// The raw value of attribute `name`.
    fn attribute<'a>(attributes: &'a str, lower: &str, name: &str) -> Option<&'a str> {
        let bytes = lower.as_bytes();
        let mut index = 0;
        loop {
            while bytes
                .get(index)
                .is_some_and(|&byte| byte.is_ascii_whitespace() || byte == b'/')
            {
                index += 1;
            }
            if index >= bytes.len() {
                return None;
            }
            let start = index;
            while bytes
                .get(index)
                .is_some_and(|&byte| !byte.is_ascii_whitespace() && byte != b'=' && byte != b'/')
            {
                index += 1;
            }
            let found = &lower[start..index] == name;
            while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                index += 1;
            }
            if bytes.get(index) != Some(&b'=') {
                continue;
            }
            index += 1;
            while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                index += 1;
            }
            let value = match bytes.get(index) {
                Some(&quote @ (b'"' | b'\'')) => {
                    let start = index + 1;
                    index = lower[start..]
                        .find(char::from(quote))
                        .map_or(bytes.len(), |end| start + end);
                    let value = &attributes[start..index];
                    index += 1;
                    value
                }
                _ => {
                    let start = index;
                    while bytes
                        .get(index)
                        .is_some_and(|byte| !byte.is_ascii_whitespace())
                    {
                        index += 1;
                    }
                    &attributes[start..index]
                }
            };
            if found {
                return Some(value);
            }
        }
    }

    /// Decodes character references; unknown or unterminated ones stay literal.
    fn decode(text: &str) -> Cow<'_, str> {
        if !text.contains('&') {
            return Cow::Borrowed(text);
        }
        let mut decoded = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(index) = rest.find('&') {
            decoded.push_str(&rest[..index]);
            rest = &rest[index..];
            let reference = rest.as_bytes()[1..]
                .iter()
                .take(33)
                .position(|&byte| byte == b';')
                .and_then(|end| Some((entity(&rest[1..=end])?, end + 2)));
            match reference {
                Some((character, length)) => {
                    decoded.push(character);
                    rest = &rest[length..];
                }
                None => {
                    decoded.push('&');
                    rest = &rest[1..];
                }
            }
        }
        decoded.push_str(rest);
        Cow::Owned(decoded)
    }

    fn entity(name: &str) -> Option<char> {
        if let Some(number) = name.strip_prefix('#') {
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            return char::from_u32(code).filter(|character| *character != '\0');
        }
        Some(match name {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            "nbsp" | "ensp" | "emsp" | "thinsp" => ' ',
            "ndash" => '\u{2013}',
            "mdash" => '\u{2014}',
            "lsquo" => '\u{2018}',
            "rsquo" => '\u{2019}',
            "ldquo" => '\u{201c}',
            "rdquo" => '\u{201d}',
            "bull" => '\u{2022}',
            "hellip" => '\u{2026}',
            "middot" => '\u{b7}',
            "laquo" => '\u{ab}',
            "raquo" => '\u{bb}',
            "larr" => '\u{2190}',
            "uarr" => '\u{2191}',
            "rarr" => '\u{2192}',
            "darr" => '\u{2193}',
            "harr" => '\u{2194}',
            "rArr" => '\u{21d2}',
            "times" => '\u{d7}',
            "divide" => '\u{f7}',
            "plusmn" => '\u{b1}',
            "le" => '\u{2264}',
            "ge" => '\u{2265}',
            "ne" => '\u{2260}',
            "deg" => '\u{b0}',
            "sect" => '\u{a7}',
            "para" => '\u{b6}',
            "copy" => '\u{a9}',
            "reg" => '\u{ae}',
            "trade" => '\u{2122}',
            "check" => '\u{2713}',
            _ => return None,
        })
    }

    #[derive(Default)]
    struct Output {
        text: String,
        /// Whitespace separated the last word from the next.
        space: bool,
        /// A table cell started since the last word.
        cell: bool,
        /// A heading or list marker owed to the next word.
        prefix: &'static str,
        /// The next preformatted text directly follows `<pre>`.
        preformatted_start: bool,
    }

    impl Output {
        fn at_line_start(&self) -> bool {
            self.text.is_empty() || self.text.ends_with('\n')
        }

        fn word(&mut self, word: &str) {
            if !self.at_line_start() {
                if self.cell {
                    self.text.push_str(" | ");
                } else if self.space && !self.text.ends_with(' ') {
                    self.text.push(' ');
                }
            }
            self.text.push_str(std::mem::take(&mut self.prefix));
            self.text.push_str(word);
            self.space = false;
            self.cell = false;
        }

        fn line(&mut self) {
            if !self.at_line_start() {
                self.text.push('\n');
            }
            self.space = false;
            self.cell = false;
        }

        fn paragraph(&mut self) {
            self.line();
            if !self.text.is_empty() && !self.text.ends_with("\n\n") {
                self.text.push('\n');
            }
        }

        fn line_break(&mut self) {
            if !self.text.is_empty() && !self.text.ends_with("\n\n") {
                self.text.push('\n');
            }
            self.space = false;
            self.cell = false;
        }

        fn text(&mut self, text: &str, preformatted: bool) {
            if text.is_empty() {
                return;
            }
            if preformatted {
                let normalized = text.replace("\r\n", "\n");
                let mut text = normalized.as_str();
                if std::mem::take(&mut self.preformatted_start) {
                    text = text.strip_prefix('\n').unwrap_or(text);
                }
                if !text.is_empty() {
                    self.word(text);
                }
                return;
            }
            for (index, word) in text.split(char::is_whitespace).enumerate() {
                self.space |= index > 0;
                if !word.is_empty() {
                    self.word(word);
                }
            }
        }

        fn finish(mut self) -> String {
            self.text.truncate(self.text.trim_end().len());
            self.text.push('\n');
            self.text
        }
    }
}

